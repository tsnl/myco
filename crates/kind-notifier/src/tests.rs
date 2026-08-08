//! Driven through the pool: mentions posted to real chats, items read
//! back through the owner-scoped verbs, privacy enforced by the framework.

use std::sync::Arc;

use myco_instance::{Pool, Principal, VerbError};
use serde_json::{Value, json};

use super::NotifierKind;

fn ada() -> Principal {
    Principal::Human("ada".into())
}
fn grace() -> Principal {
    Principal::Human("grace".into())
}

fn fixture() -> Pool {
    let pool = Pool::new();
    pool.register(Arc::new(myco_kind_chat::ChatKind::transcript_only(
        pool.clone(),
    )));
    pool.register(Arc::new(NotifierKind::new(pool.clone())));
    pool
}

async fn wait_for_unacked(pool: &Pool, id: &str, owner: &Principal, n: u64) -> Value {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
    pool.wait_until(owner, id, "pending", deadline, |pending| {
        pending["unacked"] == json!(n)
    })
    .await
    .expect("pending")
    .unwrap_or_else(|| panic!("the badge never reached {n} unacked"))
}

/// The live path: a mention in a chat becomes a private item, acking
/// clears the badge, and the acked item stays on the record.
#[tokio::test]
async fn a_mention_becomes_a_private_item_until_acked() {
    let pool = fixture();
    let notifier = pool
        .create(&ada(), "notifier", "", "", Value::Null)
        .expect("notifier");
    let chat = pool
        .create(&grace(), "chat", "", "planning", Value::Null)
        .expect("chat");

    pool.call(
        &grace(),
        &chat.id,
        "post",
        json!({"text": "@ada the tests are green"}),
    )
    .await
    .expect("post");

    let pending = wait_for_unacked(&pool, &notifier.id, &ada(), 1).await;
    let item = &pending["items"][0];
    assert_eq!(item["source"], json!(chat.id));
    assert!(item["title"].as_str().unwrap().contains("grace"), "{item}");
    assert!(item["body"].as_str().unwrap().contains("tests are green"));
    assert_eq!(item["acked"], json!(false));

    // The plain projection is the agent's window into the unseen pile.
    let text = pool
        .call(&ada(), &notifier.id, "text", Value::Null)
        .await
        .expect("text");
    assert!(text["text"].as_str().unwrap().contains("grace"));

    // Ack: the badge falls, the item stays (state, then history).
    let acked = pool
        .call(&ada(), &notifier.id, "ack", json!({"upto": 0}))
        .await
        .expect("ack");
    assert_eq!(acked["unacked"], json!(0));
    let pending = pool
        .call(&ada(), &notifier.id, "pending", Value::Null)
        .await
        .expect("pending");
    assert_eq!(pending["items"][0]["acked"], json!(true));
    assert_eq!(pending["len"], json!(1));
}

/// Attention is private: every owner-scoped verb refuses everyone but the
/// creator — the framework's owner_only axis doing the whole job.
#[tokio::test]
async fn the_inbox_refuses_everyone_but_its_owner() {
    let pool = fixture();
    let notifier = pool
        .create(&ada(), "notifier", "", "", Value::Null)
        .expect("notifier");

    for (verb, args) in [
        ("pending", Value::Null),
        ("text", Value::Null),
        ("ack", json!({"upto": 0})),
        ("mute", json!({"source": "x"})),
        (
            "register",
            json!({"subscription": {"endpoint": "https://p.example/x"}}),
        ),
        ("unregister", json!({"endpoint": "https://p.example/x"})),
        ("reconcile", Value::Null),
    ] {
        let err = pool
            .call(&grace(), &notifier.id, verb, args)
            .await
            .unwrap_err();
        assert!(matches!(err, VerbError::Denied { .. }), "{verb}: {err}");
    }
}

/// Birth reconcile: a mention posted before the notifier existed still
/// arrives — "you were mentioned" survives not being logged in yet.
#[tokio::test]
async fn a_mention_before_first_login_is_backfilled() {
    let pool = fixture();
    let chat = pool
        .create(&grace(), "chat", "", "", Value::Null)
        .expect("chat");
    pool.call(
        &grace(),
        &chat.id,
        "post",
        json!({"text": "@ada early bird"}),
    )
    .await
    .expect("post");

    let notifier = pool
        .create(&ada(), "notifier", "", "", Value::Null)
        .expect("notifier");
    let pending = wait_for_unacked(&pool, &notifier.id, &ada(), 1).await;
    assert!(
        pending["items"][0]["body"]
            .as_str()
            .unwrap()
            .contains("early bird")
    );
}

/// The feed and the reconcile scan race by design; the (source, seq)
/// dedupe makes them idempotent — one mention, one item, no matter how
/// many paths deliver it.
#[tokio::test]
async fn reconcile_never_duplicates_what_the_feed_delivered() {
    let pool = fixture();
    let notifier = pool
        .create(&ada(), "notifier", "", "", Value::Null)
        .expect("notifier");
    let chat = pool
        .create(&grace(), "chat", "", "", Value::Null)
        .expect("chat");
    pool.call(&grace(), &chat.id, "post", json!({"text": "@ada once"}))
        .await
        .expect("post");
    wait_for_unacked(&pool, &notifier.id, &ada(), 1).await;

    let after = pool
        .call(&ada(), &notifier.id, "reconcile", Value::Null)
        .await
        .expect("reconcile");
    assert_eq!(after["len"], json!(1), "no duplicate item");
}

/// Muting a source stops itemization; the proof is ordered: a later
/// mention from an unmuted chat arrives while the muted one's never does.
#[tokio::test]
async fn muted_sources_are_skipped() {
    let pool = fixture();
    let notifier = pool
        .create(&ada(), "notifier", "", "", Value::Null)
        .expect("notifier");
    let noisy = pool
        .create(&grace(), "chat", "", "noisy", Value::Null)
        .expect("chat");
    let quiet = pool
        .create(&grace(), "chat", "", "quiet", Value::Null)
        .expect("chat");

    pool.call(&ada(), &notifier.id, "mute", json!({"source": noisy.id}))
        .await
        .expect("mute");
    pool.call(
        &grace(),
        &noisy.id,
        "post",
        json!({"text": "@ada muted ping"}),
    )
    .await
    .expect("post");
    pool.call(
        &grace(),
        &quiet.id,
        "post",
        json!({"text": "@ada real ping"}),
    )
    .await
    .expect("post");

    // The feed is ordered: when the quiet chat's item has landed, the
    // noisy one's event has already been processed (and skipped).
    let pending = wait_for_unacked(&pool, &notifier.id, &ada(), 1).await;
    assert_eq!(pending["items"][0]["source"], json!(quiet.id));

    // Unmute + reconcile picks the muted mention back up from the record.
    pool.call(
        &ada(),
        &notifier.id,
        "mute",
        json!({"source": noisy.id, "on": false}),
    )
    .await
    .expect("unmute");
    let after = pool
        .call(&ada(), &notifier.id, "reconcile", Value::Null)
        .await
        .expect("reconcile");
    assert_eq!(after["unacked"], json!(2));
}

/// Push endpoints are write-only state: stored, replaceable, removable —
/// and no read verb ever returns them.
#[tokio::test]
async fn push_endpoints_are_write_only() {
    let pool = fixture();
    let notifier = pool
        .create(&ada(), "notifier", "", "", Value::Null)
        .expect("notifier");
    let endpoint = "https://push.example/very-secret-token";
    pool.call(
        &ada(),
        &notifier.id,
        "register",
        json!({"subscription": {"endpoint": endpoint, "keys": {"auth": "a", "p256dh": "b"}}}),
    )
    .await
    .expect("register");

    for verb in ["pending", "text"] {
        let out = pool
            .call(&ada(), &notifier.id, verb, Value::Null)
            .await
            .expect(verb);
        assert!(
            !out.to_string().contains("very-secret-token"),
            "{verb} leaks the endpoint"
        );
    }

    let dropped = pool
        .call(
            &ada(),
            &notifier.id,
            "unregister",
            json!({"endpoint": endpoint}),
        )
        .await
        .expect("unregister");
    assert_eq!(dropped["dropped"], json!(true));
}
