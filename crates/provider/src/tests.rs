//! The serve loop, driven from the pool side over an in-process duplex —
//! the same frames a real server would speak over ssh'd stdio.

use super::*;
use myco_instance::{CreateCtx, Instance, Kind, KindSpec, VerbError, VerbSpec};
use myco_runtime::Signals;
use myco_wire::{InstanceInfo, KindOffer, ToPool, ToProvider};
use serde_json::json;

static COUNTER: KindSpec = KindSpec {
    kind: "counter",
    version: 1,
    doc: "a number that goes up",
    verbs: &[
        VerbSpec::read("about", "the number"),
        VerbSpec::write("bump", "add one"),
        VerbSpec::write("nap", "hold the mailbox briefly"),
    ],
    primary_render: "about",
    recommended_context: "about",
};

struct CounterKind;

impl Kind for CounterKind {
    fn spec(&self) -> &'static KindSpec {
        &COUNTER
    }
    fn create(
        &self,
        _ctx: &CreateCtx,
        _args: Value,
        _signals: Signals,
    ) -> Result<Box<dyn Instance>, VerbError> {
        Ok(Box::new(Counter(0)))
    }
}

struct Counter(u64);

#[async_trait::async_trait]
impl Instance for Counter {
    async fn verb(
        &mut self,
        _caller: &Principal,
        verb: &str,
        _args: Value,
        signals: &Signals,
    ) -> Result<Value, VerbError> {
        match verb {
            "about" => Ok(json!({"n": self.0})),
            "bump" => {
                self.0 += 1;
                signals.bump();
                Ok(Value::Null)
            }
            "nap" => {
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                Ok(Value::Null)
            }
            other => Err(VerbError::UnknownVerb { verb: other.into() }),
        }
    }
}

fn pool_with_counter() -> Pool {
    let pool = Pool::new();
    pool.register(std::sync::Arc::new(CounterKind));
    pool
}

fn ada() -> Principal {
    Principal::Human("ada".into())
}

/// The pool side of the stream, as a test double: sends `ToProvider`,
/// reads `ToPool`, with a skip-until helper because marks, events, rows,
/// and replies interleave by design.
struct PoolSide {
    lines: tokio::io::Lines<BufReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>>,
    writer: tokio::io::WriteHalf<tokio::io::DuplexStream>,
}

impl PoolSide {
    fn start(pool: Pool) -> (Self, tokio::task::JoinHandle<std::io::Result<()>>) {
        let (near, far) = tokio::io::duplex(1 << 16);
        let (r, w) = tokio::io::split(far);
        let task = tokio::spawn(serve(pool, "testbox", r, w));
        let (r, writer) = tokio::io::split(near);
        (
            Self {
                lines: BufReader::new(r).lines(),
                writer,
            },
            task,
        )
    }

    async fn send(&mut self, frame: &ToProvider) {
        self.writer
            .write_all(wire::encode(frame).as_bytes())
            .await
            .expect("stream open");
    }

    async fn recv(&mut self) -> ToPool {
        let line = tokio::time::timeout(std::time::Duration::from_secs(5), self.lines.next_line())
            .await
            .expect("a frame within five seconds")
            .expect("stream readable")
            .expect("stream open");
        wire::decode(&line).expect("a wire frame")
    }

    async fn recv_until(&mut self, pred: impl Fn(&ToPool) -> bool) -> ToPool {
        loop {
            let frame = self.recv().await;
            if pred(&frame) {
                return frame;
            }
        }
    }

    /// Read the provider's hello and answer it, returning the offering.
    async fn handshake(&mut self) -> (Vec<KindOffer>, Vec<InstanceInfo>) {
        let frame = self.recv().await;
        let ToPool::Hello {
            protocol,
            kinds,
            rows,
            name,
        } = frame
        else {
            panic!("the provider speaks hello first, got {frame:?}");
        };
        assert_eq!(protocol, wire::PROTOCOL);
        assert_eq!(name, "testbox");
        self.send(&ToProvider::Hello {
            protocol: wire::PROTOCOL,
        })
        .await;
        (kinds, rows)
    }

    async fn reply(&mut self, seq: u64) -> Result<Value, VerbError> {
        let frame = self
            .recv_until(|f| matches!(f, ToPool::Reply { seq: s, .. } if *s == seq))
            .await;
        let ToPool::Reply { outcome, .. } = frame else {
            unreachable!()
        };
        outcome.into_result()
    }
}

#[tokio::test]
async fn hello_carries_offers_and_live_rows() {
    let pool = pool_with_counter();
    let before = pool
        .create(&ada(), "counter", "default", "pre-existing", json!({}))
        .expect("creates");

    let (mut side, _task) = PoolSide::start(pool);
    let (kinds, rows) = side.handshake().await;

    assert_eq!(kinds.len(), 1);
    assert_eq!(kinds[0].kind, "counter");
    assert_eq!(kinds[0].version, 1);
    assert_eq!(kinds[0].spec["doc"], "a number that goes up");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].id, before.id);
    assert_eq!(rows[0].title, "pre-existing");
}

#[tokio::test]
async fn create_call_and_marks_flow_end_to_end() {
    let (mut side, _task) = PoolSide::start(pool_with_counter());
    side.handshake().await;

    side.send(&ToProvider::Create {
        seq: 1,
        kind: "counter".into(),
        project: "default".into(),
        title: "c".into(),
        creator: ada(),
        args: json!({}),
    })
    .await;
    let info: InstanceInfo =
        serde_json::from_value(side.reply(1).await.expect("create succeeds")).expect("a row");
    assert_eq!(info.kind, "counter");
    assert_eq!(info.creator, ada());

    // The birth also travels as an event and a row upsert.
    side.recv_until(|f| matches!(f, ToPool::Event { name, .. } if name == "created"))
        .await;

    side.send(&ToProvider::Call {
        seq: 2,
        id: info.id.clone(),
        caller: ada(),
        verb: "bump".into(),
        args: Value::Null,
    })
    .await;
    // The reply and the coalesced mark race by design — the bump wakes
    // the watch task while the call task is still packaging the reply —
    // so collect until both have landed, in whichever order.
    let mut replied = false;
    let mut marked = None;
    while !(replied && marked.is_some()) {
        match side.recv().await {
            ToPool::Reply { seq: 2, outcome } => {
                assert_eq!(outcome.into_result(), Ok(Value::Null));
                replied = true;
            }
            ToPool::Mark { id, watermark } if id == info.id => marked = Some(watermark),
            _ => {}
        }
    }
    assert!(marked.expect("a mark landed") >= 1);

    side.send(&ToProvider::Call {
        seq: 3,
        id: info.id.clone(),
        caller: ada(),
        verb: "about".into(),
        args: Value::Null,
    })
    .await;
    assert_eq!(side.reply(3).await, Ok(json!({"n": 1})));
}

#[tokio::test]
async fn sys_verbs_forward_and_removal_travels_as_gone() {
    let pool = pool_with_counter();
    let info = pool
        .create(&ada(), "counter", "default", "doomed", json!({}))
        .expect("creates");
    let (mut side, _task) = PoolSide::start(pool);
    side.handshake().await;

    side.send(&ToProvider::Call {
        seq: 1,
        id: info.id.clone(),
        caller: ada(),
        verb: "sys.meta".into(),
        args: Value::Null,
    })
    .await;
    let meta: InstanceInfo =
        serde_json::from_value(side.reply(1).await.expect("meta answers")).expect("a row");
    assert_eq!(meta.title, "doomed");
    assert_eq!(meta.driver, Some(ada()), "the seat lives over here, forwarded");

    side.send(&ToProvider::Call {
        seq: 2,
        id: info.id.clone(),
        caller: ada(),
        verb: "sys.remove".into(),
        args: Value::Null,
    })
    .await;
    assert_eq!(side.reply(2).await, Ok(Value::Null));
    side.recv_until(|f| matches!(f, ToPool::Gone { id } if *id == info.id))
        .await;
}

/// Replies interleave: a verb holding one instance's mailbox does not
/// convoy a reply from another instance.
#[tokio::test]
async fn a_slow_verb_does_not_convoy_other_replies() {
    let pool = pool_with_counter();
    let slow = pool
        .create(&ada(), "counter", "default", "slow", json!({}))
        .expect("creates");
    let quick = pool
        .create(&ada(), "counter", "default", "quick", json!({}))
        .expect("creates");
    let (mut side, _task) = PoolSide::start(pool);
    side.handshake().await;

    side.send(&ToProvider::Call {
        seq: 1,
        id: slow.id,
        caller: ada(),
        verb: "nap".into(),
        args: Value::Null,
    })
    .await;
    side.send(&ToProvider::Call {
        seq: 2,
        id: quick.id,
        caller: ada(),
        verb: "about".into(),
        args: Value::Null,
    })
    .await;

    let first = side
        .recv_until(|f| matches!(f, ToPool::Reply { .. }))
        .await;
    let ToPool::Reply { seq, .. } = first else {
        unreachable!()
    };
    assert_eq!(seq, 2, "the quick reply lands while the nap holds seq 1");
    assert_eq!(side.reply(1).await, Ok(Value::Null));
}

#[tokio::test]
async fn a_wrong_protocol_ends_the_stream() {
    let (mut side, task) = PoolSide::start(pool_with_counter());
    let ToPool::Hello { .. } = side.recv().await else {
        panic!("hello first");
    };
    side.send(&ToProvider::Hello { protocol: 999 }).await;

    let outcome = task.await.expect("serve task finishes");
    assert!(outcome.is_err(), "unequal protocol is fatal, not negotiated");
}
