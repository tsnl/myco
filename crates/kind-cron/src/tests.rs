//! The table under a paused clock: cadence, attribution, parking, and
//! the cursored run log. Virtual time makes every beat deterministic —
//! a sleep here is time travel, not waiting.

use super::*;
use std::sync::Arc;

static TARGET_SPEC: KindSpec = KindSpec {
    kind: "target",
    version: 1,
    doc: "remembers pokes and who poked",
    verbs: &[
        VerbSpec::read("about", "pokes so far and the last poker"),
        VerbSpec::write("poke", "be poked"),
    ],
    primary_render: "about",
    recommended_context: "about",
};

struct TargetKind;

impl Kind for TargetKind {
    fn spec(&self) -> &'static KindSpec {
        &TARGET_SPEC
    }
    fn create(
        &self,
        _ctx: &CreateCtx,
        _args: Value,
        _signals: Signals,
    ) -> Result<Box<dyn Instance>, VerbError> {
        Ok(Box::new(Target {
            pokes: 0,
            last: None,
        }))
    }
}

struct Target {
    pokes: u64,
    last: Option<Principal>,
}

#[async_trait::async_trait]
impl Instance for Target {
    async fn verb(
        &mut self,
        caller: &Principal,
        verb: &str,
        _args: Value,
        signals: &Signals,
    ) -> Result<Value, VerbError> {
        match verb {
            "about" => Ok(json!({ "pokes": self.pokes, "last": self.last })),
            "poke" => {
                self.pokes += 1;
                self.last = Some(caller.clone());
                signals.bump();
                Ok(Value::Null)
            }
            other => Err(VerbError::UnknownVerb { verb: other.into() }),
        }
    }
}

fn ada() -> Principal {
    Principal::Human("ada".into())
}

fn bob() -> Principal {
    Principal::Human("bob".into())
}

/// A pool with one cron table and one target; answers (pool, cron, target).
fn rig() -> (Pool, String, String) {
    let pool = Pool::new();
    pool.register(Arc::new(TargetKind));
    pool.register(Arc::new(CronKind::new(pool.clone())));
    let cron = pool
        .create(&ada(), "cron", "", "the-table", json!({}))
        .expect("cron creates")
        .id;
    let target = pool
        .create(&ada(), "target", "", "the-target", json!({}))
        .expect("target creates")
        .id;
    (pool, cron, target)
}

async fn pokes(pool: &Pool, target: &str) -> (u64, Option<Principal>) {
    let about = pool
        .call(&ada(), target, "about", Value::Null)
        .await
        .expect("about answers");
    (
        about["pokes"].as_u64().unwrap(),
        serde_json::from_value(about["last"].clone()).unwrap(),
    )
}

async fn add_every_60(pool: &Pool, cron: &str, target: &str) -> u64 {
    let added = pool
        .call(
            &ada(),
            cron,
            "add",
            json!({ "target": target, "verb": "poke", "every_secs": 60 }),
        )
        .await
        .expect("add answers");
    added["entry"].as_u64().expect("an entry id")
}

#[tokio::test(start_paused = true)]
async fn entries_fire_on_cadence_as_their_author() {
    let (pool, cron, target) = rig();
    add_every_60(&pool, &cron, &target).await;

    // Not a beat early.
    tokio::time::sleep(std::time::Duration::from_secs(59)).await;
    assert_eq!(pokes(&pool, &target).await.0, 0);

    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    let (count, last) = pokes(&pool, &target).await;
    assert_eq!(count, 1);
    assert_eq!(last, Some(ada()), "the entry fires as its author");

    tokio::time::sleep(std::time::Duration::from_secs(60)).await;
    assert_eq!(pokes(&pool, &target).await.0, 2);
}

#[tokio::test(start_paused = true)]
async fn pause_holds_and_resume_releases() {
    let (pool, cron, target) = rig();
    let entry = add_every_60(&pool, &cron, &target).await;

    pool.call(&ada(), &cron, "pause", json!({ "entry": entry }))
        .await
        .expect("pauses");
    tokio::time::sleep(std::time::Duration::from_secs(300)).await;
    assert_eq!(pokes(&pool, &target).await.0, 0, "a paused entry holds");

    pool.call(&ada(), &cron, "resume", json!({ "entry": entry }))
        .await
        .expect("resumes");
    tokio::time::sleep(std::time::Duration::from_secs(61)).await;
    assert!(pokes(&pool, &target).await.0 >= 1, "a resumed entry fires");
}

#[tokio::test(start_paused = true)]
async fn rm_forgets_the_entry_and_its_clock() {
    let (pool, cron, target) = rig();
    let entry = add_every_60(&pool, &cron, &target).await;
    pool.call(&ada(), &cron, "rm", json!({ "entry": entry }))
        .await
        .expect("removes");
    tokio::time::sleep(std::time::Duration::from_secs(300)).await;
    assert_eq!(pokes(&pool, &target).await.0, 0);
    assert!(matches!(
        pool.call(&ada(), &cron, "rm", json!({ "entry": entry }))
            .await,
        Err(VerbError::BadArgs { .. })
    ));
}

#[tokio::test(start_paused = true)]
async fn a_gone_target_parks_its_entry() {
    let (pool, cron, target) = rig();
    add_every_60(&pool, &cron, &target).await;
    pool.call(&ada(), &target, "sys.remove", Value::Null)
        .await
        .expect("removes the target");

    tokio::time::sleep(std::time::Duration::from_secs(61)).await;
    let about = pool
        .call(&ada(), &cron, "about", Value::Null)
        .await
        .expect("about answers");
    assert_eq!(about["entries"][0]["paused"], true, "parked, not grinding");
    assert!(
        about["entries"][0]["parked"]
            .as_str()
            .is_some_and(|why| why.contains("gone")),
        "the parking names its reason"
    );
    let runs_after_park = about["runs"].as_u64().unwrap();

    // Parked means parked: no further failing runs pile up.
    tokio::time::sleep(std::time::Duration::from_secs(300)).await;
    let about = pool
        .call(&ada(), &cron, "about", Value::Null)
        .await
        .expect("about answers");
    assert_eq!(about["runs"].as_u64().unwrap(), runs_after_park);
}

#[tokio::test(start_paused = true)]
async fn fire_runs_now_as_the_firer() {
    let (pool, cron, target) = rig();
    let entry = add_every_60(&pool, &cron, &target).await;

    pool.call(&bob(), &cron, "fire", json!({ "entry": entry }))
        .await
        .expect("fires");
    let (count, last) = pokes(&pool, &target).await;
    assert_eq!(count, 1);
    assert_eq!(
        last,
        Some(bob()),
        "fire is the firer's act, never the author's authority"
    );
}

#[tokio::test(start_paused = true)]
async fn runs_is_a_cursored_read() {
    let (pool, cron, target) = rig();
    let entry = add_every_60(&pool, &cron, &target).await;
    pool.call(&bob(), &cron, "fire", json!({ "entry": entry }))
        .await
        .expect("fires");
    tokio::time::sleep(std::time::Duration::from_secs(61)).await;

    let page = pool
        .call(&ada(), &cron, "runs", json!({ "from": 0 }))
        .await
        .expect("runs answers");
    let runs = page["runs"].as_array().unwrap();
    assert_eq!(runs.len(), 2);
    assert_eq!(runs[0]["as_"]["id"], "bob");
    assert_eq!(runs[1]["as_"]["id"], "ada");
    let next = page["next"].as_u64().unwrap();

    let empty = pool
        .call(&ada(), &cron, "runs", json!({ "from": next }))
        .await
        .expect("runs answers");
    assert_eq!(empty["runs"].as_array().unwrap().len(), 0);
    assert_eq!(empty["next"].as_u64().unwrap(), next);
}
