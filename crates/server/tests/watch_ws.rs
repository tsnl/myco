//! Observation over the wire: the long-poll resolves on change, the
//! multiplexed socket delivers marks for watched instances and the global
//! event feed for everyone — including `gone` when a watched instance is
//! removed. This drives a real listener (WebSockets cannot be oneshot).

mod common;

use std::time::Duration;

use futures::{SinkExt as _, StreamExt};
use myco_instance::{Pool, Principal};
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::Message;

fn ada() -> Principal {
    Principal::Human("ada".into())
}

/// Serve a counter app on an ephemeral port; return (host:port, pool, token).
async fn serve() -> (String, Pool, String) {
    let (router, pool, token) = common::counter_app();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    (format!("127.0.0.1:{}", addr.port()), pool, token)
}

async fn next_json(
    socket: &mut (impl StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin),
) -> Value {
    loop {
        let msg = tokio::time::timeout(Duration::from_secs(10), socket.next())
            .await
            .expect("frame before timeout")
            .expect("socket open")
            .expect("frame ok");
        if let Message::Text(text) = msg {
            return serde_json::from_str(text.as_ref()).expect("json frame");
        }
    }
}

/// Read frames until `pred` matches, tolerating interleaved feed traffic.
async fn frame_where(
    socket: &mut (impl StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin),
    pred: impl Fn(&Value) -> bool,
) -> Value {
    for _ in 0..50 {
        let frame = next_json(socket).await;
        if pred(&frame) {
            return frame;
        }
    }
    panic!("predicate never matched");
}

#[tokio::test]
async fn the_socket_delivers_marks_events_and_gone() {
    let (host, pool, token) = serve().await;
    let info = pool
        .create(&ada(), "counter", "proj", "", Value::Null)
        .unwrap();

    let (mut socket, _) =
        tokio_tungstenite::connect_async(format!("ws://{host}/api/ws?token={token}"))
            .await
            .expect("upgrade");
    socket
        .send(Message::text(
            json!({"op": "watch", "id": info.id}).to_string(),
        ))
        .await
        .unwrap();

    // A kind-state write bumps: the watched id gets a coalesced mark.
    pool.call(&ada(), &info.id, "incr", Value::Null)
        .await
        .unwrap();
    let mark = frame_where(&mut socket, |f| {
        f["t"] == "mark" && f["id"] == json!(info.id)
    })
    .await;
    assert!(mark["watermark"].as_u64().unwrap() >= 1);

    // The global feed rides the same socket: a *new* instance's created
    // event arrives without any subscription.
    let other = pool
        .create(&ada(), "counter", "proj", "", Value::Null)
        .unwrap();
    let event = frame_where(&mut socket, |f| f["t"] == "event" && f["name"] == "created").await;
    assert_eq!(event["id"], json!(other.id));
    assert_eq!(event["data"]["creator"]["id"], "ada");

    // Removal: the watcher learns the instance is gone rather than hanging.
    pool.call(&ada(), &info.id, "sys.remove", Value::Null)
        .await
        .unwrap();
    frame_where(&mut socket, |f| {
        f["t"] == "gone" && f["id"] == json!(info.id)
    })
    .await;
}

#[tokio::test]
async fn the_socket_requires_a_live_token() {
    let (host, _pool, _token) = serve().await;
    let refused = tokio_tungstenite::connect_async(format!("ws://{host}/api/ws?token=nope")).await;
    assert!(refused.is_err(), "a bad token must not upgrade");
}

#[tokio::test]
async fn long_poll_resolves_on_change_and_on_timeout() {
    let (router, pool, token) = common::counter_app();
    let info = pool
        .create(&ada(), "counter", "proj", "", Value::Null)
        .unwrap();

    // Park a long poll, then change the instance: the poll resolves with
    // the new watermark.
    let parked = {
        use tower::ServiceExt as _;
        let router = router.clone();
        let uri = format!(
            "/api/instances/{}/changed?since=0&timeout_ms=5000&token={token}",
            info.id
        );
        tokio::spawn(async move {
            router
                .oneshot(
                    axum::http::Request::builder()
                        .uri(uri)
                        .body(axum::body::Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap()
        })
    };
    tokio::time::sleep(Duration::from_millis(50)).await;
    pool.call(&ada(), &info.id, "incr", Value::Null)
        .await
        .unwrap();
    let response = parked.await.unwrap();
    assert_eq!(response.status(), 200);
    let bytes = http_body_util::BodyExt::collect(response.into_body())
        .await
        .unwrap()
        .to_bytes();
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    assert!(body["watermark"].as_u64().unwrap() >= 1);

    // At the timeout the current value comes back — the caller re-reads
    // and re-polls; a quiet instance is not an error.
    use tower::ServiceExt as _;
    let current = body["watermark"].as_u64().unwrap();
    let response = router
        .oneshot(
            axum::http::Request::builder()
                .uri(format!(
                    "/api/instances/{}/changed?since={current}&timeout_ms=100&token={token}",
                    info.id
                ))
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let bytes = http_body_util::BodyExt::collect(response.into_body())
        .await
        .unwrap()
        .to_bytes();
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body["watermark"].as_u64().unwrap(), current);
}
