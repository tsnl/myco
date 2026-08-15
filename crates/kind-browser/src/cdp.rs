//! A minimal Chrome DevTools Protocol client: launch, one WebSocket,
//! request/response by id, events forwarded as bumps. Hand-rolled over
//! the workspace's existing WebSocket dep for the same reason the SSE
//! parser and the vt100 renderer are hand-rolled — the sliver of the
//! protocol this kind speaks is smaller than any client library's
//! surface, and owning the mechanism keeps the failure modes readable.
//!
//! The shape is the attach link's: verbs send commands through a
//! clonable handle and await a oneshot; one reader task resolves
//! replies and turns page events into watermark bumps. A dead socket
//! answers every pending and future command with an error naming it.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use futures::{SinkExt as _, StreamExt as _};
use myco_runtime::Signals;
use serde_json::{Value, json};
use tokio::io::AsyncBufReadExt as _;

/// How long a single CDP command may take before it answers as failed.
/// Navigation to a dead site can hang far longer; the timeout keeps the
/// instance's mailbox from wedging behind it.
const COMMAND_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

type Pending = Arc<Mutex<HashMap<u64, tokio::sync::oneshot::Sender<Result<Value, String>>>>>;

#[derive(Clone)]
pub struct Cdp {
    out: tokio::sync::mpsc::Sender<String>,
    pending: Pending,
    seq: Arc<AtomicU64>,
    /// The attached page session; commands carry it once set.
    session: Arc<Mutex<Option<String>>>,
}

impl Cdp {
    /// One command, session-scoped when a session exists.
    pub async fn cmd(&self, method: &str, params: Value) -> Result<Value, String> {
        let seq = self.seq.fetch_add(1, Ordering::Relaxed);
        let mut envelope = json!({ "id": seq, "method": method, "params": params });
        if let Some(session) = self
            .session
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
        {
            envelope["sessionId"] = Value::String(session);
        }
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(seq, tx);
        if self.out.send(envelope.to_string()).await.is_err() {
            self.pending
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&seq);
            return Err("the browser connection is closed".into());
        }
        match tokio::time::timeout(COMMAND_TIMEOUT, rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err("the browser connection closed mid-command".into()),
            Err(_) => {
                self.pending
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&seq);
                Err(format!("{method} took longer than the command timeout"))
            }
        }
    }

    fn set_session(&self, session: String) {
        *self.session.lock().unwrap_or_else(|e| e.into_inner()) = Some(session);
    }
}

/// Everything a live browser is: the process, the wire, the page target.
/// Dropping it kills the process (`kill_on_drop`) and ends the tasks.
pub struct Launched {
    pub cdp: Cdp,
    pub target_id: String,
    child: tokio::process::Child,
    reader: tokio::task::JoinHandle<()>,
    writer: tokio::task::JoinHandle<()>,
    /// The profile dir dies with the browser — a browser instance is a
    /// surface, not a place to accrete state.
    _profile: tempfile::TempDir,
}

impl Drop for Launched {
    fn drop(&mut self) {
        self.reader.abort();
        self.writer.abort();
        let _ = self.child.start_kill();
    }
}

/// Launch a browser, connect to its DevTools socket, attach to one page.
/// `signals` receives a bump for every page-level event — the pane's
/// screenshot re-reads ride on those.
pub async fn launch(
    browser: &str,
    extra_args: &[String],
    signals: Signals,
) -> Result<Launched, String> {
    let profile = tempfile::TempDir::new().map_err(|e| format!("profile dir: {e}"))?;
    let mut child = tokio::process::Command::new(browser)
        .arg("--headless=new")
        .arg("--remote-debugging-port=0")
        .arg("--no-first-run")
        .arg("--no-default-browser-check")
        .arg(format!("--user-data-dir={}", profile.path().display()))
        .args(extra_args)
        .arg("about:blank")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("spawn {browser}: {e}"))?;

    // The endpoint is announced on stderr: "DevTools listening on ws://…".
    let stderr = child.stderr.take().expect("stderr piped");
    let mut lines = tokio::io::BufReader::new(stderr).lines();
    let banner = tokio::time::timeout(std::time::Duration::from_secs(20), async {
        while let Ok(Some(line)) = lines.next_line().await {
            if let Some(rest) = line.strip_prefix("DevTools listening on ") {
                return Some(rest.trim().to_string());
            }
        }
        None
    })
    .await
    .map_err(|_| "the browser never announced its DevTools endpoint".to_string())?
    .ok_or_else(|| "the browser exited before announcing DevTools".to_string())?;
    // Keep draining stderr so the pipe cannot fill and stall the browser.
    tokio::spawn(async move { while let Ok(Some(_)) = lines.next_line().await {} });

    let (socket, _) = tokio_tungstenite::connect_async(&banner)
        .await
        .map_err(|e| format!("connect {banner}: {e}"))?;
    let (mut sink, mut stream) = socket.split();

    let (out, mut out_rx) = tokio::sync::mpsc::channel::<String>(64);
    let writer = tokio::spawn(async move {
        while let Some(text) = out_rx.recv().await {
            if sink
                .send(tokio_tungstenite::tungstenite::Message::text(text))
                .await
                .is_err()
            {
                break;
            }
        }
    });

    let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
    let reader = tokio::spawn({
        let pending = Arc::clone(&pending);
        async move {
            while let Some(Ok(message)) = stream.next().await {
                let Ok(text) = message.into_text() else {
                    continue;
                };
                let Ok(frame) = serde_json::from_str::<Value>(text.as_str()) else {
                    continue;
                };
                if let Some(id) = frame["id"].as_u64() {
                    let waiter = pending.lock().unwrap_or_else(|e| e.into_inner()).remove(&id);
                    if let Some(tx) = waiter {
                        let result = match frame.get("error") {
                            Some(err) => Err(err["message"]
                                .as_str()
                                .unwrap_or("browser error")
                                .to_string()),
                            None => Ok(frame["result"].clone()),
                        };
                        let _ = tx.send(result);
                    }
                } else if frame["method"]
                    .as_str()
                    .is_some_and(|m| m.starts_with("Page."))
                {
                    // Page events are the "something changed" signal; the
                    // watermark coalesces however many arrive.
                    signals.bump();
                }
            }
            // Socket gone: everything pending answers, nothing hangs.
            for (_, tx) in pending.lock().unwrap_or_else(|e| e.into_inner()).drain() {
                let _ = tx.send(Err("the browser connection closed".into()));
            }
        }
    });

    let cdp = Cdp {
        out,
        pending,
        seq: Arc::new(AtomicU64::new(1)),
        session: Arc::new(Mutex::new(None)),
    };

    // One page target, one flat session; every later command is scoped
    // to it.
    let created = cdp
        .cmd("Target.createTarget", json!({ "url": "about:blank" }))
        .await?;
    let target_id = created["targetId"]
        .as_str()
        .ok_or("no targetId from createTarget")?
        .to_string();
    let attached = cdp
        .cmd(
            "Target.attachToTarget",
            json!({ "targetId": target_id, "flatten": true }),
        )
        .await?;
    let session = attached["sessionId"]
        .as_str()
        .ok_or("no sessionId from attachToTarget")?
        .to_string();
    cdp.set_session(session);
    cdp.cmd("Page.enable", json!({})).await?;
    // A document handle makes backend-node lookups (click-by-ref) valid.
    cdp.cmd("DOM.getDocument", json!({})).await?;

    Ok(Launched {
        cdp,
        target_id,
        child,
        reader,
        writer,
        _profile: profile,
    })
}
