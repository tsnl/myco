//! The framework, proven with the smallest possible kind: a counter whose
//! `incr` is driver-gated, whose `reset`/`peek` are owner-scoped, and whose
//! plain reads are free. Everything here is the contract every real kind
//! inherits — authority (driver and owner), refusal semantics, the sys
//! verbs, events, and the two attack-derived guarantees: authority binds at
//! apply time, and re-entrant self-calls refuse instead of deadlocking.

use super::*;
use myco_runtime::Signals;

struct CounterSpec;

static COUNTER_SPEC: KindSpec = KindSpec {
    kind: "counter",
    version: 1,
    doc: "a number that goes up",
    verbs: &[
        VerbSpec::driven("incr", "add {by} (default 1)"),
        VerbSpec::driven("nap", "hold the mailbox for {ms} before incrementing"),
        VerbSpec::owned("reset", "set the value to 0 (creator only)"),
        VerbSpec::owned_read("peek", "the value, privately (creator only)"),
        VerbSpec::read("get", "the current value"),
        VerbSpec::read("text", "the value as plain text"),
    ],
    primary_render: "get",
    recommended_context: "text",
};

struct Counter(i64);

#[async_trait::async_trait]
impl Instance for Counter {
    async fn verb(
        &mut self,
        _caller: &Principal,
        verb: &str,
        args: Value,
        signals: &Signals,
    ) -> Result<Value, VerbError> {
        match verb {
            "incr" => {
                self.0 += args.get("by").and_then(Value::as_i64).unwrap_or(1);
                signals.bump();
                Ok(json!(self.0))
            }
            "nap" => {
                let ms = args.get("ms").and_then(Value::as_u64).unwrap_or(100);
                tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
                self.0 += 1;
                signals.bump();
                Ok(json!(self.0))
            }
            "reset" => {
                self.0 = 0;
                signals.bump();
                Ok(json!(0))
            }
            "peek" | "get" => Ok(json!(self.0)),
            "text" => Ok(json!(self.0.to_string())),
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
        _id: &str,
        args: Value,
        _signals: Signals,
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

async fn meta(pool: &Pool, id: &str) -> InstanceInfo {
    let v = pool
        .call(
            &Principal::System("test".into()),
            id,
            "sys.meta",
            Value::Null,
        )
        .await
        .expect("sys.meta");
    serde_json::from_value(v).expect("info deserializes")
}

#[tokio::test]
async fn verbs_dispatch_and_plain_reads_are_free_for_everyone() {
    let pool = pool();
    let info = pool
        .create(&ada(), "counter", "proj", "", json!({"start": 40}))
        .unwrap();
    assert_eq!(info.kind, "counter");
    assert_eq!(info.driver, Some(ada()), "the creator starts as driver");

    let n = pool.call(&ada(), &info.id, "incr", json!({"by": 2})).await;
    assert_eq!(n.unwrap(), json!(42));

    // Plain reads are never gated: the agent and another human both read.
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

/// The concurrency reviewer's emergency-stop scenario: a verb the old
/// driver queued before losing the seat must be refused when it lands, not
/// applied. Authority binds at apply time.
#[tokio::test]
async fn taking_the_seat_fences_the_old_drivers_queued_verbs() {
    let pool = pool();
    // The agent creates and drives.
    let info = pool
        .create(&agent(), "counter", "proj", "", Value::Null)
        .unwrap();

    // In-flight: the agent's `nap` holds the mailbox.
    let napping = {
        let pool = pool.clone();
        let id = info.id.clone();
        tokio::spawn(async move { pool.call(&agent(), &id, "nap", json!({"ms": 250})).await })
    };
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // Queued behind it: an `incr` that passes the enqueue check (the agent
    // still drives at this instant).
    let queued = {
        let pool = pool.clone();
        let id = info.id.clone();
        tokio::spawn(async move { pool.call(&agent(), &id, "incr", Value::Null).await })
    };
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // The human seizes the seat while both are outstanding.
    pool.call(&ada(), &info.id, "sys.take", Value::Null)
        .await
        .unwrap();

    // The in-flight verb completes (a take is a fence, not an interrupt)…
    assert_eq!(napping.await.unwrap().unwrap(), json!(1));
    // …but the queued one is refused at apply time: the emergency stop
    // actually stopped.
    assert_eq!(
        queued.await.unwrap(),
        Err(VerbError::NotDriver {
            held_by: Some(ada())
        })
    );
    assert_eq!(
        pool.call(&ada(), &info.id, "get", Value::Null)
            .await
            .unwrap(),
        json!(1),
        "the fenced incr never landed"
    );
}

/// A verb handler calling back into its own instance would deadlock the
/// mailbox; the dispatch refuses it instead.
#[tokio::test]
async fn a_self_call_is_refused_not_deadlocked() {
    struct Selfie {
        pool: Pool,
    }
    static SELFIE_SPEC: KindSpec = KindSpec {
        kind: "selfie",
        version: 1,
        doc: "calls itself, to prove it cannot",
        verbs: &[
            VerbSpec::write("recurse", "call {id}'s get from inside a handler"),
            VerbSpec::read("get", "unit"),
        ],
        primary_render: "get",
        recommended_context: "get",
    };
    struct SelfieInstance {
        pool: Pool,
    }
    #[async_trait::async_trait]
    impl Instance for SelfieInstance {
        async fn verb(
            &mut self,
            _caller: &Principal,
            verb: &str,
            args: Value,
            _: &Signals,
        ) -> Result<Value, VerbError> {
            match verb {
                "recurse" => {
                    let id = args["id"].as_str().unwrap_or_default();
                    // The inner call is refused; surface its error verbatim.
                    self.pool
                        .call(&Principal::System("selfie".into()), id, "get", Value::Null)
                        .await
                }
                _ => Ok(Value::Null),
            }
        }
    }
    impl Kind for Selfie {
        fn spec(&self) -> &'static KindSpec {
            &SELFIE_SPEC
        }
        fn create(&self, _: &str, _: Value, _: Signals) -> Result<Box<dyn Instance>, VerbError> {
            Ok(Box::new(SelfieInstance {
                pool: self.pool.clone(),
            }))
        }
    }

    let pool = pool();
    pool.register(Arc::new(Selfie { pool: pool.clone() }));
    let info = pool
        .create(&ada(), "selfie", "proj", "", Value::Null)
        .unwrap();

    let result = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        pool.call(&ada(), &info.id, "recurse", json!({"id": info.id})),
    )
    .await
    .expect("refused, not deadlocked");
    assert!(
        matches!(result, Err(VerbError::Denied { ref why }) if why.contains("re-entrant")),
        "{result:?}"
    );
}

/// The whole seat policy: humans outrank agents, nobody else takes an
/// occupied seat, release returns it to the creator — or empties it when
/// the creator releases.
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

    // The creator releases: the seat empties.
    release(ada()).await.unwrap();
    assert_eq!(meta(&pool, &info.id).await.driver, None);

    // An empty seat is anyone's; the agent takes it and drives.
    take(agent()).await.unwrap();
    pool.call(&agent(), &info.id, "incr", Value::Null)
        .await
        .unwrap();

    // A human takes from an agent without asking.
    take(grace()).await.unwrap();
    assert_eq!(meta(&pool, &info.id).await.driver, Some(grace()));

    // Grace releases: the seat returns home, to the creator.
    release(grace()).await.unwrap();
    assert_eq!(meta(&pool, &info.id).await.driver, Some(ada()));

    // Only the holder can release.
    assert!(matches!(
        release(agent()).await,
        Err(VerbError::NotDriver { .. })
    ));
}

/// Owner scoping is the second authority axis, orthogonal to the seat: the
/// creator keeps owned verbs even while someone else drives, and the driver
/// does not gain them.
#[tokio::test]
async fn owned_verbs_belong_to_the_creator_regardless_of_the_seat() {
    let pool = pool();
    let info = pool
        .create(&ada(), "counter", "proj", "", json!({"start": 7}))
        .unwrap();

    // Hand the seat to grace (ada releases, grace takes the empty seat).
    pool.call(&ada(), &info.id, "sys.release", Value::Null)
        .await
        .unwrap();
    pool.call(&grace(), &info.id, "sys.take", Value::Null)
        .await
        .unwrap();

    // The driver does not own: grace may drive incr, but not reset or peek.
    pool.call(&grace(), &info.id, "incr", Value::Null)
        .await
        .unwrap();
    assert!(matches!(
        pool.call(&grace(), &info.id, "reset", Value::Null).await,
        Err(VerbError::Denied { .. })
    ));
    assert!(matches!(
        pool.call(&agent(), &info.id, "peek", Value::Null).await,
        Err(VerbError::Denied { .. })
    ));

    // The owner keeps owned verbs without the seat.
    assert_eq!(
        pool.call(&ada(), &info.id, "peek", Value::Null)
            .await
            .unwrap(),
        json!(8)
    );
    assert_eq!(
        pool.call(&ada(), &info.id, "reset", Value::Null)
            .await
            .unwrap(),
        json!(0)
    );
}

#[tokio::test]
async fn sys_verbs_expose_spec_meta_and_the_verb_log() {
    let pool = pool();
    let info = pool
        .create(&ada(), "counter", "proj", "the-count", Value::Null)
        .unwrap();

    let spec = pool
        .call(&agent(), &info.id, "sys.spec", Value::Null)
        .await
        .unwrap();
    assert_eq!(spec["kind"], "counter");
    assert_eq!(spec["version"], 1);
    assert_eq!(spec["recommended_context"], "text");
    assert!(
        spec["verbs"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v["name"] == "incr" && v["requires_driver"] == true)
    );

    let m = meta(&pool, &info.id).await;
    assert_eq!(m.title, "the-count");
    assert_eq!(m.project, "proj");

    // The log records refusals — that is what makes it a debugger — and its
    // error names come from the serde tag, so they cannot drift.
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
    // Introspection does not log itself: reading the log must not erase it.
    assert!(
        !entries.iter().any(|e| e["verb"]
            .as_str()
            .is_some_and(|v| matches!(v, "sys.log" | "sys.meta" | "sys.spec"))),
        "{entries:?}"
    );

    pool.call(&ada(), &info.id, "sys.rename", json!({"title": "renamed"}))
        .await
        .unwrap();
    assert_eq!(meta(&pool, &info.id).await.title, "renamed");
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
        pool.call(&ada(), &info.id, "explode", Value::Null).await,
        Err(VerbError::UnknownVerb { .. })
    ));
    assert!(matches!(
        pool.call(&ada(), &info.id, "sys.nope", Value::Null).await,
        Err(VerbError::UnknownVerb { .. })
    ));
}

/// Watermarks wake watchers for kind-state changes, meta changes, and
/// removal alike — a `changed` wait always resolves to something readable
/// (or to the discovery that the instance is gone).
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
    let seen = watcher.await.unwrap().unwrap();
    assert!(seen > 0);

    // Meta changes bump too: a title watcher re-reads without the feed.
    let watcher = {
        let pool = pool.clone();
        let id = info.id.clone();
        tokio::spawn(async move { pool.changed(&id, seen).await })
    };
    pool.call(&ada(), &info.id, "sys.rename", json!({"title": "t"}))
        .await
        .unwrap();
    let seen = watcher.await.unwrap().unwrap();

    // An agent that neither created nor drives may not remove.
    assert!(matches!(
        pool.call(&agent(), &info.id, "sys.remove", Value::Null)
            .await,
        Err(VerbError::Denied { .. })
    ));

    // Removal wakes a parked watcher, then the instance is forgotten. The
    // join polls the wait first, so it is parked before the remove runs.
    let (woken, removed) = tokio::join!(pool.changed(&info.id, seen), async {
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        pool.call(&ada(), &info.id, "sys.remove", Value::Null).await
    });
    removed.unwrap();
    assert!(woken.is_ok(), "removal resolves the wait");
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
    pool.call(&ada(), &info.id, "sys.remove", Value::Null)
        .await
        .unwrap();

    let mut names = Vec::new();
    while let Ok((id, event)) = feed.try_recv() {
        assert_eq!(id, info.id);
        if event.name == "created" {
            assert_eq!(event.data["creator"]["id"], "ada");
        }
        names.push(event.name);
    }
    assert_eq!(names, ["created", "driver", "removed"]);
}

#[tokio::test]
async fn titles_are_slugs_unique_per_project() {
    let pool = pool();
    let first = pool
        .create(&ada(), "counter", "alpha", "lab-1", Value::Null)
        .unwrap();
    assert_eq!(first.title, "lab-1");
    // Empty title becomes the kind name.
    let unnamed = pool
        .create(&ada(), "counter", "alpha", "", Value::Null)
        .unwrap();
    assert_eq!(unnamed.title, "counter");
    let twice = pool.create(&ada(), "counter", "alpha", "", Value::Null);
    assert!(
        matches!(
            twice,
            Err(VerbError::BadArgs { ref why }) if why.contains(&unnamed.id)
        ),
        "a second empty title collides on the kind name: {twice:?}"
    );

    for bad in ["has space", "foo_bar", "-leading"] {
        let err = pool
            .create(&ada(), "counter", "alpha", bad, Value::Null)
            .unwrap_err();
        assert!(
            matches!(err, VerbError::BadArgs { ref why } if why.contains("must match")),
            "{bad:?} → {err:?}"
        );
    }

    let clash = pool.create(&ada(), "counter", "alpha", "lab-1", Value::Null);
    assert!(
        matches!(
            clash,
            Err(VerbError::BadArgs { ref why }) if why.contains(&first.id)
        ),
        "{clash:?}"
    );

    // Same title, different project: allowed.
    let other = pool
        .create(&ada(), "counter", "beta", "lab-1", Value::Null)
        .unwrap();
    assert_eq!(other.title, "lab-1");

    // Empty rename is refused; renaming to your own title is not a collision.
    let empty = pool
        .call(&ada(), &other.id, "sys.rename", json!({"title": ""}))
        .await;
    assert!(matches!(empty, Err(VerbError::BadArgs { .. })), "{empty:?}");
    pool.call(&ada(), &other.id, "sys.rename", json!({"title": "lab-1"}))
        .await
        .unwrap();

    let clash_rename = pool
        .call(&ada(), &unnamed.id, "sys.rename", json!({"title": "lab-1"}))
        .await;
    assert!(
        matches!(
            clash_rename,
            Err(VerbError::BadArgs { ref why }) if why.contains(&first.id)
        ),
        "{clash_rename:?}"
    );
}

#[tokio::test]
async fn listing_scopes_by_project() {
    let pool = pool();
    pool.create(&ada(), "counter", "alpha", "a1", Value::Null)
        .unwrap();
    pool.create(&ada(), "counter", "alpha", "a2", Value::Null)
        .unwrap();
    pool.create(&ada(), "counter", "beta", "", Value::Null)
        .unwrap();

    assert_eq!(pool.list(None).len(), 3);
    assert_eq!(pool.list(Some("alpha")).len(), 2);
    assert_eq!(pool.list(Some("beta")).len(), 1);
    assert_eq!(pool.list(Some("gamma")).len(), 0);
}

/// The watermark-wait, once: it checks before it waits (the condition may
/// already hold), wakes on a bump rather than a timer, and gives up at the
/// deadline instead of hanging forever on something that will never be true.
#[tokio::test]
async fn wait_until_checks_first_wakes_on_a_bump_and_gives_up_at_the_deadline() {
    let pool = pool();
    let info = pool
        .create(&ada(), "counter", "proj", "", json!({"start": 3}))
        .unwrap();
    let far = tokio::time::Instant::now() + std::time::Duration::from_secs(30);

    let seen = pool
        .wait_until(&ada(), &info.id, "get", far, |v| v == &json!(3))
        .await
        .unwrap();
    assert_eq!(seen, Some(json!(3)), "already true: no wait at all");

    let bumper = pool.clone();
    let id = info.id.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        bumper
            .call(&ada(), &id, "incr", json!({"by": 1}))
            .await
            .unwrap();
    });
    let seen = pool
        .wait_until(&ada(), &info.id, "get", far, |v| v == &json!(4))
        .await
        .unwrap();
    assert_eq!(seen, Some(json!(4)), "true later: the bump wakes the read");

    let soon = tokio::time::Instant::now() + std::time::Duration::from_millis(50);
    let seen = pool
        .wait_until(&ada(), &info.id, "get", soon, |v| v == &json!(99))
        .await
        .unwrap();
    assert_eq!(seen, None, "never true: the deadline ends it");
}

/// Parentage is identity, not kind state: fixed at birth, carried by the
/// listing without entering any cell, refused when it names nothing, and
/// left standing when the parent is removed — a child that is orphaned is
/// not a child that was lying about where it came from.
#[tokio::test]
async fn parentage_is_fixed_at_birth_and_carried_by_the_listing() {
    let pool = pool();
    let root = pool
        .create(&ada(), "counter", "proj", "root", Value::Null)
        .unwrap();
    assert_eq!(root.parent, None, "an unparented instance is a root");

    let child = pool
        .create_under(&ada(), "counter", "proj", "child", Value::Null, Some(&root.id))
        .unwrap();
    let grandchild = pool
        .create_under(
            &ada(),
            "counter",
            "proj",
            "grandchild",
            Value::Null,
            Some(&child.id),
        )
        .unwrap();
    assert_eq!(child.parent.as_deref(), Some(root.id.as_str()));
    assert_eq!(
        meta(&pool, &child.id).await.parent,
        child.parent,
        "sys.meta and the create reply agree"
    );
    assert_eq!(
        pool.list(Some("proj"))
            .iter()
            .filter(|i| i.parent.is_some())
            .count(),
        2
    );

    // Nearest first, so a depth cap is a length.
    assert_eq!(
        pool.ancestors(&grandchild.id),
        vec![child.id.clone(), root.id.clone()]
    );
    assert!(pool.ancestors(&root.id).is_empty());

    // A parent nothing answers to is a bad birth certificate, not a root.
    assert_eq!(
        pool.create_under(&ada(), "counter", "proj", "", Value::Null, Some("nobody")),
        Err(VerbError::UnknownInstance {
            id: "nobody".into()
        })
    );

    pool.call(&ada(), &root.id, "sys.remove", Value::Null)
        .await
        .unwrap();
    assert_eq!(
        meta(&pool, &child.id).await.parent.as_deref(),
        Some(root.id.as_str()),
        "removal orphans; it does not rewrite history"
    );
}

/// A panicking kind takes down its own instance and nothing else; the
/// corpse answers `Gone`, and its `sys.meta` (entry state, not cell state)
/// still reports `crashed` for the tree UI.
#[tokio::test]
async fn a_kind_bug_crashes_one_instance_only() {
    struct Buggy;
    static BUGGY_SPEC: KindSpec = KindSpec {
        kind: "buggy",
        version: 1,
        doc: "panics on demand",
        verbs: &[
            VerbSpec::write("boom", "panic"),
            VerbSpec::read("get", "value"),
        ],
        primary_render: "get",
        recommended_context: "get",
    };
    struct BuggyInstance;
    #[async_trait::async_trait]
    impl Instance for BuggyInstance {
        async fn verb(
            &mut self,
            _caller: &Principal,
            verb: &str,
            _: Value,
            _: &Signals,
        ) -> Result<Value, VerbError> {
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
        fn create(&self, _: &str, _: Value, _: Signals) -> Result<Box<dyn Instance>, VerbError> {
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
    // The crashed flag is eventually-true; Gone above was the definitive
    // signal. Wait for the monitor to mark the corpse for listings.
    let mut tries = 0;
    while !meta(&pool, &victim.id).await.crashed && tries < 200 {
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        tries += 1;
    }
    assert!(meta(&pool, &victim.id).await.crashed);

    // The bystander never noticed.
    assert_eq!(
        pool.call(&ada(), &bystander.id, "get", Value::Null)
            .await
            .unwrap(),
        json!(0)
    );
}

/// The attention envelope round-trips — the pinned convention notifiers and
/// standing subscriptions will share.
#[test]
fn the_attention_envelope_round_trips() {
    let a = events::Attention {
        for_: vec![ada()],
        title: "turn finished".into(),
        body: "the build is green".into(),
    };
    let data = a.data();
    assert_eq!(data["for"][0]["id"], "ada");
    assert_eq!(events::Attention::from_data(&data), Some(a));
}
