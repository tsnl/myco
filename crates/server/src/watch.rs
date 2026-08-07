//! Observation over the wire, honoring DP-2's transport-agnostic framing:
//! the protocol is *logical channels* — verbs (the gateway), one event/watch
//! stream (here), media flows (later) — and the carrier maps them. Today the
//! carrier is one WebSocket multiplexing marks and events, plus a long-poll
//! for clients that want no socket at all. A WebTransport carrier later maps
//! the same channels to streams and datagrams; the frames don't change.
//!
//! The feed doctrine applies verbatim (DESIGN.md): events are best-effort,
//! marks are level-triggered. A lagging connection gets a `lagged` frame and
//! is expected to re-`list` and re-read — the wire never pretends the feed
//! is complete.

use std::collections::HashMap;

use axum::Json;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::Response;
use futures::{SinkExt as _, StreamExt as _};
use serde_json::{Value, json};

use crate::{App, Caller, refusal};
use myco_instance::VerbError;

/// Cap on a single long-poll; a client wanting longer just re-polls, so a
/// dead client cannot park a request forever.
const MAX_LONG_POLL_MS: u64 = 120_000;

#[derive(serde::Deserialize)]
pub(crate) struct ChangedQuery {
    since: Option<u64>,
    timeout_ms: Option<u64>,
}

/// Long-poll `changed`: resolves when the instance's watermark exceeds
/// `since`, or at the timeout with the current value — the caller re-reads
/// either way, which is the whole watermark contract.
pub(crate) async fn changed(
    State(app): State<App>,
    _caller: Caller,
    Path(id): Path<String>,
    Query(q): Query<ChangedQuery>,
) -> Result<Json<Value>, (StatusCode, Json<VerbError>)> {
    let since = q.since.unwrap_or(0);
    let wait =
        std::time::Duration::from_millis(q.timeout_ms.unwrap_or(30_000).min(MAX_LONG_POLL_MS));
    match tokio::time::timeout(wait, app.pool.changed(&id, since)).await {
        Ok(result) => result
            .map(|w| Json(json!({"watermark": w})))
            .map_err(refusal),
        Err(_elapsed) => {
            let w = app.pool.watermark(&id).map_err(refusal)?;
            Ok(Json(json!({"watermark": w})))
        }
    }
}

/// One multiplexed socket per client: subscribe to instance watermarks,
/// receive the global event feed. Frames, all JSON text:
///
/// - client → server: `{"op":"watch","id":…}` · `{"op":"unwatch","id":…}`
/// - server → client: `{"t":"mark","id":…,"watermark":n}` (coalesced;
///   re-read on receipt) · `{"t":"event","id":…,"name":…,"data":…}` ·
///   `{"t":"gone","id":…}` (watched instance removed/crashed) ·
///   `{"t":"lagged"}` (events were skipped — re-list and re-read)
pub(crate) async fn ws(
    State(app): State<App>,
    _caller: Caller,
    upgrade: WebSocketUpgrade,
) -> Response {
    upgrade.on_upgrade(move |socket| run(app, socket))
}

async fn run(app: App, socket: WebSocket) {
    let (mut sink, mut stream) = socket.split();
    // One outbound queue serializes marks and events onto the socket, so
    // watch tasks and the feed forwarder never contend for the sink.
    let (out, mut out_rx) = tokio::sync::mpsc::channel::<String>(256);

    // The global feed rides every connection: the tree UI's lifeline.
    let feed = tokio::spawn({
        let out = out.clone();
        let mut rx = app.pool.events();
        async move {
            loop {
                let frame = match rx.recv().await {
                    Ok((id, event)) => {
                        json!({"t": "event", "id": id, "name": event.name, "data": event.data})
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        json!({"t": "lagged"})
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                };
                if out.send(frame.to_string()).await.is_err() {
                    break;
                }
            }
        }
    });

    let mut watches: HashMap<String, tokio::task::JoinHandle<()>> = HashMap::new();
    loop {
        tokio::select! {
            incoming = stream.next() => {
                let text = match incoming {
                    Some(Ok(Message::Text(text))) => text,
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Err(_)) => break,
                    Some(Ok(_)) => continue,
                };
                let Ok(op) = serde_json::from_str::<Value>(text.as_str()) else {
                    continue;
                };
                let id = op["id"].as_str().unwrap_or_default().to_string();
                match op["op"].as_str() {
                    Some("watch") if !id.is_empty() => {
                        watches.entry(id.clone()).or_insert_with(|| {
                            tokio::spawn(watch_one(app.pool.clone(), id, out.clone()))
                        });
                    }
                    Some("unwatch") => {
                        if let Some(task) = watches.remove(&id) {
                            task.abort();
                        }
                    }
                    _ => {}
                }
            }
            frame = out_rx.recv() => {
                match frame {
                    Some(f) => {
                        if sink.send(Message::Text(f.into())).await.is_err() {
                            break;
                        }
                    }
                    None => break,
                }
            }
        }
    }

    feed.abort();
    for (_, task) in watches {
        task.abort();
    }
}

/// One watched instance: level-triggered marks until it is gone. Starting
/// from 0 means a client watching an already-active instance gets a first
/// mark immediately — no re-subscribe race, per the watermark contract.
async fn watch_one(pool: myco_instance::Pool, id: String, out: tokio::sync::mpsc::Sender<String>) {
    let mut seen = 0;
    loop {
        match pool.changed(&id, seen).await {
            Ok(watermark) => {
                seen = watermark;
                let frame = json!({"t": "mark", "id": id, "watermark": watermark});
                if out.send(frame.to_string()).await.is_err() {
                    return;
                }
            }
            Err(_) => {
                let _ = out.send(json!({"t": "gone", "id": id}).to_string()).await;
                return;
            }
        }
    }
}
