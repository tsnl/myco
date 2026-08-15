//! Driven through the pool, not the struct: attribution comes from the bus,
//! and that path is the claim under test.

use std::sync::Arc;

use myco_instance::{Pool, Principal, VerbError};
use serde_json::{Value, json};

use super::ChatKind;

fn ada() -> Principal {
    Principal::Human("ada".into())
}

fn pool_with_chat(args: Value) -> (Pool, String) {
    let pool = Pool::new();
    pool.register(Arc::new(ChatKind));
    let info = pool.create(&ada(), "chat", "", "", args).expect("create");
    (pool, info.id)
}

#[tokio::test]
async fn posts_are_attributed_to_the_bus_principal_and_bump_the_watermark() {
    let (pool, id) = pool_with_chat(Value::Null);
    let before = pool.watermark(&id).expect("watermark");

    let posted = pool
        .call(&ada(), &id, "post", json!({"text": "hello"}))
        .await
        .expect("post");
    assert_eq!(posted["seq"], 0);
    assert_eq!(posted["author"]["kind"], "human");
    assert_eq!(posted["author"]["id"], "ada");

    // A different principal posts to the same chat — no seat needed, the
    // transcript is multiplayer; only the attribution differs.
    let agent = Principal::Agent(id.clone());
    pool.call(&agent, &id, "post", json!({"text": "hi ada"}))
        .await
        .expect("agent post");

    let after = pool.watermark(&id).expect("watermark");
    assert!(after > before, "posts wake watchers");

    let tail = pool
        .call(&ada(), &id, "tail", Value::Null)
        .await
        .expect("tail");
    let entries = tail["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0]["author"]["kind"], "human");
    assert_eq!(entries[1]["author"]["kind"], "agent");
    assert_eq!(entries[1]["t"], "message");
}

/// The author is the framework's fact: nothing in the arguments can forge
/// it — an `author` field in the args is simply ignored.
#[tokio::test]
async fn the_arguments_cannot_forge_an_author() {
    let (pool, id) = pool_with_chat(Value::Null);
    let posted = pool
        .call(
            &ada(),
            &id,
            "post",
            json!({"text": "x", "author": {"kind": "agent", "id": "mallory"}}),
        )
        .await
        .expect("post");
    assert_eq!(posted["author"]["id"], "ada");
}

#[tokio::test]
async fn tail_cursors_in_dense_sequence_numbers_with_a_budget() {
    let (pool, id) = pool_with_chat(Value::Null);
    for i in 0..5 {
        pool.call(&ada(), &id, "post", json!({"text": format!("m{i}")}))
            .await
            .expect("post");
    }

    let first = pool
        .call(&ada(), &id, "tail", json!({"from": 0, "max_entries": 2}))
        .await
        .expect("tail");
    assert_eq!(first["entries"].as_array().unwrap().len(), 2);
    assert_eq!(first["next"], 2);
    assert_eq!(first["len"], 5);

    // Resume from `next`: the cursor contract, no overlap, no gap.
    let rest = pool
        .call(&ada(), &id, "tail", json!({"from": 2}))
        .await
        .expect("tail");
    let entries = rest["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 3);
    assert_eq!(entries[0]["seq"], 2);
    assert_eq!(rest["next"], 5);

    // A cursor past the end is clamped, answers empty, and is not an error
    // (a quiet chat is not a broken one).
    let past = pool
        .call(&ada(), &id, "tail", json!({"from": 99}))
        .await
        .expect("tail");
    assert_eq!(past["entries"].as_array().unwrap().len(), 0);
    assert_eq!(past["next"], 5);
}

#[tokio::test]
async fn text_is_the_plain_transcript() {
    let (pool, id) = pool_with_chat(Value::Null);
    pool.call(&ada(), &id, "post", json!({"text": "hello"}))
        .await
        .expect("post");
    pool.call(
        &Principal::Agent(id.clone()),
        &id,
        "post",
        json!({"text": "hi"}),
    )
    .await
    .expect("post");

    let text = pool
        .call(&ada(), &id, "text", Value::Null)
        .await
        .expect("text");
    assert_eq!(
        text["text"].as_str().unwrap(),
        format!("ada: hello\nagent:{id}: hi\n")
    );
}

/// A subagent is a chat and nothing more: the parentage lives in L1's
/// identity, so the kind has no field to set, no field to leak, and no way
/// for the two answers to drift apart.
#[tokio::test]
async fn a_subagent_is_a_chat_created_under_another_chat() {
    let (pool, parent_id) = pool_with_chat(Value::Null);
    let child = pool
        .create_under(&ada(), "chat", "", "", Value::Null, Some(&parent_id))
        .expect("create child");
    assert_eq!(child.parent.as_deref(), Some(parent_id.as_str()));

    let about = pool
        .call(&ada(), &child.id, "about", Value::Null)
        .await
        .expect("about");
    assert_eq!(about, json!({"len": 0}), "the kind knows only its own log");

    let listed = pool.list(None);
    let child_row = listed.iter().find(|i| i.id == child.id).expect("listed");
    assert_eq!(child_row.parent.as_deref(), Some(parent_id.as_str()));
}

#[tokio::test]
async fn empty_posts_are_refused_by_name() {
    let (pool, id) = pool_with_chat(Value::Null);
    for bad in [json!({}), json!({"text": ""}), json!({"text": "   "})] {
        let err = pool.call(&ada(), &id, "post", bad).await.unwrap_err();
        assert!(matches!(err, VerbError::BadArgs { .. }), "{err}");
    }
}
