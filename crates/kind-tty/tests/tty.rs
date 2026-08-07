//! The tty kind end to end, driven only through the bus — the same calls
//! any adapter (HTTP, agent dispatcher) would make. This is the abstraction
//! working for a living: nothing here imports pty, vt100, or the kind's
//! internals.

use std::sync::Arc;
use std::time::Duration;

use myco_instance::{Pool, Principal, VerbError};
use myco_kind_tty::TtyKind;
use serde_json::{Value, json};

fn pool() -> Pool {
    let pool = Pool::new();
    pool.register(Arc::new(TtyKind));
    pool
}

fn ada() -> Principal {
    Principal::Human("ada".into())
}

fn agent() -> Principal {
    Principal::Agent("chat-1".into())
}

/// Wait (bounded) until a read verb's result satisfies `pred`, waking on
/// watermark changes rather than polling blind.
async fn wait_for(
    pool: &Pool,
    principal: &Principal,
    id: &str,
    verb: &str,
    args: Value,
    pred: impl Fn(&Value) -> bool,
) -> Value {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let mut seen = 0;
    loop {
        let got = pool
            .call(principal, id, verb, args.clone())
            .await
            .expect("read verb");
        if pred(&got) {
            return got;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting on {verb}; last: {got}"
        );
        seen = tokio::time::timeout(Duration::from_millis(500), pool.changed(id, seen))
            .await
            .map(|r| r.expect("instance alive"))
            .unwrap_or(seen);
    }
}

#[tokio::test]
async fn a_terminal_echoes_what_its_driver_types() {
    let pool = pool();
    let tty = pool
        .create(&ada(), "tty", "proj", "cat", json!({"command": "cat"}))
        .unwrap();

    pool.call(&ada(), &tty.id, "input", json!({"data": "hello\n"}))
        .await
        .unwrap();

    // Echo + cat's copy: "hello" appears twice in the raw stream.
    let tail = wait_for(&pool, &agent(), &tty.id, "tail", json!({"from": 0}), |v| {
        v["data"].as_str().unwrap_or("").matches("hello").count() >= 2
    })
    .await;
    assert!(tail["next"].as_u64().unwrap() > 0);

    // The screen shows it too, and the plain-text read agrees.
    let text = wait_for(&pool, &agent(), &tty.id, "text", Value::Null, |v| {
        v["text"].as_str().unwrap_or("").contains("hello")
    })
    .await;
    assert_eq!(text["running"], true);
    let screen = pool
        .call(&agent(), &tty.id, "screen", Value::Null)
        .await
        .unwrap();
    assert!(
        screen["runs"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["text"].as_str().unwrap_or("").contains("hello"))
    );
}

#[tokio::test]
async fn typing_is_driver_gated_but_watching_is_not() {
    let pool = pool();
    let tty = pool
        .create(&ada(), "tty", "proj", "", json!({"command": "cat"}))
        .unwrap();

    // The agent may watch freely…
    pool.call(&agent(), &tty.id, "screen", Value::Null)
        .await
        .unwrap();
    // …but not type: ada created the terminal, so ada drives.
    let refused = pool
        .call(&agent(), &tty.id, "input", json!({"data": "x"}))
        .await;
    assert_eq!(
        refused,
        Err(VerbError::NotDriver {
            held_by: Some(ada())
        })
    );

    // Ada steps away; the agent takes the seat and types.
    pool.call(&ada(), &tty.id, "sys.release", Value::Null)
        .await
        .unwrap();
    pool.call(&agent(), &tty.id, "sys.take", Value::Null)
        .await
        .unwrap();
    pool.call(&agent(), &tty.id, "input", json!({"data": "y\n"}))
        .await
        .unwrap();
}

#[tokio::test]
async fn resize_reshapes_the_screen() {
    let pool = pool();
    let tty = pool
        .create(&ada(), "tty", "proj", "", json!({"command": "cat"}))
        .unwrap();

    let before = pool
        .call(&ada(), &tty.id, "screen", Value::Null)
        .await
        .unwrap();
    assert_eq!(
        (before["cols"].as_u64(), before["rows"].as_u64()),
        (Some(80), Some(24))
    );

    pool.call(&ada(), &tty.id, "resize", json!({"cols": 100, "rows": 30}))
        .await
        .unwrap();
    let after = pool
        .call(&ada(), &tty.id, "screen", Value::Null)
        .await
        .unwrap();
    assert_eq!(
        (after["cols"].as_u64(), after["rows"].as_u64()),
        (Some(100), Some(30))
    );

    let bad = pool
        .call(
            &ada(),
            &tty.id,
            "resize",
            json!({"cols": 100000, "rows": 30}),
        )
        .await;
    assert!(matches!(bad, Err(VerbError::BadArgs { .. })));
}

#[tokio::test]
async fn a_finished_child_reports_exit_and_the_transcript_survives_it() {
    let pool = pool();
    let mut feed = pool.events();
    let tty = pool
        .create(&ada(), "tty", "proj", "", json!({"command": "echo done"}))
        .unwrap();

    let text = wait_for(&pool, &ada(), &tty.id, "text", Value::Null, |v| {
        v["running"] == false
    })
    .await;
    assert!(text["text"].as_str().unwrap().contains("done"));

    // The exit is an event on the global feed — history, not just state.
    let mut saw_exit = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while !saw_exit && tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(200), feed.recv()).await {
            Ok(Ok((id, event))) => saw_exit = id == tty.id && event.name == "exited",
            _ => break,
        }
    }
    assert!(saw_exit, "the exited event reaches the global feed");

    // Dead child, living record: the tail still reads.
    let tail = pool
        .call(&ada(), &tty.id, "tail", json!({"from": 0}))
        .await
        .unwrap();
    assert!(tail["data"].as_str().unwrap().contains("done"));
}

#[tokio::test]
async fn removal_forgets_the_terminal() {
    let pool = pool();
    let tty = pool
        .create(&ada(), "tty", "proj", "", json!({"command": "sleep 100"}))
        .unwrap();
    pool.remove(&ada(), &tty.id).unwrap();
    assert!(matches!(
        pool.call(&ada(), &tty.id, "screen", Value::Null).await,
        Err(VerbError::UnknownInstance { .. })
    ));
}
