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

/// What the mock recorded: (headers, body) per push request.
type Hits = Arc<std::sync::Mutex<Vec<(String, Vec<u8>)>>>;

/// A push service small enough to read: raw TCP, one request per
/// connection, scripted status. Records (headers, body) pairs.
async fn mock_push_service(status: Arc<std::sync::atomic::AtomicU16>, hits: Hits) -> String {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((mut sock, _)) = listener.accept().await {
            let status = status.clone();
            let hits = hits.clone();
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                let (head_end, need) = loop {
                    let Ok(n) = sock.read(&mut chunk).await else {
                        return;
                    };
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&buf[..pos]).to_lowercase();
                        let len = head
                            .lines()
                            .find_map(|l| l.strip_prefix("content-length:"))
                            .and_then(|v| v.trim().parse::<usize>().ok())
                            .unwrap_or(0);
                        break (pos + 4, len);
                    }
                };
                while buf.len() < head_end + need {
                    let Ok(n) = sock.read(&mut chunk).await else {
                        return;
                    };
                    if n == 0 {
                        break;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                }
                let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
                let body = buf[head_end..].to_vec();
                hits.lock().unwrap().push((head, body));
                let code = status.load(std::sync::atomic::Ordering::SeqCst);
                let _ = sock
                    .write_all(
                        format!(
                            "HTTP/1.1 {code} X\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
                        )
                        .as_bytes(),
                    )
                    .await;
            });
        }
    });
    format!("http://{addr}/wpush/v1/sub-1")
}

/// The whole delivery story: a live mention pushes a sealed payload the
/// receiver's keys open; reconcile's backfill pushes nothing; a 410
/// prunes the endpoint so later items stop knocking.
#[tokio::test]
async fn live_items_push_sealed_payloads_and_dead_endpoints_prune() {
    use base64::Engine as _;
    let b64 = &base64::engine::general_purpose::URL_SAFE_NO_PAD;

    let status = Arc::new(std::sync::atomic::AtomicU16::new(201));
    let hits = Arc::new(std::sync::Mutex::new(Vec::new()));
    let endpoint = mock_push_service(status.clone(), hits.clone()).await;

    // The pusher speaks plain HTTP to localhost; no proxy may interfere.
    let pusher =
        crate::push::Pusher::for_tests(reqwest::Client::builder().no_proxy().build().unwrap());
    let pool = Pool::new();
    pool.register(Arc::new(myco_kind_chat::ChatKind::transcript_only(
        pool.clone(),
    )));
    pool.register(Arc::new(NotifierKind::with_push(
        pool.clone(),
        Arc::new(pusher),
    )));

    let notifier = pool
        .create(&ada(), "notifier", "", "", Value::Null)
        .expect("notifier");
    let chat = pool
        .create(&grace(), "chat", "", "planning", Value::Null)
        .expect("chat");

    // A browser-shaped subscription with keys we hold the other half of.
    let ua_secret = crate::push::random_secret();
    use p256::elliptic_curve::sec1::ToEncodedPoint as _;
    let ua_public = ua_secret.public_key().to_encoded_point(false);
    let auth: Vec<u8> = (0..16u8).collect();
    let subscription = json!({
        "endpoint": endpoint,
        "keys": {
            "p256dh": b64.encode(ua_public.as_bytes()),
            "auth": b64.encode(&auth),
        },
    });
    pool.call(
        &ada(),
        &notifier.id,
        "register",
        json!({ "subscription": subscription }),
    )
    .await
    .expect("registers");

    pool.call(
        &grace(),
        &chat.id,
        "post",
        json!({"text": "@ada the far box is up"}),
    )
    .await
    .expect("posts");
    wait_for_unacked(&pool, &notifier.id, &ada(), 1).await;

    // The push is async beside the inbox; give it its moment.
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    while hits.lock().unwrap().is_empty() {
        assert!(tokio::time::Instant::now() < deadline, "a push arrived");
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let (head, body) = hits.lock().unwrap()[0].clone();
    assert!(head.contains("content-encoding: aes128gcm"));
    assert!(head.contains("authorization: vapid t="));
    let opened = crate::push::decrypt(&ua_secret, &auth, &body);
    let payload: Value = serde_json::from_slice(&opened).expect("the payload is JSON");
    assert!(
        payload["body"]
            .as_str()
            .is_some_and(|b| b.contains("far box")),
        "the sealed payload carries the item, got {payload}"
    );

    // The service turns the subscription away for good; the next live
    // item knocks once, then the endpoint is gone.
    status.store(410, std::sync::atomic::Ordering::SeqCst);
    pool.call(&grace(), &chat.id, "post", json!({"text": "@ada two"}))
        .await
        .expect("posts");
    wait_for_unacked(&pool, &notifier.id, &ada(), 2).await;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    while hits.lock().unwrap().len() < 2 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the 410 knock arrived"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    pool.call(&grace(), &chat.id, "post", json!({"text": "@ada three"}))
        .await
        .expect("posts");
    wait_for_unacked(&pool, &notifier.id, &ada(), 3).await;
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert_eq!(
        hits.lock().unwrap().len(),
        2,
        "a pruned endpoint is not knocked again — the item still landed in the inbox"
    );
}
