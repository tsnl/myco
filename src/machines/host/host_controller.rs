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
use crate::machines::host::HostWorker;
use crate::machines::host::protocol::{Request, Response};
use crate::machines::tool_services::{
    BashService, HostDispatchContext, ShellLock, ShellOverview, ShellScreen, ShellTail,
};
use crate::models::ToolSpec;
use myco_api::{ToolResult, ToolUse};

pub use crate::config::harness::HostConfig;

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
    pub fn local_in_process(max_image_base64_bytes: u64) -> Arc<Self> {
        Self::in_process(
            "local",
            Arc::new(HostWorker::standard("local", max_image_base64_bytes)),
        )
    }

    /// Create a remote/subprocess controller. The worker is **not** started until
    /// the first [`call`].
    pub fn new(config: HostConfig, max_image_base64_bytes: u64) -> Arc<Self> {
        Self::with_timeout(config, 10, max_image_base64_bytes)
    }

    /// Like [`new`] with an explicit connect timeout (`0` disables it) and the
    /// image cap the remote is spawned with — advertised before connecting, so
    /// it must match the `--max-image-base64-bytes` in `config.command`.
    pub fn with_timeout(
        config: HostConfig,
        connect_timeout_secs: u64,
        max_image_base64_bytes: u64,
    ) -> Arc<Self> {
        let name = config.name.clone();
        Arc::new(Self {
            name,
            next_id: AtomicU64::new(1),
            tools: HostWorker::standard_tool_specs(max_image_base64_bytes),
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

    /// One-line summaries of tool work still running for `agent_id` on this
    /// host (e.g. live bash sessions).
    ///
    /// In-process hosts only: the NDJSON protocol has no query message, and
    /// prompt-time display must never block on a lazy/dead remote connection,
    /// so subprocess hosts report none.
    pub fn running_tool_summaries(&self, agent_id: uuid::Uuid) -> Vec<String> {
        match &self.backend {
            Backend::InProcess { worker } => worker.running_tool_summaries(agent_id),
            Backend::Subprocess { .. } => Vec::new(),
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
        tool_use: ToolUse,
        cancel: CancelToken,
    ) -> ToolResult {
        match &self.backend {
            Backend::InProcess { worker } => {
                let tool_id = tool_use.id.clone();
                let worker = Arc::clone(worker);
                worker
                    .dispatch_tool_use(tool_use, HostDispatchContext { agent_id, cancel })
                    .await
                    .with_id(tool_id)
            }
            Backend::Subprocess { .. } => self.call_subprocess(agent_id, tool_use, cancel).await,
        }
    }

    async fn call_subprocess(
        &self,
        agent_id: uuid::Uuid,
        tool_use: ToolUse,
        cancel: CancelToken,
    ) -> ToolResult {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed).to_string();
        let tool_id = tool_use.id.clone();

        let request = Request::ToolCall {
            id: id.clone(),
            agent_id,
            tool_use,
        };

        let rx = match self.submit(&id, &request, true).await {
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

        // `run_reader` routes a reply to the waiter registered under exactly
        // its id, so a delivered response is always this call's ToolResult or
        // Error — never another call's reply and never a hello.
        match reply {
            Ok(Response::ToolResult { result, .. }) => {
                let mut result = result;
                if result.id.is_empty() {
                    result.id = tool_id;
                }
                result
            }
            Ok(Response::Error { message, .. }) => {
                ToolResult::err(format!("host {:?}: {message}", self.name)).with_id(tool_id)
            }
            Ok(other) => ToolResult::err(format!(
                "host {:?}: mismatched reply to a tool call: {other:?}",
                self.name
            ))
            .with_id(tool_id),
            Err(_closed) => {
                ToolResult::err(format!("host {:?}: connection closed", self.name)).with_id(tool_id)
            }
        }
    }

    /// Notify the worker that an agent session ended (reap sessions, …).
    ///
    /// In-process: runs immediately. Subprocess: fire-and-forget — the worker
    /// reaps without replying, and a missing/dead connection is a quiet no-op
    /// (worker process exit when the connection drops is the hard guarantee).
    pub async fn agent_finished(&self, agent_id: uuid::Uuid) -> Result<(), String> {
        match &self.backend {
            Backend::InProcess { worker } => {
                worker.notify_agent_finished(agent_id);
                Ok(())
            }
            Backend::Subprocess { conn, .. } => {
                let write_tx = {
                    let slot = conn.lock().await;
                    match slot.as_ref().filter(|c| !c.dead.load(Ordering::SeqCst)) {
                        Some(c) => c.write_tx.clone(),
                        // Not connected: nothing to reap on the worker.
                        None => return Ok(()),
                    }
                };
                let bytes = Request::AgentFinished { agent_id }.encode()?;
                write_tx
                    .send(bytes)
                    .await
                    .map_err(|_| format!("host {:?}: write: connection closed", self.name))
            }
        }
    }

    /// Register waiter + enqueue request, spawning the worker if `conn` is
    /// `None` (or only holds a dead connection) and `connect` allows it.
    /// Observer requests pass `connect: false`: a host nobody is using has no
    /// shells, and a 1 Hz rail poll must never be what spawns SSH.
    ///
    /// The connection mutex is held to (re)connect and clone handles — never
    /// across the channel send — so a wedged host cannot block sibling submits
    /// or cancels on this controller.
    async fn submit(
        &self,
        id: &str,
        request: &Request,
        connect: bool,
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
                if !connect {
                    return Err("host not connected".into());
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

    // --- shell observer surface ------------------------------------------
    //
    // The same watch/type/lock/screenshot surface the local host serves,
    // reached over the protocol for subprocess hosts. Reads never spawn a
    // connection: a host that is not connected has no shells to show.

    /// Live shells on this host. Not connected ⇒ empty, not an error — the
    /// rail lists what exists, and nothing exists on a dormant host.
    pub async fn shell_overviews(&self) -> Result<Vec<ShellOverview>, String> {
        match &self.backend {
            Backend::InProcess { worker } => Ok(worker
                .bash()
                .map(|b| b.shell_overviews())
                .unwrap_or_default()),
            Backend::Subprocess { .. } => {
                if !self.is_connected() {
                    return Ok(Vec::new());
                }
                let id = self.next_id.fetch_add(1, Ordering::Relaxed).to_string();
                match self
                    .shell_request(&id, &Request::ShellList { id: id.clone() })
                    .await?
                {
                    Response::Shells { shells, .. } => Ok(shells),
                    other => Err(format!("mismatched reply: {other:?}")),
                }
            }
        }
    }

    /// Non-consuming scrollback tail (see [`crate::machines::tool_services::BashService::shell_tail`]).
    pub async fn shell_tail(
        &self,
        shell: &str,
        from: u64,
        max_bytes: usize,
    ) -> Result<ShellTail, String> {
        match &self.backend {
            Backend::InProcess { worker } => {
                self.bash_of(worker)?.shell_tail(shell, from, max_bytes)
            }
            Backend::Subprocess { .. } => {
                let id = self.next_id.fetch_add(1, Ordering::Relaxed).to_string();
                let request = Request::ShellTail {
                    id: id.clone(),
                    shell: shell.to_string(),
                    from,
                    max_bytes: max_bytes as u64,
                };
                match self.shell_request(&id, &request).await? {
                    Response::ShellTail {
                        from,
                        end,
                        data,
                        running,
                        lock,
                        ..
                    } => Ok(ShellTail {
                        from,
                        end,
                        data: data.into_bytes(),
                        running,
                        lock,
                    }),
                    other => Err(format!("mismatched reply: {other:?}")),
                }
            }
        }
    }

    /// A user keystroke line (requires the user keyboard lock on the host).
    pub async fn shell_input(&self, shell: &str, data: &str) -> Result<ShellOverview, String> {
        match &self.backend {
            Backend::InProcess { worker } => {
                let bash = self.bash_of(worker)?;
                bash.shell_user_write(shell, data).await?;
                bash.shell_overview(shell)
            }
            Backend::Subprocess { .. } => {
                let id = self.next_id.fetch_add(1, Ordering::Relaxed).to_string();
                let request = Request::ShellInput {
                    id: id.clone(),
                    shell: shell.to_string(),
                    data: data.to_string(),
                };
                match self.shell_request(&id, &request).await? {
                    Response::Shell { shell, .. } => Ok(shell),
                    other => Err(format!("mismatched reply: {other:?}")),
                }
            }
        }
    }

    /// Move the keyboard; returns `(previous, fresh overview)` so a caller
    /// can tell a transition from an idempotent re-take.
    pub async fn shell_lock(
        &self,
        shell: &str,
        lock: ShellLock,
    ) -> Result<(ShellLock, ShellOverview), String> {
        match &self.backend {
            Backend::InProcess { worker } => {
                let bash = self.bash_of(worker)?;
                let previous = bash.shell_set_lock(shell, lock)?;
                Ok((previous, bash.shell_overview(shell)?))
            }
            Backend::Subprocess { .. } => {
                let id = self.next_id.fetch_add(1, Ordering::Relaxed).to_string();
                let request = Request::ShellLock {
                    id: id.clone(),
                    shell: shell.to_string(),
                    lock,
                };
                match self.shell_request(&id, &request).await? {
                    Response::Shell {
                        shell,
                        previous_lock,
                        ..
                    } => Ok((previous_lock.unwrap_or(shell.lock), shell)),
                    other => Err(format!("mismatched reply: {other:?}")),
                }
            }
        }
    }

    /// The shell's rendered terminal screen.
    pub async fn shell_screen(&self, shell: &str) -> Result<ShellScreen, String> {
        match &self.backend {
            Backend::InProcess { worker } => self.bash_of(worker)?.shell_screen(shell),
            Backend::Subprocess { .. } => {
                let id = self.next_id.fetch_add(1, Ordering::Relaxed).to_string();
                let request = Request::ShellScreenshot {
                    id: id.clone(),
                    shell: shell.to_string(),
                };
                match self.shell_request(&id, &request).await? {
                    Response::ShellScreen { screen, .. } => Ok(screen),
                    other => Err(format!("mismatched reply: {other:?}")),
                }
            }
        }
    }

    fn bash_of<'w>(&self, worker: &'w Arc<HostWorker>) -> Result<&'w Arc<BashService>, String> {
        worker
            .bash()
            .ok_or_else(|| format!("host {:?} has no bash service", self.name))
    }

    /// Submit an observer request (never connecting) and await its routed
    /// reply, unwrapping worker-side errors.
    async fn shell_request(&self, id: &str, request: &Request) -> Result<Response, String> {
        let rx = self.submit(id, request, false).await?;
        match rx.await {
            Ok(Response::Error { message, .. }) => Err(message),
            Ok(resp) => Ok(resp),
            Err(_closed) => Err("host connection closed".into()),
        }
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
        0 => fut.await,
        secs => match tokio::time::timeout(Duration::from_secs(secs), fut).await {
            Ok(r) => r,
            Err(_) => Err(format!("connect timed out after {secs}s")),
        },
    }
}

async fn connect(config: &HostConfig) -> Result<Conn, String> {
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

    let version = match reply {
        Response::HelloOk { version } => version,
        Response::Error { message, .. } => {
            let _ = child.start_kill();
            return Err(format!("hello error: {message}"));
        }
        other => {
            let _ = child.start_kill();
            return Err(format!("unexpected hello reply: {other:?}"));
        }
    };

    // Same-version lockstep: the assumed tool catalog and the NDJSON protocol
    // are only sound when both ends run the same build, so skew is a connect
    // error — not a latent tool failure hours later.
    let local_version = env!("CARGO_PKG_VERSION");
    if version != local_version {
        let _ = child.start_kill();
        return Err(format!(
            "remote myco {version} does not match local {local_version}; \
             rebuild myco on host {:?} (see harness-ops.md in the manual)",
            config.name
        ));
    }

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
    Ok(conn)
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

        if let Some(id) = msg.correlation_id().map(str::to_string) {
            let mut pending = pending.lock().await;
            if let Some(tx) = pending.remove(&id) {
                let _ = tx.send(msg);
            }
        } else if let Response::Error { id: None, message } = &msg {
            break format!("host protocol error: {message}");
        }
        // Anything else unaddressed (a stray hello) is noise, not a reason to
        // poison the pipe.
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

    /// Hosts under test serve the standard catalog at the resolved default;
    /// these tests are about routing and connection state, not the cap.
    const TEST_IMAGE_CAP: u64 = crate::config::DEFAULT_MAX_IMAGE_BASE64_BYTES;
    use crate::test_support::text_parts;
    use myco_api::ToolUse;
    use serde_json::json;
    use std::time::{Duration, Instant};

    /// Subprocess host whose worker is `bash -c <script>` (scripted hellos,
    /// deliberate deaths — no real myco binary needed).
    fn scripted_host(name: &str, script: String) -> Arc<HostController> {
        HostController::new(
            HostConfig {
                name: name.into(),
                command: vec!["bash".into(), "-c".into(), script],
            },
            TEST_IMAGE_CAP,
        )
    }

    /// One `hello_ok` NDJSON line claiming `version`.
    fn hello_line(version: &str) -> String {
        format!("{{\"type\":\"hello_ok\",\"version\":\"{version}\"}}")
    }

    async fn bash_call(ctl: &HostController, id: &str, command: &str) -> ToolResult {
        ctl.call(
            uuid::Uuid::nil(),
            ToolUse {
                id: id.into(),
                name: "bash".into(),
                input: json!({"command": command}),
            },
            CancelToken::new(),
        )
        .await
    }

    #[tokio::test]
    async fn in_process_local_host_is_always_connected() {
        let ctl = HostController::local_in_process(TEST_IMAGE_CAP);

        assert!(ctl.is_in_process());
        assert!(ctl.is_connected());
        assert!(
            ctl.tool_specs().iter().any(|t| t.name == "bash"),
            "expected bash tool from standard catalog"
        );

        let result = bash_call(&ctl, "t1", "printf 'hello-host\\n'").await;
        assert!(!result.is_error, "{result:?}");
        assert!(
            text_parts(&result).join("").contains("hello-host"),
            "{result:?}"
        );
        assert!(ctl.is_connected());
    }

    /// Two calls on one host must run as overlapping intervals, not back to
    /// back. Each command marks its own start on disk, sleeps, then reports
    /// whether its sibling's mark was already there when it woke: both reports
    /// positive means each call started before the other ended. Serial dispatch
    /// cannot produce that — whichever ran first would find no sibling mark.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn concurrent_calls_pipeline_in_process() {
        let ctl = HostController::local_in_process(TEST_IMAGE_CAP);
        let tmp = crate::test_support::temp_dir("host-concurrent");
        let dir = tmp.path().display();

        // The sleep is the overlap window: a sibling dispatched alongside us has
        // marked its start long before we look, however loaded the machine is.
        let script = |mine: &str, sibling: &str, marker: &str| {
            format!(
                "touch '{dir}/{mine}'; sleep 1; \
                 if [ -e '{dir}/{sibling}' ]; then echo SAW-SIBLING; fi; echo {marker}"
            )
        };

        let (script_a, script_b) = (script("a", "b", "AAA"), script("b", "a", "BBB"));

        let t0 = Instant::now();
        let a = bash_call(&ctl, "a", &script_a);
        let b = bash_call(&ctl, "b", &script_b);

        let (ra, rb) = tokio::time::timeout(Duration::from_secs(15), async { tokio::join!(a, b) })
            .await
            .expect("concurrent host calls hung");

        let wall = t0.elapsed();
        assert!(!ra.is_error, "a: {ra:?}");
        assert!(!rb.is_error, "b: {rb:?}");
        let out_a = text_parts(&ra).join("");
        let out_b = text_parts(&rb).join("");
        assert!(out_a.contains("AAA"), "{ra:?}");
        assert!(out_b.contains("BBB"), "{rb:?}");

        assert!(
            out_a.contains("SAW-SIBLING") && out_b.contains("SAW-SIBLING"),
            "expected overlapping execution: a={out_a:?} b={out_b:?}"
        );

        // Interval overlap above is the real concurrency signal. Wall clock is
        // only a coarse guard against fully serial execution; allow large slack
        // for CI / parallel suite load (scheduler jitter, other tests).
        assert!(
            wall < Duration::from_secs(8),
            "expected concurrent wall time ~1s, got {wall:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancel_midcall_does_not_wedge_next_call_in_process() {
        let ctl = HostController::local_in_process(TEST_IMAGE_CAP);

        let cancel = CancelToken::new();
        // Cancel from the same task after a short delay — more reliable than a
        // background spawn under heavy suite load / current_thread runtimes.
        let mut call = std::pin::pin!(ctl.call(
            uuid::Uuid::nil(),
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
            text_parts(&cancelled).join("").contains("cancelled"),
            "expected cancelled result, got: {cancelled:?}"
        );

        // Next call must not hang: cancel cleanup must free the host path.
        let result = tokio::time::timeout(
            Duration::from_secs(30),
            bash_call(&ctl, "next", "echo hello-after-cancel"),
        )
        .await
        .expect("next call timed out");
        assert!(!result.is_error, "{result:?}");
        assert!(
            text_parts(&result).join("").contains("hello-after-cancel"),
            "{result:?}"
        );
    }

    /// A host that dies after hello must *fail* calls, never hang them: the
    /// reader's exit poisons the connection, and the next call drops the dead
    /// conn and respawns instead of registering a waiter nobody will answer.
    /// A remote answering hello with a different package version must fail
    /// the connect with an actionable error, not limp along on an assumed
    /// tool catalog that may no longer match.
    #[tokio::test]
    async fn version_skew_fails_connect_with_actionable_error() {
        let ctl = scripted_host(
            "skewed",
            format!(
                "read -r _line; printf '%s\\n' '{}'; sleep 5",
                hello_line("0.0.1")
            ),
        );
        let result = tokio::time::timeout(Duration::from_secs(5), bash_call(&ctl, "t", "true"))
            .await
            .expect("skewed connect must fail fast");
        assert!(result.is_error, "{result:?}");
        let text = text_parts(&result).join("");
        assert!(text.contains("0.0.1"), "{text}");
        assert!(text.contains(env!("CARGO_PKG_VERSION")), "{text}");
        assert!(text.contains("rebuild"), "{text}");
    }

    #[tokio::test]
    async fn dead_host_fails_calls_and_respawns_instead_of_hanging() {
        // Answers the hello handshake, then exits immediately.
        let ctl = scripted_host(
            "dies",
            format!(
                "read -r _line; printf '%s\\n' '{}'",
                hello_line(env!("CARGO_PKG_VERSION"))
            ),
        );

        for attempt in 0..2 {
            let result = tokio::time::timeout(
                Duration::from_secs(5),
                bash_call(&ctl, &format!("t{attempt}"), "echo hi"),
            )
            .await
            .expect("call against dead host must fail fast, not hang");
            assert!(result.is_error, "attempt {attempt}: {result:?}");
        }
    }

    #[tokio::test]
    async fn subprocess_host_still_lazy_connects() {
        // Still supported for remotes / tests that force a local subprocess.
        let ctl = HostController::new(
            HostConfig {
                name: "sub".into(),
                command: crate::machines::harness::default_local_host_command(),
            },
            TEST_IMAGE_CAP,
        );

        assert!(!ctl.is_connected());
        let result = bash_call(&ctl, "t1", "printf 'via-sub\\n'").await;
        assert!(!result.is_error, "{result:?}");
        assert!(
            text_parts(&result).join("").contains("via-sub"),
            "{result:?}"
        );
        assert!(ctl.is_connected());
    }
}
