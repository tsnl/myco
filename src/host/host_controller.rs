//! Agent-side controller for one host.
//!
//! Two backends:
//! - **In-process** ([`HostController::in_process`]): shares an in-memory
//!   [`HostWorker`] with the agent process (used for the always-on `local` host).
//! - **Subprocess** ([`HostController::with_timeout`]): lazy-spawn a remote
//!   `myco --mode host` (typically over SSH) and pipeline NDJSON calls.
//!
//! Concurrent `call`s share one pipe (subprocess) or the same worker (in-process).

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout};
use tokio::sync::{Mutex, mpsc, oneshot};

use crate::core::CancelToken;
use crate::generative_model::{ToolResult, ToolSpec, ToolUse};
use crate::host::HostWorker;
use crate::host::protocol::{Request, Response};
use crate::tool_services::HostDispatchContext;

/// Configuration for one remote host endpoint (spawn argv).
///
/// Local is never described by this type — use [`HostController::in_process`].
#[derive(Debug, Clone)]
pub struct HostConfig {
    pub name: String,
    /// argv to spawn (e.g. `["ssh", "-o", "BatchMode=yes", "devbox", "myco", "--mode", "host", "--name", "devbox"]`).
    pub command: Vec<String>,
    /// SSH destination alias/hostname when this host is SSH-backed (for preflight).
    pub ssh_destination: Option<String>,
}

/// How the controller talks to its worker.
#[allow(clippy::large_enum_variant)] // Subprocess carries Conn state; InProcess is tiny.
enum Backend {
    /// Always-ready in-process worker (no child, no NDJSON).
    InProcess { worker: Arc<HostWorker> },
    /// Lazy subprocess over NDJSON stdio.
    Subprocess {
        config: HostConfig,
        conn: Mutex<Option<Conn>>,
        connect_timeout_secs: u64,
        last_error: StdMutex<Option<String>>,
    },
}

/// Controller for one host: in-process local, or lazy remote subprocess.
pub struct HostController {
    pub name: String,
    next_id: AtomicU64,
    /// Assumed tool catalog (`myco --mode host` standard set).
    tools: Vec<ToolSpec>,
    backend: Backend,
}

/// Live child + demux state. Drop aborts I/O tasks and kills the child.
struct Conn {
    child: Child,
    write_tx: mpsc::Sender<Vec<u8>>,
    pending: Arc<Mutex<HashMap<String, oneshot::Sender<Response>>>>,
    dead: Arc<AtomicBool>,
    reader_abort: tokio::task::AbortHandle,
    writer_abort: tokio::task::AbortHandle,
}

impl Drop for Conn {
    fn drop(&mut self) {
        self.reader_abort.abort();
        self.writer_abort.abort();
        let _ = self.child.start_kill();
        if let Ok(mut pending) = self.pending.try_lock() {
            for (_, tx) in pending.drain() {
                let _ = tx.send(Response::Error {
                    id: None,
                    message: "host connection closed".into(),
                });
            }
        }
    }
}

impl HostController {
    /// Always-on local host: tools run in-process via `worker`.
    pub fn in_process(name: impl Into<String>, worker: Arc<HostWorker>) -> Arc<Self> {
        let name = name.into();
        Arc::new(Self {
            name,
            next_id: AtomicU64::new(1),
            tools: worker.tool_specs(),
            backend: Backend::InProcess { worker },
        })
    }

    /// Convenience: standard bash + editor worker named `"local"`.
    pub fn local_in_process() -> Arc<Self> {
        Self::in_process("local", Arc::new(HostWorker::standard("local")))
    }

    /// Create a remote/subprocess controller. The worker is **not** started until
    /// the first [`call`].
    pub fn new(config: HostConfig) -> Arc<Self> {
        Self::with_timeout(config, 10)
    }

    /// Like [`new`] with an explicit connect timeout (`0` disables it).
    pub fn with_timeout(config: HostConfig, connect_timeout_secs: u64) -> Arc<Self> {
        let name = config.name.clone();
        Arc::new(Self {
            name,
            next_id: AtomicU64::new(1),
            tools: HostWorker::standard_tool_specs(),
            backend: Backend::Subprocess {
                config,
                conn: Mutex::new(None),
                connect_timeout_secs,
                last_error: StdMutex::new(None),
            },
        })
    }

    pub fn tool_specs(&self) -> &[ToolSpec] {
        &self.tools
    }

    /// Whether this host is in-process (always "connected").
    pub fn is_in_process(&self) -> bool {
        matches!(self.backend, Backend::InProcess { .. })
    }

    /// Whether a live worker connection is currently held.
    ///
    /// In-process hosts are always connected.
    pub fn is_connected(&self) -> bool {
        match &self.backend {
            Backend::InProcess { .. } => true,
            Backend::Subprocess { conn, .. } => {
                conn.try_lock().map(|g| g.is_some()).unwrap_or(false)
            }
        }
    }

    /// Last connect failure, if any (cleared after a successful connect).
    /// Always `None` for in-process hosts.
    pub fn last_error(&self) -> Option<String> {
        match &self.backend {
            Backend::InProcess { .. } => None,
            Backend::Subprocess { last_error, .. } => {
                last_error.lock().ok().and_then(|g| g.clone())
            }
        }
    }

    /// Fire a tool call and await its demuxed reply.
    ///
    /// **In-process:** cancel is delivered only via [`HostDispatchContext`] so the
    /// tool can kill children and return. We deliberately do **not**
    /// `select!`-abandon the dispatch future — that leaked `sleep`/pipe work and
    /// wedged later calls under suite load. Tools that ignore cancel may run to
    /// completion (or their own timeout).
    ///
    /// **Subprocess:** cancel abandons this waiter only (host may still finish
    /// the tool). Connect happens on first use; concurrent callers only serialize
    /// briefly in [`submit`].
    pub async fn call(
        &self,
        agent_id: uuid::Uuid,
        session_id: Option<String>,
        tool_use: ToolUse,
        cancel: CancelToken,
    ) -> ToolResult {
        self.call_with_root(agent_id, session_id, tool_use, cancel, None)
            .await
    }

    /// Like [`call`], optionally attaching agent-root handles for in-process tools
    /// (e.g. `subagent`). Remote backends ignore `agent_root`.
    pub async fn call_with_root(
        &self,
        agent_id: uuid::Uuid,
        session_id: Option<String>,
        tool_use: ToolUse,
        cancel: CancelToken,
        agent_root: Option<Arc<dyn std::any::Any + Send + Sync>>,
    ) -> ToolResult {
        match &self.backend {
            Backend::InProcess { worker } => {
                let tool_id = tool_use.id.clone();
                let worker = Arc::clone(worker);
                worker
                    .dispatch_tool_use(
                        tool_use,
                        HostDispatchContext {
                            agent_id,
                            session_id,
                            cancel,
                            agent_root,
                        },
                    )
                    .await
                    .with_id(tool_id)
            }
            Backend::Subprocess { .. } => {
                self.call_subprocess(agent_id, session_id, tool_use, cancel)
                    .await
            }
        }
    }

    async fn call_subprocess(
        &self,
        agent_id: uuid::Uuid,
        session_id: Option<String>,
        tool_use: ToolUse,
        cancel: CancelToken,
    ) -> ToolResult {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed).to_string();
        let tool_id = tool_use.id.clone();

        let request = Request::ToolCall {
            id: id.clone(),
            agent_id,
            session_id,
            tool_use,
        };

        let rx = match self.submit(&id, &request, /*connect_if_needed*/ true).await {
            Ok(rx) => rx,
            Err(e) => {
                return ToolResult::err(format!("host {:?}: {e}", self.name)).with_id(tool_id);
            }
        };

        let reply = tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                self.abandon(&id).await;
                return ToolResult::err("cancelled").with_id(tool_id);
            }
            r = rx => r,
        };

        match reply {
            Ok(Response::ToolResult {
                id: reply_id,
                result,
            }) => {
                if reply_id != id {
                    return ToolResult::err(format!(
                        "host {:?}: correlation mismatch (sent {id}, got {reply_id})",
                        self.name
                    ))
                    .with_id(tool_id);
                }
                let mut result = result;
                if result.id.is_empty() {
                    result.id = tool_id;
                }
                result
            }
            Ok(Response::Error { message, .. }) => {
                ToolResult::err(format!("host {:?}: {message}", self.name)).with_id(tool_id)
            }
            Ok(other) => {
                ToolResult::err(format!("host {:?}: unexpected reply: {other:?}", self.name))
                    .with_id(tool_id)
            }
            Err(_closed) => {
                ToolResult::err(format!("host {:?}: connection closed", self.name)).with_id(tool_id)
            }
        }
    }

    /// Notify the worker that an agent session ended (reap sessions, …).
    ///
    /// In-process: runs immediately. Subprocess: no-ops if never connected.
    pub async fn agent_finished(&self, agent_id: uuid::Uuid) -> Result<(), String> {
        match &self.backend {
            Backend::InProcess { worker } => {
                worker.notify_agent_finished(agent_id);
                Ok(())
            }
            Backend::Subprocess { .. } => {
                let id = self.next_id.fetch_add(1, Ordering::Relaxed).to_string();
                let key = format!("agent_finished:{id}");
                let request = Request::AgentFinished { agent_id };

                let rx = match self
                    .submit(&key, &request, /*connect_if_needed*/ false)
                    .await
                {
                    Ok(rx) => rx,
                    // Not connected yet: nothing to reap on the worker.
                    Err(e) if e == "not connected" => return Ok(()),
                    Err(e) => return Err(format!("host {:?}: {e}", self.name)),
                };

                match rx.await {
                    Ok(Response::AgentFinishedOk { .. }) => Ok(()),
                    Ok(Response::Error { message, .. }) => Err(format!(
                        "host {:?}: agent_finished error: {message}",
                        self.name
                    )),
                    Ok(other) => Err(format!(
                        "host {:?}: unexpected agent_finished reply: {other:?}",
                        self.name
                    )),
                    Err(_) => Err(format!(
                        "host {:?}: agent_finished: connection closed",
                        self.name
                    )),
                }
            }
        }
    }

    /// Register waiter + enqueue request.
    ///
    /// When `connect_if_needed`, spawns the worker if `conn` is `None` (or
    /// only holds a dead connection). The connection mutex is held to
    /// (re)connect and clone handles — never across the channel send — so a
    /// wedged host cannot block sibling submits or cancels on this controller.
    async fn submit(
        &self,
        id: &str,
        request: &Request,
        connect_if_needed: bool,
    ) -> Result<oneshot::Receiver<Response>, String> {
        let Backend::Subprocess {
            config,
            conn,
            connect_timeout_secs,
            last_error,
        } = &self.backend
        else {
            return Err("submit on in-process host".into());
        };

        let (write_tx, pending, dead) = {
            let mut slot = conn.lock().await;
            // A connection whose reader/writer exited (host died, protocol
            // desync) sits in the slot looking alive; drop it — Conn::drop
            // kills the child — so the path below can respawn cleanly.
            // Also poll the child: reader/writer tasks may not have run yet
            // after an immediate post-hello exit, so `dead` can lag.
            if slot.as_mut().is_some_and(|c| {
                c.dead.load(Ordering::SeqCst) || c.child.try_wait().ok().flatten().is_some()
            }) {
                *slot = None;
            }
            if slot.is_none() {
                if !connect_if_needed {
                    return Err("not connected".into());
                }
                match connect_with_timeout(config, *connect_timeout_secs).await {
                    Ok(c) => {
                        if let Ok(mut err) = last_error.lock() {
                            *err = None;
                        }
                        *slot = Some(c);
                    }
                    Err(e) => {
                        if let Ok(mut err) = last_error.lock() {
                            *err = Some(e.clone());
                        }
                        return Err(e);
                    }
                }
            }
            // Fresh connect can still race: host exits right after hello,
            // before the reader task observes EOF. Reject immediately rather
            // than registering a waiter nobody will answer.
            if slot.as_mut().is_some_and(|c| {
                c.dead.load(Ordering::SeqCst) || c.child.try_wait().ok().flatten().is_some()
            }) {
                *slot = None;
                let msg = "host connection lost".to_string();
                if let Ok(mut err) = last_error.lock() {
                    *err = Some(msg.clone());
                }
                return Err(msg);
            }
            let c = slot.as_ref().expect("connected");
            (
                c.write_tx.clone(),
                Arc::clone(&c.pending),
                Arc::clone(&c.dead),
            )
        };

        let (tx, rx) = oneshot::channel();
        {
            // Checking `dead` under the pending lock pairs with the reader
            // setting `dead` *before* draining: a new waiter is either
            // rejected here or is already registered when the reader drains.
            // Either way nobody awaits a reply that can no longer come.
            let mut pending = pending.lock().await;
            if dead.load(Ordering::SeqCst) {
                return Err("host connection lost".into());
            }
            pending.insert(id.to_string(), tx);
        }

        let bytes = request.encode()?;
        if write_tx.send(bytes).await.is_err() {
            dead.store(true, Ordering::SeqCst);
            let mut pending = pending.lock().await;
            pending.remove(id);
            let msg = "write: connection closed".to_string();
            if let Ok(mut err) = last_error.lock() {
                *err = Some(msg.clone());
            }
            return Err(msg);
        }
        Ok(rx)
    }

    /// Best-effort removal of this call's waiter so cancel returns instantly.
    /// `try_lock` so cancel is never stuck behind an in-flight connect; a
    /// missed removal self-cleans when the reply arrives or the reader drains.
    async fn abandon(&self, id: &str) {
        let Backend::Subprocess { conn, .. } = &self.backend else {
            return;
        };
        let Ok(slot) = conn.try_lock() else {
            return;
        };
        if let Some(c) = slot.as_ref() {
            let mut pending = c.pending.lock().await;
            pending.remove(id);
        }
    }
}

impl Drop for HostController {
    fn drop(&mut self) {
        if let Backend::Subprocess { conn, .. } = &self.backend
            && let Ok(mut slot) = conn.try_lock()
        {
            *slot = None;
        }
    }
}

async fn connect_with_timeout(
    config: &HostConfig,
    connect_timeout_secs: u64,
) -> Result<Conn, String> {
    let fut = connect(config);
    match connect_timeout_secs {
        0 => fut.await.map(|(c, _, _)| c),
        secs => match tokio::time::timeout(Duration::from_secs(secs), fut).await {
            Ok(Ok((c, _, _))) => Ok(c),
            Ok(Err(e)) => Err(e),
            Err(_) => Err(format!("connect timed out after {secs}s")),
        },
    }
}

async fn connect(config: &HostConfig) -> Result<(Conn, String, Vec<ToolSpec>), String> {
    if config.command.is_empty() {
        return Err(format!("host {:?}: empty command", config.name));
    }
    let program = &config.command[0];
    let args = &config.command[1..];

    let mut child = tokio::process::Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("spawn {:?}: {e}", config.command))?;

    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| format!("host {:?}: missing stdin", config.name))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| format!("host {:?}: missing stdout", config.name))?;
    let mut stdout = BufReader::new(stdout);

    // Hello before demux tasks.
    let hello = Request::Hello.encode()?;
    write_all(&mut stdin, &hello).await?;
    let line = read_line(&mut stdout).await?;
    let reply = Response::decode(&line)?;

    let (name, tools) = match reply {
        Response::HelloOk { name, tools, .. } => (name, tools),
        Response::Error { message, .. } => {
            let _ = child.start_kill();
            return Err(format!("hello error: {message}"));
        }
        other => {
            let _ = child.start_kill();
            return Err(format!("unexpected hello reply: {other:?}"));
        }
    };

    let pending: Arc<Mutex<HashMap<String, oneshot::Sender<Response>>>> =
        Arc::new(Mutex::new(HashMap::new()));
    let pending_reader = Arc::clone(&pending);
    let pending_writer = Arc::clone(&pending);
    let dead = Arc::new(AtomicBool::new(false));
    let writer_dead = Arc::clone(&dead);
    let reader_dead = Arc::clone(&dead);
    let (write_tx, write_rx) = mpsc::channel::<Vec<u8>>(64);

    let writer = tokio::spawn(async move {
        run_writer(stdin, write_rx, pending_writer, writer_dead).await;
    });
    let reader = tokio::spawn(async move {
        run_reader(stdout, pending_reader, reader_dead).await;
    });

    let conn = Conn {
        child,
        write_tx,
        pending,
        dead,
        reader_abort: reader.abort_handle(),
        writer_abort: writer.abort_handle(),
    };
    Ok((conn, name, tools))
}

async fn write_all(w: &mut ChildStdin, bytes: &[u8]) -> Result<(), String> {
    w.write_all(bytes)
        .await
        .map_err(|e| format!("write: {e}"))?;
    w.flush().await.map_err(|e| format!("flush: {e}"))?;
    Ok(())
}

async fn read_line(r: &mut BufReader<ChildStdout>) -> Result<String, String> {
    let mut line = String::new();
    loop {
        line.clear();
        let n = r
            .read_line(&mut line)
            .await
            .map_err(|e| format!("read: {e}"))?;
        if n == 0 {
            return Err("peer closed".into());
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        return Ok(trimmed.to_string());
    }
}

async fn run_writer(
    mut stdin: ChildStdin,
    mut rx: mpsc::Receiver<Vec<u8>>,
    pending: Arc<Mutex<HashMap<String, oneshot::Sender<Response>>>>,
    dead: Arc<AtomicBool>,
) {
    let exit_message = loop {
        let Some(bytes) = rx.recv().await else {
            break "host write channel closed".to_string();
        };
        if write_all(&mut stdin, &bytes).await.is_err() {
            break "host write failed".to_string();
        }
    };
    // Same contract as run_reader: poison first, then drain. A waiter that
    // registered after the reader already drained (host died mid-hello) but
    // before this write failed must still be failed, not left hanging.
    dead.store(true, Ordering::SeqCst);
    let mut pending = pending.lock().await;
    for (_, tx) in pending.drain() {
        let _ = tx.send(Response::Error {
            id: None,
            message: exit_message.clone(),
        });
    }
}

/// Demux loop. Exits on EOF or on a connection-fatal `Error{id:None}` from
/// the worker (undecodable request — agent/worker version skew); either way
/// the connection is poisoned: `dead` is set *before* draining `pending`
/// (see `submit` for the pairing), then every waiter gets an error instead
/// of hanging on a reply that can no longer come.
async fn run_reader(
    mut stdout: BufReader<ChildStdout>,
    pending: Arc<Mutex<HashMap<String, oneshot::Sender<Response>>>>,
    dead: Arc<AtomicBool>,
) {
    let exit_message = loop {
        let line = match read_line(&mut stdout).await {
            Ok(l) => l,
            Err(_) => break "host closed stdout".to_string(),
        };
        let msg = match Response::decode(&line) {
            Ok(m) => m,
            Err(_) => continue,
        };

        match &msg {
            Response::ToolResult { id, .. } => {
                let mut pending = pending.lock().await;
                if let Some(tx) = pending.remove(id) {
                    let _ = tx.send(msg);
                }
            }
            Response::Error { id: Some(id), .. } => {
                let mut pending = pending.lock().await;
                if let Some(tx) = pending.remove(id) {
                    let _ = tx.send(msg);
                }
            }
            Response::AgentFinishedOk { .. } => {
                let mut pending = pending.lock().await;
                let key = pending
                    .keys()
                    .find(|k| k.starts_with("agent_finished:"))
                    .cloned();
                if let Some(k) = key
                    && let Some(tx) = pending.remove(&k)
                {
                    let _ = tx.send(msg);
                }
            }
            Response::Error { id: None, message } => {
                break format!("host protocol error: {message}");
            }
            Response::HelloOk { .. } => {}
        }
    };

    dead.store(true, Ordering::SeqCst);
    let mut pending = pending.lock().await;
    for (_, tx) in pending.drain() {
        let _ = tx.send(Response::Error {
            id: None,
            message: exit_message.clone(),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::generative_model::ToolUse;
    use serde_json::json;
    use std::time::{Duration, Instant};

    fn tool_text(result: &ToolResult) -> String {
        result
            .content
            .iter()
            .filter_map(|c| match c {
                crate::generative_model::Content::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("")
    }

    #[tokio::test]
    async fn in_process_local_host_is_always_connected() {
        let ctl = HostController::local_in_process();

        assert!(ctl.is_in_process());
        assert!(ctl.is_connected());
        assert!(
            ctl.tool_specs().iter().any(|t| t.name == "bash"),
            "expected bash tool from standard catalog"
        );

        let result = ctl
            .call(
                uuid::Uuid::nil(),
                None,
                ToolUse {
                    id: "t1".into(),
                    name: "bash".into(),
                    input: json!({"command": "printf 'hello-host\\n'"}),
                },
                CancelToken::new(),
            )
            .await;
        assert!(!result.is_error, "{result:?}");
        assert!(tool_text(&result).contains("hello-host"), "{result:?}");
        assert!(ctl.is_connected());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn concurrent_calls_pipeline_in_process() {
        let ctl = HostController::local_in_process();

        // Real sleeps must overlap: serial would be ~2s, concurrent ~1s (+ slack).
        let t0 = Instant::now();
        let a = ctl.call(
            uuid::Uuid::nil(),
            None,
            ToolUse {
                id: "a".into(),
                name: "bash".into(),
                input: json!({"command": "sleep 1; echo AAA", "timeout_ms": 10000}),
            },
            CancelToken::new(),
        );
        let b = ctl.call(
            uuid::Uuid::nil(),
            None,
            ToolUse {
                id: "b".into(),
                name: "bash".into(),
                input: json!({"command": "sleep 1; echo BBB", "timeout_ms": 10000}),
            },
            CancelToken::new(),
        );

        let (ra, rb) = tokio::time::timeout(Duration::from_secs(15), async { tokio::join!(a, b) })
            .await
            .expect("concurrent host calls hung");

        let wall = t0.elapsed();
        assert!(!ra.is_error, "a: {ra:?}");
        assert!(!rb.is_error, "b: {rb:?}");
        assert!(tool_text(&ra).contains("AAA"), "{ra:?}");
        assert!(tool_text(&rb).contains("BBB"), "{rb:?}");
        assert!(
            wall < Duration::from_millis(1700),
            "expected concurrent overlap (~1s), wall={wall:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancel_midcall_does_not_wedge_next_call_in_process() {
        let ctl = HostController::local_in_process();

        let cancel = CancelToken::new();
        // Cancel from the same task after a short delay — more reliable than a
        // background spawn under heavy suite load / current_thread runtimes.
        let mut call = std::pin::pin!(ctl.call(
            uuid::Uuid::nil(),
            None,
            ToolUse {
                id: "slow".into(),
                name: "bash".into(),
                // Long enough that cancel always races before natural exit.
                // Explicit timeout so we never lean on the 60s default.
                input: json!({
                    "command": "sleep 120; echo done-slow",
                    "timeout_ms": 180_000
                }),
            },
            cancel.clone(),
        ));
        let cancelled = tokio::select! {
            r = &mut call => r,
            _ = tokio::time::sleep(Duration::from_millis(400)) => {
                cancel.cancel();
                call.await
            }
        };
        assert!(
            cancel.is_cancelled(),
            "token should be cancelled after test cancel"
        );
        assert!(cancelled.is_error, "{cancelled:?}");
        assert!(
            tool_text(&cancelled).contains("cancelled"),
            "expected cancelled result, got: {cancelled:?}"
        );

        // Next call must not hang: cancel cleanup must free the host path.
        let result = tokio::time::timeout(
            Duration::from_secs(30),
            ctl.call(
                uuid::Uuid::nil(),
                None,
                ToolUse {
                    id: "next".into(),
                    name: "bash".into(),
                    input: json!({"command": "echo hello-after-cancel"}),
                },
                CancelToken::new(),
            ),
        )
        .await
        .expect("next call timed out");
        assert!(!result.is_error, "{result:?}");
        assert!(
            tool_text(&result).contains("hello-after-cancel"),
            "{result:?}"
        );
    }

    /// A host that dies after hello must *fail* calls, never hang them: the
    /// reader's exit poisons the connection, and the next call drops the dead
    /// conn and respawns instead of registering a waiter nobody will answer.
    #[tokio::test]
    async fn dead_host_fails_calls_and_respawns_instead_of_hanging() {
        let hello = "{\"type\":\"hello_ok\",\"name\":\"fake\",\"version\":\"0\",\"tools\":[]}";
        let ctl = HostController::new(HostConfig {
            name: "dies".into(),
            // Answers the hello handshake, then exits immediately.
            command: vec![
                "bash".into(),
                "-c".into(),
                format!("read -r _line; printf '%s\\n' '{hello}'"),
            ],
            ssh_destination: None,
        });

        for attempt in 0..2 {
            let result = tokio::time::timeout(
                Duration::from_secs(5),
                ctl.call(
                    uuid::Uuid::nil(),
                    None,
                    ToolUse {
                        id: format!("t{attempt}"),
                        name: "bash".into(),
                        input: json!({"command": "echo hi"}),
                    },
                    CancelToken::new(),
                ),
            )
            .await
            .expect("call against dead host must fail fast, not hang");
            assert!(result.is_error, "attempt {attempt}: {result:?}");
        }
    }

    #[tokio::test]
    async fn subprocess_host_still_lazy_connects() {
        // Still supported for remotes / tests that force a local subprocess.
        let ctl = HostController::new(HostConfig {
            name: "sub".into(),
            command: crate::harness::default_local_host_command(),
            ssh_destination: None,
        });

        assert!(!ctl.is_connected());
        let result = ctl
            .call(
                uuid::Uuid::nil(),
                None,
                ToolUse {
                    id: "t1".into(),
                    name: "bash".into(),
                    input: json!({"command": "printf 'via-sub\\n'"}),
                },
                CancelToken::new(),
            )
            .await;
        assert!(!result.is_error, "{result:?}");
        assert!(tool_text(&result).contains("via-sub"), "{result:?}");
        assert!(ctl.is_connected());
    }
}
