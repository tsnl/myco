//! The framework, proven with the smallest possible kind: a counter whose
//! `incr` is driver-gated and whose reads are free. Everything here is the
//! contract every real kind inherits — driver policy, sys verbs, refusal
//! semantics, events — pinned once, at the framework level.

use super::*;
use myco_runtime::Ctx;

struct CounterSpec;

static COUNTER_SPEC: KindSpec = KindSpec {
    kind: "counter",
    doc: "a number that goes up",
    verbs: &[
        VerbSpec::driven("incr", "add {by} (default 1)"),
        VerbSpec::read("get", "the current value"),
        VerbSpec::read("text", "the value as plain text"),
    ],
    primary_render: "get",
    recommended_context: "text",
};

struct Counter(i64);

#[async_trait::async_trait]
impl Instance for Counter {
    async fn verb(&mut self, verb: &str, args: Value, cx: &mut Ctx) -> Result<Value, VerbError> {
        match verb {
            "incr" => {
                self.0 += args.get("by").and_then(Value::as_i64).unwrap_or(1);
                cx.bump();
                Ok(json!(self.0))
            }
            "get" => Ok(json!(self.0)),
            "text" => Ok(json!(self.0.to_string())),
            "boom" => panic!("kind bug"),
            other => Err(VerbError::UnknownVerb { verb: other.into() }),
        }
    }
}

impl Kind for CounterSpec {
    fn spec(&self) -> &'static KindSpec {
        &COUNTER_SPEC
    }

    fn create(
        &self,
        args: Value,
        _signals: myco_runtime::Signals,
    ) -> Result<Box<dyn Instance>, VerbError> {
        let start = args.get("start").and_then(Value::as_i64).unwrap_or(0);
        Ok(Box::new(Counter(start)))
    }
}

fn pool() -> Pool {
    let pool = Pool::new();
    pool.register(Arc::new(CounterSpec));
    pool
}

fn ada() -> Principal {
    Principal::Human("ada".into())
}

fn grace() -> Principal {
    Principal::Human("grace".into())
}

fn agent() -> Principal {
    Principal::Agent("chat-1".into())
}

#[tokio::test]
async fn verbs_dispatch_and_reads_are_free_for_everyone() {
    let pool = pool();
    let info = pool
        .create(&ada(), "counter", "proj", "", json!({"start": 40}))
        .unwrap();
    assert_eq!(info.kind, "counter");
    assert_eq!(info.driver, Some(ada()), "the creator starts as driver");

    let n = pool.call(&ada(), &info.id, "incr", json!({"by": 2})).await;
    assert_eq!(n.unwrap(), json!(42));

    // Reads are never driver-gated: the agent and another human both read.
    for p in [agent(), grace()] {
        assert_eq!(
            pool.call(&p, &info.id, "get", Value::Null).await.unwrap(),
            json!(42)
        );
    }
    // The plain-text read — the acme escape hatch — says the same thing.
    assert_eq!(
        pool.call(&agent(), &info.id, "text", Value::Null)
            .await
            .unwrap(),
        json!("42")
    );
}

#[tokio::test]
async fn driver_gated_verbs_refuse_non_drivers_by_name() {
    let pool = pool();
    let info = pool
        .create(&ada(), "counter", "proj", "", Value::Null)
        .unwrap();

    let refused = pool.call(&agent(), &info.id, "incr", Value::Null).await;
    assert_eq!(
        refused,
        Err(VerbError::NotDriver {
            held_by: Some(ada())
        }),
        "refusal names the holder — nobody blocks, nobody waits"
    );
}

/// The whole policy matrix: humans outrank agents, nobody takes from a
/// human, release returns the seat to the default driver (the creator) —
/// or empties it when the creator releases.
#[tokio::test]
async fn the_driver_seat_changes_hands_by_policy() {
    let pool = pool();
    let info = pool
        .create(&ada(), "counter", "proj", "", Value::Null)
        .unwrap();
    let take = |p: Principal| {
        let pool = pool.clone();
        let id = info.id.clone();
        async move { pool.call(&p, &id, "sys.take", Value::Null).await }
    };
    let release = |p: Principal| {
        let pool = pool.clone();
        let id = info.id.clone();
        async move { pool.call(&p, &id, "sys.release", Value::Null).await }
    };

    // An agent may not take from a human; another human may not either.
    assert!(matches!(
        take(agent()).await,
        Err(VerbError::NotDriver { .. })
    ));
    assert!(matches!(
        take(grace()).await,
        Err(VerbError::NotDriver { .. })
    ));

    // The creator releases: the seat empties (they are the default).
    release(ada()).await.unwrap();
    assert_eq!(pool.info(&info.id).unwrap().driver, None);

    // An empty seat is anyone's; the agent takes it and drives.
    take(agent()).await.unwrap();
    pool.call(&agent(), &info.id, "incr", Value::Null)
        .await
        .unwrap();

    // A human takes from an agent without asking.
    take(grace()).await.unwrap();
    assert_eq!(pool.info(&info.id).unwrap().driver, Some(grace()));

    // Grace releases: the seat returns to the default driver, ada.
    release(grace()).await.unwrap();
    assert_eq!(pool.info(&info.id).unwrap().driver, Some(ada()));

    // Only the holder can release.
    assert!(matches!(
        release(agent()).await,
        Err(VerbError::NotDriver { .. })
    ));
}

#[tokio::test]
async fn sys_verbs_expose_spec_meta_and_the_verb_log() {
    let pool = pool();
    let info = pool
        .create(&ada(), "counter", "proj", "the count", Value::Null)
        .unwrap();

    let spec = pool
        .call(&agent(), &info.id, "sys.spec", Value::Null)
        .await
        .unwrap();
    assert_eq!(spec["kind"], "counter");
    assert_eq!(spec["recommended_context"], "text");
    assert!(
        spec["verbs"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v["name"] == "incr" && v["requires_driver"] == true)
    );

    let meta = pool
        .call(&agent(), &info.id, "sys.meta", Value::Null)
        .await
        .unwrap();
    assert_eq!(meta["title"], "the count");
    assert_eq!(meta["project"], "proj");

    // The log records refusals too — that is what makes it a debugger.
    let _ = pool.call(&agent(), &info.id, "incr", Value::Null).await;
    let log = pool
        .call(&ada(), &info.id, "sys.log", json!({"limit": 8}))
        .await
        .unwrap();
    let entries = log.as_array().unwrap();
    assert!(
        entries
            .iter()
            .any(|e| e["verb"] == "incr" && e["ok"] == false && e["error"] == "not_driver"),
        "{entries:?}"
    );

    pool.call(&ada(), &info.id, "sys.rename", json!({"title": "renamed"}))
        .await
        .unwrap();
    assert_eq!(pool.info(&info.id).unwrap().title, "renamed");
}

#[tokio::test]
async fn unknown_things_fail_by_name() {
    let pool = pool();
    let info = pool
        .create(&ada(), "counter", "proj", "", Value::Null)
        .unwrap();

    assert!(matches!(
        pool.create(&ada(), "nope", "proj", "", Value::Null),
        Err(VerbError::UnknownKind { .. })
    ));
    assert!(matches!(
        pool.call(&ada(), "nope", "get", Value::Null).await,
        Err(VerbError::UnknownInstance { .. })
    ));
    // A verb outside the spec is refused *before* reaching the kind — the
    // spec is the contract, not the implementation's match arms.
    assert!(matches!(
        pool.call(&ada(), &info.id, "boom", Value::Null).await,
        Err(VerbError::UnknownVerb { .. })
    ));
    assert!(matches!(
        pool.call(&ada(), &info.id, "sys.nope", Value::Null).await,
        Err(VerbError::UnknownVerb { .. })
    ));
}

#[tokio::test]
async fn watermarks_wake_watchers_and_removal_forgets() {
    let pool = pool();
    let info = pool
        .create(&ada(), "counter", "proj", "", Value::Null)
        .unwrap();

    let seen = pool.watermark(&info.id).unwrap();
    let watcher = {
        let pool = pool.clone();
        let id = info.id.clone();
        tokio::spawn(async move { pool.changed(&id, seen).await })
    };
    pool.call(&ada(), &info.id, "incr", Value::Null)
        .await
        .unwrap();
    assert!(watcher.await.unwrap().unwrap() > seen);

    // An agent that neither created nor drives may not remove.
    assert!(matches!(
        pool.remove(&agent(), &info.id),
        Err(VerbError::Denied { .. })
    ));
    pool.remove(&ada(), &info.id).unwrap();
    assert!(matches!(
        pool.call(&ada(), &info.id, "get", Value::Null).await,
        Err(VerbError::UnknownInstance { .. })
    ));
    assert!(pool.list(None).is_empty());
}

#[tokio::test]
async fn the_global_feed_carries_lifecycle_driver_and_kind_events() {
    let pool = pool();
    let mut feed = pool.events();
    let info = pool
        .create(&ada(), "counter", "proj", "", Value::Null)
        .unwrap();
    pool.call(&ada(), &info.id, "sys.release", Value::Null)
        .await
        .unwrap();
    pool.remove(&ada(), &info.id).unwrap();

    let mut names = Vec::new();
    while let Ok((id, event)) = feed.try_recv() {
        assert_eq!(id, info.id);
        names.push(event.name);
    }
    assert_eq!(names, ["created", "driver", "removed"]);
}

#[tokio::test]
async fn listing_scopes_by_project() {
    let pool = pool();
    pool.create(&ada(), "counter", "alpha", "", Value::Null)
        .unwrap();
    pool.create(&ada(), "counter", "alpha", "", Value::Null)
        .unwrap();
    pool.create(&ada(), "counter", "beta", "", Value::Null)
        .unwrap();

    assert_eq!(pool.list(None).len(), 3);
    assert_eq!(pool.list(Some("alpha")).len(), 2);
    assert_eq!(pool.list(Some("beta")).len(), 1);
    assert_eq!(pool.list(Some("gamma")).len(), 0);
}

/// A panicking kind takes down its own instance and nothing else; the
/// corpse answers `Gone`, and its `sys.meta` (framework state, not cell
/// state) still reports `crashed` for the tree UI.
#[tokio::test]
async fn a_kind_bug_crashes_one_instance_only() {
    struct Buggy;
    static BUGGY_SPEC: KindSpec = KindSpec {
        kind: "buggy",
        doc: "panics on demand",
        verbs: &[
            VerbSpec::write("boom", "panic"),
            VerbSpec::read("get", "value"),
            VerbSpec::read("text", "value as text"),
        ],
        primary_render: "get",
        recommended_context: "text",
    };
    struct BuggyInstance;
    #[async_trait::async_trait]
    impl Instance for BuggyInstance {
        async fn verb(&mut self, verb: &str, _: Value, _: &mut Ctx) -> Result<Value, VerbError> {
            match verb {
                "boom" => panic!("kind bug"),
                _ => Ok(json!(1)),
            }
        }
    }
    impl Kind for Buggy {
        fn spec(&self) -> &'static KindSpec {
            &BUGGY_SPEC
        }
        fn create(
            &self,
            _: Value,
            _: myco_runtime::Signals,
        ) -> Result<Box<dyn Instance>, VerbError> {
            Ok(Box::new(BuggyInstance))
        }
    }

    let pool = pool();
    pool.register(Arc::new(Buggy));
    let victim = pool
        .create(&ada(), "buggy", "proj", "", Value::Null)
        .unwrap();
    let bystander = pool
        .create(&ada(), "counter", "proj", "", Value::Null)
        .unwrap();

    assert_eq!(
        pool.call(&ada(), &victim.id, "boom", Value::Null).await,
        Err(VerbError::Gone)
    );
    assert_eq!(
        pool.call(&ada(), &victim.id, "get", Value::Null).await,
        Err(VerbError::Gone)
    );
    // wait for the monitor to mark the corpse
    let mut tries = 0;
    while !pool.info(&victim.id).unwrap().crashed && tries < 200 {
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        tries += 1;
    }
    assert!(pool.info(&victim.id).unwrap().crashed);

    // The bystander never noticed.
    assert_eq!(
        pool.call(&ada(), &bystander.id, "get", Value::Null)
            .await
            .unwrap(),
        json!(0)
    );
}
