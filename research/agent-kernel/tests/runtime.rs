//! Small interpreters exercise the boundary without Tokio, HTTP, or Myco.
//! The store models atomic commit/failure; it does not model fsync or process crashes.

use std::collections::BTreeMap;
use std::sync::{Arc, mpsc};
use std::time::Duration;

use agent_kernel_experiment::{data::*, run::*, trace::*};
use serde::{Deserialize, Serialize};
use serde_json::json;

fn initial() -> State {
    State::new(
        "run-1",
        Principal::Agent("agent-1".into()),
        Context::new("context-1", vec![]),
        Settings {
            model: "fake".into(),
            parameters: json!({}),
            documents: Arc::new(vec![]),
            tools: Arc::new(vec![Tool {
                name: "counter".into(),
                description: "add".into(),
                schema: json!({}),
                binding: "counter-1".into(),
            }]),
        },
    )
}

#[derive(Clone, Serialize, Deserialize)]
struct Stored {
    trace: Trace,
    receipts: BTreeMap<String, (Input, usize)>,
}

struct Runner {
    stored: Stored,
    bytes: Vec<u8>,
    fail_commit: bool,
}

impl Runner {
    fn new() -> Self {
        Self {
            stored: Stored {
                trace: Trace::new(initial()),
                receipts: BTreeMap::new(),
            },
            bytes: vec![],
            fail_commit: false,
        }
    }

    fn submit(&mut self, key: &str, input: Input) -> Result<(usize, Vec<Effect>), &'static str> {
        if let Some((previous, receipt)) = self.stored.receipts.get(key) {
            return if previous == &input {
                Ok((*receipt, vec![]))
            } else {
                Err("idempotency key reused with different input")
            };
        }
        let mut staged = self.stored.clone();
        let effects = staged.trace.push(input.clone())?;
        let cursor = staged.trace.frames.len();
        staged.receipts.insert(key.into(), (input, cursor));
        let bytes = serde_json::to_vec(&staged).unwrap();
        if self.fail_commit {
            return Err("store unavailable");
        }
        self.bytes = bytes;
        self.stored = staged;
        Ok((cursor, effects))
    }
}

#[test]
fn failed_commit_cannot_publish_effects_and_submission_retry_is_idempotent() {
    let mut r = Runner::new();
    r.fail_commit = true;
    assert!(r.submit("request-1", Input::Advance).is_err());
    assert!(r.stored.trace.frames.is_empty());
    assert!(r.bytes.is_empty());
    r.fail_commit = false;
    let (cursor, effects) = r.submit("request-1", Input::Advance).unwrap();
    assert_eq!(effects.len(), 1);
    assert_eq!(
        r.submit("request-1", Input::Advance).unwrap(),
        (cursor, vec![])
    );
    assert!(r.submit("request-1", Input::Stop(End::Cancelled)).is_err());
}

#[test]
fn reconnect_reads_committed_events_from_its_own_cursor() {
    let mut r = Runner::new();
    let (cursor, _) = r.submit("request-1", Input::Advance).unwrap();
    r.submit("request-2", Input::Stop(End::Cancelled)).unwrap();
    let suffix = &r.stored.trace.frames[cursor..];
    assert_eq!(suffix.len(), 1);
    assert_eq!(suffix[0].input, Input::Stop(End::Cancelled));
    let second_viewer = &r.stored.trace.frames[0..];
    assert_eq!(second_viewer.len(), 2);
}

#[test]
fn recovery_exposes_uncertain_commands_without_reexecuting_them() {
    let mut r = Runner::new();
    let (_, effects) = r.submit("begin", Input::Advance).unwrap();
    let Effect::Generate { id, .. } = &effects[0] else {
        panic!()
    };
    let (_, effects) = r
        .submit(
            "model",
            Input::Generated {
                id: id.clone(),
                result: Ok(Completion {
                    finish: Finish::Complete,
                    reply: Reply {
                        content: vec![],
                        calls: vec![Call {
                            tool: "counter".into(),
                            arguments: json!(1),
                        }],
                    },
                    tokens: None,
                }),
            },
        )
        .unwrap();
    let Effect::Call { id, .. } = &effects[0] else {
        panic!()
    };
    // External mutation happens, then the process loses its uncommitted reply.
    let mut world_counter = 0;
    world_counter += 1;
    let restored: Stored = serde_json::from_slice(&r.bytes).unwrap();
    let state = restored.trace.replay().unwrap();
    assert_eq!(state.pending(), vec![id.clone()]);
    let mut recovered = Runner {
        stored: restored,
        bytes: r.bytes,
        fail_commit: false,
    };
    recovered
        .submit("interrupt", Input::Stop(End::Interrupted))
        .unwrap();
    let (_, effects) = recovered
        .submit(
            "reconcile",
            Input::Observed {
                id: id.clone(),
                outcome: Outcome::Unknown("receipt lost; do not replay mutation".into()),
            },
        )
        .unwrap();
    assert!(effects.is_empty());
    assert_eq!(world_counter, 1);
    assert_eq!(
        recovered.stored.trace.replay().unwrap().phase,
        Phase::Finished(End::Interrupted)
    );
}

#[test]
fn an_actor_stays_responsive_while_model_work_and_viewers_are_detached() {
    type Request = (String, Input, mpsc::Sender<(usize, Vec<Effect>)>);
    let (send, receive) = mpsc::channel::<Request>();
    let actor = std::thread::spawn(move || {
        let mut r = Runner::new();
        while let Ok((key, input, reply)) = receive.recv() {
            let result = r.submit(&key, input).unwrap();
            // A departed viewer cannot terminate the run owner.
            let _ = reply.send(result);
        }
        r.stored.trace
    });
    let (reply, result) = mpsc::channel();
    send.send(("start".into(), Input::Advance, reply)).unwrap();
    let (_, effects) = result.recv_timeout(Duration::from_secs(2)).unwrap();
    let Effect::Generate { id, .. } = &effects[0] else {
        panic!()
    };
    drop(result);
    // A worker can still be waiting for this generation; mailbox handling continues.
    let (reply, result) = mpsc::channel();
    send.send(("cancel".into(), Input::Stop(End::Cancelled), reply))
        .unwrap();
    assert_eq!(
        result.recv_timeout(Duration::from_secs(2)).unwrap().1,
        vec![Effect::Cancel(id.clone())]
    );
    let (reply, result) = mpsc::channel();
    send.send((
        "late".into(),
        Input::Generated {
            id: id.clone(),
            result: Ok(Completion {
                finish: Finish::Complete,
                reply: Reply {
                    content: vec![],
                    calls: vec![Call {
                        tool: "counter".into(),
                        arguments: json!(1),
                    }],
                },
                tokens: None,
            }),
        },
        reply,
    ))
    .unwrap();
    assert!(
        result
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
            .1
            .is_empty()
    );
    drop(send);
    assert_eq!(
        actor.join().unwrap().replay().unwrap().phase,
        Phase::Finished(End::Cancelled)
    );
}

#[test]
fn actor_and_direct_interpreters_produce_identical_semantic_records() {
    let inputs = vec![Input::Advance, Input::Stop(End::Budget)];
    let mut direct = Runner::new();
    for (index, input) in inputs.iter().enumerate() {
        direct.submit(&index.to_string(), input.clone()).unwrap();
    }
    let (send, receive) = mpsc::channel();
    let actor = std::thread::spawn(move || {
        let mut r = Runner::new();
        for (index, input) in receive {
            r.submit(&format!("{index}"), input).unwrap();
        }
        r.stored.trace
    });
    for (index, input) in inputs.into_iter().enumerate() {
        send.send((index, input)).unwrap();
    }
    drop(send);
    assert_eq!(actor.join().unwrap(), direct.stored.trace);
}

#[test]
fn dataset_records_are_consumable_without_replaying_a_controller() {
    // An arbitrary existing agent can supply this record at its model-call boundary.
    let record = GenerationRecord {
        generation: OperationId {
            run: "foreign-run".into(),
            sequence: 1,
        },
        input: ModelInput {
            context: initial().context,
            settings: initial().settings,
        },
        result: Some(Ok(Completion {
            finish: Finish::Complete,
            reply: Reply {
                content: vec![Content::Text("answer".into())],
                calls: vec![],
            },
            tokens: None,
        })),
        applied: true,
    };
    let bytes = serde_json::to_vec(&record).unwrap();
    let decoded: GenerationRecord = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        decoded.candidate().unwrap().output.reply.content,
        vec![Content::Text("answer".into())]
    );
    assert!(decoded.candidate().unwrap().token_evidence().is_err());
}
