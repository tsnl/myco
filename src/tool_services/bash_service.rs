use std::collections::HashMap;
use std::os::unix::process::ExitStatusExt;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, ChildStdin};
use tokio::sync::Notify;

use super::*;

use uuid::Uuid;

/// Default hard wait ceiling for a single session start/write/read.
///
/// Sessions return early on idle (`idle_ms`) or byte cap; this is only the
/// outer ceiling when output keeps arriving (or never does). 30s is long enough
/// for common interactive waits without thrashing short polls.
const DEFAULT_TIMEOUT_MS: u64 = 30_000;
/// Safety ceiling on session `timeout_ms` (30 minutes). Explicit values above
/// this are rejected (not clamped). Cancel still aborts mid-wait.
const MAX_TIMEOUT_MS: u64 = 1_800_000;
/// Default wait for one-shot `exec` (`bash -c`) when `timeout_ms` is omitted.
/// Exec blocks until the process exits; 60s covers typical builds/tests without
/// requiring an explicit override.
const DEFAULT_EXEC_TIMEOUT_MS: u64 = 60_000;
/// Safety ceiling on exec `timeout_ms` (30 minutes). Requests above this are
/// rejected (not clamped). Cancel still kills the process group.
const MAX_EXEC_TIMEOUT_MS: u64 = 1_800_000;
/// Bound for a single stdin write into a live session (stuck pipe / non-reader).
/// Independent of the larger session `timeout_ms` ceiling.
const STDIN_WRITE_TIMEOUT_MS: u64 = 5_000;
/// Default "no new bytes for this long ⇒ done collecting".
const DEFAULT_IDLE_MS: u64 = 300;
/// Default max bytes returned per tool call: session drain cap and per-stream
/// exec cap. Bounds how much one bash call can put into model context.
const DEFAULT_MAX_BYTES: usize = 32_768;
/// Soft cap on concurrent sessions per harness.
const MAX_SESSIONS: usize = 8;

/// Executes bash commands on behalf of the agent.
///
/// Supports one-shot `bash -c` execution and long-lived interactive sessions
/// (Python REPLs, SSH, shells, …) addressed by agent-chosen `session_id`s.
///
/// Session model: each tool call is a bounded interaction against a live child
/// process — write optional stdin, then collect output until idle gap, hard
/// timeout, byte cap, or process exit.
pub struct BashService {
    sessions: Mutex<HashMap<String, Session>>,
    /// Test override for the exec dump location. `None` ⇒ the conversation
    /// session directory under this host's `~/.myco` (see [`Self::exec_dump`]).
    dump_root: Option<std::path::PathBuf>,
}

impl Default for BashService {
    fn default() -> Self {
        Self {
            sessions: Mutex::new(HashMap::new()),
            dump_root: None,
        }
    }
}

impl ToolService for BashService {
    fn tool_specs(&self) -> Vec<generative_model::ToolSpec> {
        vec![generative_model::ToolSpec {
            name: "bash".to_string(),
            description: format!(
                "Executes bash commands and manages long-lived interactive sessions \
                (shells, Python REPLs, SSH, etc.).\n\n\
                Actions:\n\
                - exec (default): one-shot `bash -c <command>`; **blocks until the process \
                exits** (or `timeout_ms`, default {DEFAULT_EXEC_TIMEOUT_MS} ms / \
                {exec_default_s}s; max {MAX_EXEC_TIMEOUT_MS} ms / {exec_max_min} min). \
                Returns exit code, signal, stdout, stderr. Each stream is capped at \
                `max_bytes` (default {DEFAULT_MAX_BYTES}): over the cap, the head and tail \
                are kept and the middle is elided with a `[... N bytes omitted ...]` marker. \
                Before eliding, the full stream is saved verbatim on the executing host \
                under `~/.myco/session/<shard>/<session>/tool_output/`, and the marker \
                names the exact file — search it there (grep/head/tail) instead of \
                re-running the command; don't cat a dump whole. Prefer `exec` for finite \
                commands (builds, tests, installs). Raise `timeout_ms` when the job may \
                exceed {exec_default_s}s.\n\
                - start: spawn a long-lived process **in the background**. Requires \
                `session_id`. `command` is the program line (default: `bash -i`). Optional \
                `stdin` is written after spawn. Returns a snapshot; the process keeps \
                running.\n\
                - write: write `stdin` to a session, then collect a snapshot (process stays \
                alive).\n\
                - read: collect more output without writing (process stays alive).\n\
                - close: kill and reap a session.\n\
                - list: list live sessions.\n\n\
                For start/write/read, the child runs in the background. Each call waits until \
                an idle gap (`idle_ms`, default {DEFAULT_IDLE_MS}), a hard timeout \
                (`timeout_ms`, default {DEFAULT_TIMEOUT_MS} ms / {session_default_s}s; max \
                {MAX_TIMEOUT_MS} ms / {session_max_min} min), a byte cap (`max_bytes`, \
                default {DEFAULT_MAX_BYTES}), or process exit — then returns partial output \
                with status timed_out / truncated / running while the session stays live. \
                Raise `timeout_ms` when you need to wait longer for quiet interactive \
                programs.\n\n\
                **Working directory:** pass optional `cwd` on `exec` / `start` to set the \
                process working directory. Prefer `cwd` over prefixing commands with `cd … &&`. \
                Tool uses whose `command` starts with `cd` are **rejected** — use `cwd` \
                instead. (`write` stdin may still send interactive `cd` into a live shell.)",
                exec_default_s = DEFAULT_EXEC_TIMEOUT_MS / 1000,
                exec_max_min = MAX_EXEC_TIMEOUT_MS / 60_000,
                session_default_s = DEFAULT_TIMEOUT_MS / 1000,
                session_max_min = MAX_TIMEOUT_MS / 60_000,
            ),
            input_schema: schemars::schema_for!(Input).to_value(),
        }]
    }

    fn dispatch_tool_use(
        self: Arc<Self>,
        tool_use: generative_model::ToolUse,
        ctx: HostDispatchContext,
    ) -> Async<generative_model::ToolResult> {
        Box::pin(async move {
            let input: Input = match serde_json::from_value(tool_use.input) {
                Ok(input) => input,
                Err(e) => {
                    return generative_model::ToolResult::err(format!(
                        "Error deserializing bash input: {e}"
                    ));
                }
            };
            let action = match resolve_action(&input) {
                Ok(a) => a,
                Err(e) => return generative_model::ToolResult::err(e),
            };
            // Owner is the agent that issued this tool call (root or subagent).
            self.execute(action, ctx.agent_id, &tool_use.id, ctx.cancel)
                .await
        })
    }

    fn on_agent_finished(&self, agent_id: Uuid) {
        self.reap_owner(agent_id);
    }
}

impl BashService {
    pub fn new() -> Self {
        Self::default()
    }

    /// Test constructor: dump exec output under `root/{agent_hex}/tool_output`
    /// instead of the real `~/.myco` session tree.
    #[cfg(test)]
    fn with_dump_root(root: std::path::PathBuf) -> Self {
        Self {
            dump_root: Some(root),
            ..Self::default()
        }
    }

    async fn execute(
        &self,
        action: Action,
        owner: Uuid,
        tool_use_id: &str,
        cancel: crate::core::CancelToken,
    ) -> generative_model::ToolResult {
        match action {
            Action::Exec {
                command,
                cwd,
                timeout_ms,
                max_bytes,
            } => {
                let dump = self.exec_dump(owner, tool_use_id);
                self.run_oneshot(
                    &command,
                    cwd.as_deref(),
                    timeout_ms,
                    max_bytes,
                    dump,
                    cancel,
                )
                .await
            }
            Action::Start {
                session_id,
                command,
                cwd,
                stdin,
                timeout_ms,
                idle_ms,
                max_bytes,
            } => {
                self.session_start(
                    &session_id,
                    owner,
                    command.as_deref(),
                    cwd.as_deref(),
                    stdin.as_deref(),
                    timeout_ms,
                    idle_ms,
                    max_bytes,
                    cancel,
                )
                .await
            }
            Action::Write {
                session_id,
                stdin,
                timeout_ms,
                idle_ms,
                max_bytes,
            } => {
                self.session_write(
                    &session_id,
                    owner,
                    &stdin,
                    timeout_ms,
                    idle_ms,
                    max_bytes,
                    cancel,
                )
                .await
            }
            Action::Read {
                session_id,
                timeout_ms,
                idle_ms,
                max_bytes,
            } => {
                self.session_read(&session_id, owner, timeout_ms, idle_ms, max_bytes, cancel)
                    .await
            }
            Action::Close { session_id } => self.session_close(&session_id, owner).await,
            Action::List => self.session_list(owner),
        }
    }

    /// Where one exec call's over-cap streams are dumped verbatim: the
    /// conversation-session directory (layout mirrored on every host, keyed by
    /// the owning agent id), one `<tool_use_id>.stdout` / `.stderr` pair per
    /// tool use. `None` when this host cannot resolve `~/.myco` — dumping is
    /// best-effort and must never fail the tool call.
    fn exec_dump(&self, owner: Uuid, tool_use_id: &str) -> Option<ExecDump> {
        let hex = crate::session::uuid_simple_hex(owner);
        let session_dir = match &self.dump_root {
            Some(root) => root.join(&hex),
            None => crate::session::session_dir_path(&hex).ok()?,
        };
        Some(ExecDump {
            dir: session_dir.join("tool_output"),
            stem: sanitize_file_stem(tool_use_id),
        })
    }

    /// Run `command` in a fresh bash process (`bash -c`).
    ///
    /// Unlike sessions, exec **waits for the process to exit**. Bounded by
    /// `timeout_ms` so a runaway command cannot hang the agent forever; on
    /// timeout or cancel the child is killed and partial stdout/stderr are
    /// returned. The final pipe drain is bounded too (`EXEC_DRAIN_GRACE`):
    /// a backgrounded grandchild that inherits the pipes must not hold the
    /// result hostage past the child's own exit. Every return path caps each
    /// stream at `max_bytes` (see `cap_exec_stream`) so one flooding command
    /// cannot saturate the model context; over-cap streams are first dumped
    /// verbatim to `dump` so nothing is lost.
    #[allow(clippy::too_many_arguments)]
    async fn run_oneshot(
        &self,
        command: &str,
        cwd: Option<&str>,
        timeout_ms: u64,
        max_bytes: usize,
        dump: Option<ExecDump>,
        cancel: crate::core::CancelToken,
    ) -> generative_model::ToolResult {
        let mut cmd = tokio::process::Command::new("bash");
        cmd.args(["-c", command])
            // Never inherit stdin: in `--mode host` it is the NDJSON protocol
            // pipe, and a child that reads it (python, xargs, `read`…) would
            // consume protocol bytes and desync the whole host connection.
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            // Own process group so timeout/cancel can kill grandchildren too.
            .process_group(0);
        if let Some(dir) = cwd {
            cmd.current_dir(dir);
        }
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(error) => {
                return generative_model::ToolResult::err(format!(
                    "Error spawning command{}: {error}",
                    cwd.map(|d| format!(" (cwd={d:?})")).unwrap_or_default()
                ));
            }
        };
        let child_pid = child.id();

        let stdout = child.stdout.take().expect("stdout piped");
        let stderr = child.stderr.take().expect("stderr piped");
        // Shared buffers, appended to as data arrives, so the bounded drain
        // below can report partial output even when a reader never hits EOF.
        let stdout_buf = Arc::new(Mutex::new(Vec::new()));
        let stderr_buf = Arc::new(Mutex::new(Vec::new()));
        let stdout_task = spawn_capture(stdout, Arc::clone(&stdout_buf));
        let stderr_task = spawn_capture(stderr, Arc::clone(&stderr_buf));

        let deadline = Duration::from_millis(timeout_ms.max(1));
        // When cancel/timeout wins, select drops the wait future so we can
        // kill the process group + wait without a conflicting &mut Child borrow.
        enum Outcome {
            Cancelled,
            TimedOut,
            Status(std::io::Result<std::process::ExitStatus>),
        }
        let outcome = tokio::select! {
            biased;
            _ = cancel.cancelled() => Outcome::Cancelled,
            _ = tokio::time::sleep(deadline) => Outcome::TimedOut,
            status = child.wait() => Outcome::Status(status),
        };

        match outcome {
            Outcome::Cancelled => {
                kill_process_group(child_pid);
                let _ = child.start_kill();
                let _ = child.wait().await;
                drain_capture(stdout_task, stderr_task).await;
                let out = stdout_buf.lock().map(|g| g.clone()).unwrap_or_default();
                let err = stderr_buf.lock().map(|g| g.clone()).unwrap_or_default();
                generative_model::ToolResult::err(format!(
                    "exec cancelled\n\
                     stdout:\n{}\n\
                     stderr:\n{}",
                    cap_exec_stream(&out, max_bytes, dump.as_ref(), "stdout").await,
                    cap_exec_stream(&err, max_bytes, dump.as_ref(), "stderr").await,
                ))
            }
            Outcome::TimedOut => {
                kill_process_group(child_pid);
                let _ = child.start_kill();
                let _ = child.wait().await;
                drain_capture(stdout_task, stderr_task).await;
                let out = stdout_buf.lock().map(|g| g.clone()).unwrap_or_default();
                let err = stderr_buf.lock().map(|g| g.clone()).unwrap_or_default();
                generative_model::ToolResult::text(format!(
                    "Exit code: None\n\
                     Termination signal: None\n\
                     status: timed_out\n\
                     timeout_ms: {timeout_ms}\n\
                     stdout:\n{}\n\
                     stderr:\n{}\n\
                     (exec timed out after {timeout_ms}ms; process group killed)\n",
                    cap_exec_stream(&out, max_bytes, dump.as_ref(), "stdout").await,
                    cap_exec_stream(&err, max_bytes, dump.as_ref(), "stderr").await,
                ))
            }
            Outcome::Status(status) => {
                drain_capture(stdout_task, stderr_task).await;
                let out = stdout_buf.lock().map(|g| g.clone()).unwrap_or_default();
                let err = stderr_buf.lock().map(|g| g.clone()).unwrap_or_default();
                match status {
                    Ok(status) => generative_model::ToolResult::text(format!(
                        "Exit code: {:?}\n\
                         Termination signal: {:?}\n\
                         stdout:\n{}\n\
                         stderr:\n{}",
                        status.code(),
                        status.signal(),
                        cap_exec_stream(&out, max_bytes, dump.as_ref(), "stdout").await,
                        cap_exec_stream(&err, max_bytes, dump.as_ref(), "stderr").await,
                    )),
                    Err(error) => generative_model::ToolResult::err(format!(
                        "Error executing command: {error}"
                    )),
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn session_start(
        &self,
        session_id: &str,
        owner: Uuid,
        command: Option<&str>,
        cwd: Option<&str>,
        stdin: Option<&str>,
        timeout_ms: u64,
        idle_ms: u64,
        max_bytes: usize,
        cancel: crate::core::CancelToken,
    ) -> generative_model::ToolResult {
        if session_id.is_empty() {
            return generative_model::ToolResult::err("session_id must be non-empty");
        }

        // Reject duplicates and enforce session cap before spawning.
        {
            let sessions = match self.sessions.lock() {
                Ok(g) => g,
                Err(e) => {
                    return generative_model::ToolResult::err(format!(
                        "sessions lock poisoned: {e}"
                    ));
                }
            };
            if sessions.contains_key(session_id) {
                return generative_model::ToolResult::err(format!(
                    "session {session_id:?} already exists; close it first"
                ));
            }
            if sessions.len() >= MAX_SESSIONS {
                return generative_model::ToolResult::err(format!(
                    "too many sessions (max {MAX_SESSIONS}); close one first"
                ));
            }
        }

        let cmdline = command.unwrap_or("bash -i");
        let mut cmd = tokio::process::Command::new("bash");
        cmd.args(["-c", cmdline])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .env("PYTHONUNBUFFERED", "1")
            // Own process group so close/reap can kill the whole tree.
            .process_group(0);
        if let Some(dir) = cwd {
            cmd.current_dir(dir);
        }
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                return generative_model::ToolResult::err(format!(
                    "failed to spawn session command {cmdline:?}{}: {e}",
                    cwd.map(|d| format!(" (cwd={d:?})")).unwrap_or_default()
                ));
            }
        };

        let pid = child.id();

        let child_stdin = match child.stdin.take() {
            Some(s) => s,
            None => {
                let _ = child.kill().await;
                return generative_model::ToolResult::err("child stdin missing after spawn");
            }
        };
        let stdout = match child.stdout.take() {
            Some(s) => s,
            None => {
                let _ = child.kill().await;
                return generative_model::ToolResult::err("child stdout missing after spawn");
            }
        };
        let stderr = match child.stderr.take() {
            Some(s) => s,
            None => {
                let _ = child.kill().await;
                return generative_model::ToolResult::err("child stderr missing after spawn");
            }
        };

        let shared = Arc::new(SessionShared {
            buffer: Mutex::new(OutputBuffer::default()),
            notify: Notify::new(),
            generation: AtomicU64::new(0),
        });

        // Reader tasks push into the shared buffer and notify waiters.
        spawn_reader(stdout, StreamKind::Stdout, Arc::clone(&shared));
        spawn_reader(stderr, StreamKind::Stderr, Arc::clone(&shared));
        spawn_waiter(child, Arc::clone(&shared));

        let session = Session {
            owner,
            cmdline: cmdline.to_string(),
            stdin: Mutex::new(Some(child_stdin)),
            shared,
            created_at: Instant::now(),
            last_used: Mutex::new(Instant::now()),
            pid,
        };

        {
            let mut sessions = match self.sessions.lock() {
                Ok(g) => g,
                Err(e) => {
                    return generative_model::ToolResult::err(format!(
                        "sessions lock poisoned: {e}"
                    ));
                }
            };
            // Re-check under lock in case of concurrent start with same id.
            // The child is already running (readers + waiter own it), so a
            // losing start must kill it here or it leaks unkillable: its pid
            // is never stored anywhere the agent can reach.
            if sessions.contains_key(session_id) {
                kill_process_group(pid);
                return generative_model::ToolResult::err(format!(
                    "session {session_id:?} already exists; close it first"
                ));
            }
            if sessions.len() >= MAX_SESSIONS {
                kill_process_group(pid);
                return generative_model::ToolResult::err(format!(
                    "too many sessions (max {MAX_SESSIONS}); close one first"
                ));
            }
            sessions.insert(session_id.to_string(), session);
        }

        // Optional initial stdin, then collect a first snapshot.
        if let Some(data) = stdin
            && let Err(e) = self.write_to_session(session_id, data).await
        {
            return generative_model::ToolResult::err(format!(
                "session {session_id:?} started but initial stdin write failed: {e}"
            ));
        }

        let snapshot = self
            .collect_from_session(session_id, timeout_ms, idle_ms, max_bytes, cancel)
            .await;
        match snapshot {
            Ok(s) => generative_model::ToolResult::text(s.format()),
            Err(e) => generative_model::ToolResult::err(e),
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn session_write(
        &self,
        session_id: &str,
        owner: Uuid,
        stdin: &str,
        timeout_ms: u64,
        idle_ms: u64,
        max_bytes: usize,
        cancel: crate::core::CancelToken,
    ) -> generative_model::ToolResult {
        if let Err(e) = self.ensure_owner(session_id, owner) {
            return generative_model::ToolResult::err(e);
        }
        if let Err(e) = self.write_to_session(session_id, stdin).await {
            return generative_model::ToolResult::err(e);
        }
        match self
            .collect_from_session(session_id, timeout_ms, idle_ms, max_bytes, cancel)
            .await
        {
            Ok(s) => generative_model::ToolResult::text(s.format()),
            Err(e) => generative_model::ToolResult::err(e),
        }
    }

    async fn session_read(
        &self,
        session_id: &str,
        owner: Uuid,
        timeout_ms: u64,
        idle_ms: u64,
        max_bytes: usize,
        cancel: crate::core::CancelToken,
    ) -> generative_model::ToolResult {
        if let Err(e) = self.ensure_owner(session_id, owner) {
            return generative_model::ToolResult::err(e);
        }
        match self
            .collect_from_session(session_id, timeout_ms, idle_ms, max_bytes, cancel)
            .await
        {
            Ok(s) => generative_model::ToolResult::text(s.format()),
            Err(e) => generative_model::ToolResult::err(e),
        }
    }

    async fn session_close(&self, session_id: &str, owner: Uuid) -> generative_model::ToolResult {
        let session = {
            let mut sessions = match self.sessions.lock() {
                Ok(g) => g,
                Err(e) => {
                    return generative_model::ToolResult::err(format!(
                        "sessions lock poisoned: {e}"
                    ));
                }
            };
            match sessions.get(session_id) {
                Some(s) if s.owner != owner => {
                    return generative_model::ToolResult::err(format!(
                        "session {session_id:?} is owned by another agent"
                    ));
                }
                Some(_) => {}
                None => {
                    return generative_model::ToolResult::err(format!(
                        "unknown session {session_id:?}"
                    ));
                }
            }
            sessions
                .remove(session_id)
                .expect("session present after check")
        };

        // Drop stdin (EOF). The waiter task owns the Child with kill_on_drop.
        {
            let mut guard = match session.stdin.lock() {
                Ok(g) => g,
                Err(e) => {
                    return generative_model::ToolResult::err(format!("stdin lock poisoned: {e}"));
                }
            };
            *guard = None;
        }

        // Drain any final buffered output (short wait).
        let snapshot = collect_output(
            &session.shared,
            session_id,
            session.owner,
            &session.cmdline,
            1_000,
            100,
            DEFAULT_MAX_BYTES,
            &crate::core::CancelToken::new(),
        )
        .await;

        kill_session_process(&session);

        let mut text = snapshot.format();
        text.push_str("\n(session closed)\n");
        generative_model::ToolResult::text(text)
    }

    fn session_list(&self, owner: Uuid) -> generative_model::ToolResult {
        let sessions = match self.sessions.lock() {
            Ok(g) => g,
            Err(e) => {
                return generative_model::ToolResult::err(format!("sessions lock poisoned: {e}"));
            }
        };
        let mine: Vec<_> = sessions.iter().filter(|(_, s)| s.owner == owner).collect();
        if mine.is_empty() {
            return generative_model::ToolResult::text("(no live sessions)\n");
        }
        let mut lines = Vec::new();
        lines.push(format!("sessions: {}", mine.len()));
        for (id, s) in mine {
            let exited = s
                .shared
                .buffer
                .lock()
                .ok()
                .and_then(|b| {
                    if b.exited {
                        Some(format!("exited({:?})", b.exit_code))
                    } else {
                        None
                    }
                })
                .unwrap_or_else(|| "running".into());
            let last_used = s
                .last_used
                .lock()
                .map(|t| t.elapsed().as_secs())
                .unwrap_or(0);
            lines.push(format!(
                "- id={id} owner={} cmdline={:?} status={exited} last_used_s_ago={last_used} created_s_ago={}",
                crate::session::uuid_simple_hex(s.owner),
                s.cmdline,
                s.created_at.elapsed().as_secs()
            ));
        }
        lines.push(String::new());
        generative_model::ToolResult::text(lines.join("\n"))
    }

    fn ensure_owner(&self, session_id: &str, owner: Uuid) -> Result<(), String> {
        let sessions = self
            .sessions
            .lock()
            .map_err(|e| format!("sessions lock poisoned: {e}"))?;
        let session = sessions
            .get(session_id)
            .ok_or_else(|| format!("unknown session {session_id:?}"))?;
        if session.owner != owner {
            return Err(format!("session {session_id:?} is owned by another agent"));
        }
        Ok(())
    }

    /// Synchronously kill and drop every session owned by `owner`.
    /// Called from `on_agent_finished` / `Agent::drop` — must not await.
    fn reap_owner(&self, owner: Uuid) {
        let victims: Vec<Session> = {
            let Ok(mut sessions) = self.sessions.lock() else {
                return;
            };
            let keys: Vec<String> = sessions
                .iter()
                .filter(|(_, s)| s.owner == owner)
                .map(|(id, _)| id.clone())
                .collect();
            keys.into_iter()
                .filter_map(|id| sessions.remove(&id))
                .collect()
        };
        for session in victims {
            // Drop stdin (EOF) then best-effort SIGKILL.
            if let Ok(mut guard) = session.stdin.lock() {
                *guard = None;
            }
            kill_session_process(&session);
        }
    }

    async fn write_to_session(&self, session_id: &str, data: &str) -> Result<(), String> {
        // Bump generation so an in-flight collect doesn't treat pre-write idle as done.
        // Take stdin out briefly to write without holding the sessions map lock across await.
        let (stdin_slot, shared) = {
            let sessions = self
                .sessions
                .lock()
                .map_err(|e| format!("sessions lock poisoned: {e}"))?;
            let session = sessions
                .get(session_id)
                .ok_or_else(|| format!("unknown session {session_id:?}"))?;
            session.shared.generation.fetch_add(1, Ordering::SeqCst);
            let shared = Arc::clone(&session.shared);
            let stdin = session
                .stdin
                .lock()
                .map_err(|e| format!("stdin lock poisoned: {e}"))?
                .take();
            (stdin, shared)
        };

        let Some(mut stdin) = stdin_slot else {
            let exited = shared.buffer.lock().ok().map(|b| b.exited).unwrap_or(false);
            if exited {
                return Err(format!(
                    "session {session_id:?} has exited; close it and start a new one"
                ));
            }
            return Err(format!("session {session_id:?} stdin is closed"));
        };

        // Bound the write so a full pipe / stuck child cannot hang the agent.
        // Keep this independent of the larger session timeout_ms ceiling.
        let write_timeout = Duration::from_millis(STDIN_WRITE_TIMEOUT_MS);
        let write_result = tokio::time::timeout(write_timeout, async {
            stdin.write_all(data.as_bytes()).await?;
            stdin.flush().await
        })
        .await;
        match write_result {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                self.return_stdin(session_id, stdin);
                return Err(format!("stdin write failed: {e}"));
            }
            Err(_elapsed) => {
                self.return_stdin(session_id, stdin);
                return Err(format!(
                    "stdin write timed out after {}ms (child may not be reading stdin)",
                    STDIN_WRITE_TIMEOUT_MS
                ));
            }
        }

        self.return_stdin(session_id, stdin);
        if let Ok(sessions) = self.sessions.lock()
            && let Some(session) = sessions.get(session_id)
            && let Ok(mut t) = session.last_used.lock()
        {
            *t = Instant::now();
        }
        shared.notify.notify_waiters();
        Ok(())
    }

    fn return_stdin(&self, session_id: &str, stdin: ChildStdin) {
        let Ok(sessions) = self.sessions.lock() else {
            return;
        };
        let Some(session) = sessions.get(session_id) else {
            return;
        };
        if let Ok(mut slot) = session.stdin.lock() {
            *slot = Some(stdin);
        }
    }

    async fn collect_from_session(
        &self,
        session_id: &str,
        timeout_ms: u64,
        idle_ms: u64,
        max_bytes: usize,
        cancel: crate::core::CancelToken,
    ) -> Result<SessionSnapshot, String> {
        let (shared, owner, cmdline) = {
            let sessions = self
                .sessions
                .lock()
                .map_err(|e| format!("sessions lock poisoned: {e}"))?;
            let session = sessions
                .get(session_id)
                .ok_or_else(|| format!("unknown session {session_id:?}"))?;
            if let Ok(mut t) = session.last_used.lock() {
                *t = Instant::now();
            }
            (
                Arc::clone(&session.shared),
                session.owner,
                session.cmdline.clone(),
            )
        };
        if cancel.is_cancelled() {
            return Err("cancelled".into());
        }
        Ok(collect_output(
            &shared, session_id, owner, &cmdline, timeout_ms, idle_ms, max_bytes, &cancel,
        )
        .await)
    }
}

// --- session internals -------------------------------------------------------

struct Session {
    /// Agent that started this session; only this agent may write/read/close it.
    owner: Uuid,
    cmdline: String,
    stdin: Mutex<Option<ChildStdin>>,
    shared: Arc<SessionShared>,
    created_at: Instant,
    last_used: Mutex<Instant>,
    /// OS pid for best-effort kill on close / reap.
    pid: Option<u32>,
}

struct SessionShared {
    buffer: Mutex<OutputBuffer>,
    notify: Notify,
    /// Bumped on each write so waiters reset their idle clock.
    generation: AtomicU64,
}

#[derive(Default)]
struct OutputBuffer {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    /// Total bytes ever observed (for activity detection).
    total_bytes: usize,
    exited: bool,
    exit_code: Option<i32>,
    exit_signal: Option<i32>,
    /// Readers (stdout, stderr) that hit EOF. 2 + `exited` ⇒ the whole
    /// process group is almost certainly gone (see `kill_session_process`).
    eof_streams: u8,
}

#[derive(Clone, Copy)]
enum StreamKind {
    Stdout,
    Stderr,
}

struct SessionSnapshot {
    session_id: String,
    owner: Uuid,
    cmdline: String,
    status: SnapshotStatus,
    exit_code: Option<i32>,
    exit_signal: Option<i32>,
    stdout: String,
    stderr: String,
    bytes_returned: usize,
}

#[derive(Clone, Copy)]
enum SnapshotStatus {
    Running,
    Exited,
    TimedOut,
    Truncated,
}

impl SnapshotStatus {
    fn as_str(self) -> &'static str {
        match self {
            SnapshotStatus::Running => "running",
            SnapshotStatus::Exited => "exited",
            SnapshotStatus::TimedOut => "timed_out",
            SnapshotStatus::Truncated => "truncated",
        }
    }
}

impl SessionSnapshot {
    fn format(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!("session_id: {}\n", self.session_id));
        out.push_str(&format!(
            "owner: {}\n",
            crate::session::uuid_simple_hex(self.owner)
        ));
        out.push_str(&format!("cmdline: {:?}\n", self.cmdline));
        out.push_str(&format!("status: {}\n", self.status.as_str()));
        out.push_str(&format!("exit_code: {:?}\n", self.exit_code));
        out.push_str(&format!("exit_signal: {:?}\n", self.exit_signal));
        out.push_str(&format!("bytes_returned: {}\n", self.bytes_returned));
        out.push_str("stdout:\n");
        out.push_str(&self.stdout);
        if !self.stdout.ends_with('\n') && !self.stdout.is_empty() {
            out.push('\n');
        }
        out.push_str("stderr:\n");
        out.push_str(&self.stderr);
        if !self.stderr.ends_with('\n') && !self.stderr.is_empty() {
            out.push('\n');
        }
        match self.status {
            SnapshotStatus::TimedOut => {
                out.push_str(
                    "(timed out waiting for more output; session still live — call read/write/close)\n",
                );
            }
            SnapshotStatus::Truncated => {
                out.push_str("(output truncated at max_bytes; more may be buffered — call read)\n");
            }
            SnapshotStatus::Running => {
                out.push_str("(session still running; call read/write/close as needed)\n");
            }
            SnapshotStatus::Exited => {
                out.push_str("(process exited; call close to reap the session)\n");
            }
        }
        out
    }
}

/// How long exec waits for its pipe readers after the child is gone. The
/// readers only EOF when *every* write end closes, so a backgrounded
/// grandchild (`sleep 300 & echo hi`) would otherwise stall the result for
/// its whole lifetime — or forever, for a daemon.
const EXEC_DRAIN_GRACE: Duration = Duration::from_millis(500);

/// Read `reader` to EOF, appending to `buf` as data arrives (not just at EOF),
/// so a bounded drain still observes everything read so far.
fn spawn_capture<R>(mut reader: R, buf: Arc<Mutex<Vec<u8>>>) -> tokio::task::JoinHandle<()>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut chunk = vec![0u8; 4096];
        loop {
            match reader.read(&mut chunk).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if let Ok(mut g) = buf.lock() {
                        g.extend_from_slice(&chunk[..n]);
                    }
                }
            }
        }
    })
}

/// Verbatim on-disk destination for one exec call's over-cap streams:
/// `<dir>/<stem>.stdout` / `<dir>/<stem>.stderr`.
struct ExecDump {
    dir: std::path::PathBuf,
    stem: String,
}

/// Make a provider-issued tool-use id (`toolu_…`, `call_…`) safe as a file
/// stem: keep `[A-Za-z0-9._-]`, replace the rest, bound the length.
fn sanitize_file_stem(raw: &str) -> String {
    let mut stem: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '_'
            }
        })
        .take(128)
        .collect();
    if stem.is_empty() {
        stem.push_str("tool_use");
    }
    stem
}

/// Cap one exec stream at `max_bytes`, keeping the head and tail halves and
/// eliding the middle with a marker.
///
/// Unlike session output (buffered; the agent can `read` the rest later),
/// exec output is gone once the call returns, so before eliding, the full
/// stream is written verbatim to `dump` and the marker names that path: the
/// agent can search the file post-hoc instead of re-running the command. The
/// write is best-effort — on failure the marker falls back to re-run advice.
/// The kept ends carry the signal either way: build/test tools print the
/// root-cause error first and the failure summary last. Cuts are byte-exact;
/// a split UTF-8 char renders as U+FFFD at the seam.
async fn cap_exec_stream(
    bytes: &[u8],
    max_bytes: usize,
    dump: Option<&ExecDump>,
    ext: &str,
) -> String {
    if bytes.len() <= max_bytes {
        return String::from_utf8_lossy(bytes).into_owned();
    }
    let head_len = max_bytes.div_ceil(2);
    let tail_len = max_bytes / 2;
    let omitted = bytes.len() - head_len - tail_len;
    let saved = match dump {
        Some(d) => save_dump(d, ext, bytes).await,
        None => None,
    };
    let hint = match &saved {
        Some(path) => format!(
            "full {ext} saved verbatim on this host at {path} — \
             search it there (grep/head/tail), don't re-run the command"
        ),
        None => "filter with grep/head/tail, redirect to a file, or raise max_bytes".to_string(),
    };
    format!(
        "{}\n[... {omitted} bytes omitted ({} bytes total, max_bytes={max_bytes}); {hint} ...]\n{}",
        String::from_utf8_lossy(&bytes[..head_len]),
        bytes.len(),
        String::from_utf8_lossy(&bytes[bytes.len() - tail_len..]),
    )
}

/// Best-effort verbatim write of one stream; returns the path on success.
async fn save_dump(dump: &ExecDump, ext: &str, bytes: &[u8]) -> Option<String> {
    let path = dump.dir.join(format!("{}.{ext}", dump.stem));
    tokio::fs::create_dir_all(&dump.dir).await.ok()?;
    tokio::fs::write(&path, bytes).await.ok()?;
    Some(path.display().to_string())
}

/// Wait up to `EXEC_DRAIN_GRACE` per exec pipe reader, then abort it. Reached
/// with the child already dead, so anything still holding the pipes open is a
/// stray grandchild whose future output we deliberately give up on.
async fn drain_capture(
    stdout_task: tokio::task::JoinHandle<()>,
    stderr_task: tokio::task::JoinHandle<()>,
) {
    for mut task in [stdout_task, stderr_task] {
        if tokio::time::timeout(EXEC_DRAIN_GRACE, &mut task)
            .await
            .is_err()
        {
            task.abort();
        }
    }
}

fn spawn_reader<R>(mut reader: R, kind: StreamKind, shared: Arc<SessionShared>)
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut buf = vec![0u8; 4096];
        loop {
            match reader.read(&mut buf).await {
                Ok(0) => break,
                Ok(n) => {
                    if let Ok(mut b) = shared.buffer.lock() {
                        match kind {
                            StreamKind::Stdout => b.stdout.extend_from_slice(&buf[..n]),
                            StreamKind::Stderr => b.stderr.extend_from_slice(&buf[..n]),
                        }
                        b.total_bytes = b.total_bytes.saturating_add(n);
                    }
                    shared.notify.notify_waiters();
                }
                Err(_) => break,
            }
        }
        if let Ok(mut b) = shared.buffer.lock() {
            b.eof_streams += 1;
        }
        shared.notify.notify_waiters();
    });
}

/// Best-effort SIGKILL of a process group (negative pid to `kill(2)`).
///
/// Children are spawned with `.process_group(0)` so the leader pid is also the
/// pgid. Killing only the leader leaves grandchildren orphaned under init.
fn kill_process_group(pid: Option<u32>) {
    let Some(pid) = pid else {
        return;
    };
    // `kill -KILL -- -<pgid>` targets the whole group.
    let _ = std::process::Command::new("kill")
        .args(["-KILL", "--", &format!("-{pid}")])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// Best-effort SIGKILL of a session's process group.
/// Sync so it is safe to call from `Drop` / `on_agent_finished`.
///
/// Skipped when the leader exited *and* both pipes hit EOF: the group is then
/// almost certainly empty, and `kill(-pgid)` on a fully-dead group could hit
/// an unrelated process that recycled the pid. If anything still holds a pipe
/// open (a live grandchild), we do kill — that is exactly the process close
/// is meant to stop.
fn kill_session_process(session: &Session) {
    let group_done = session
        .shared
        .buffer
        .lock()
        .map(|b| b.exited && b.eof_streams >= 2)
        .unwrap_or(false);
    if !group_done {
        kill_process_group(session.pid);
    }
}

fn spawn_waiter(mut child: Child, shared: Arc<SessionShared>) {
    tokio::spawn(async move {
        let status = child.wait().await;
        if let Ok(mut b) = shared.buffer.lock() {
            b.exited = true;
            if let Ok(st) = status {
                b.exit_code = st.code();
                b.exit_signal = st.signal();
            }
        }
        shared.notify.notify_waiters();
        // `child` drops here; kill_on_drop is a no-op if already exited.
    });
}

/// Drain the shared buffer until idle / timeout / exit / byte cap / cancel.
///
/// Bytes returned are removed from the buffer so subsequent reads only see new data.
/// On cancel, returns whatever is buffered with `TimedOut` status (session stays live).
#[allow(clippy::too_many_arguments)]
async fn collect_output(
    shared: &Arc<SessionShared>,
    session_id: &str,
    owner: Uuid,
    cmdline: &str,
    timeout_ms: u64,
    idle_ms: u64,
    max_bytes: usize,
    cancel: &crate::core::CancelToken,
) -> SessionSnapshot {
    let deadline = Instant::now() + Duration::from_millis(timeout_ms.max(1));
    let idle = Duration::from_millis(idle_ms.max(1));
    let max_bytes = max_bytes.max(1);

    let mut last_activity = Instant::now();
    let mut last_total = shared.buffer.lock().map(|b| b.total_bytes).unwrap_or(0);
    let already_pending = shared
        .buffer
        .lock()
        .map(|b| !b.stdout.is_empty() || !b.stderr.is_empty() || b.exited)
        .unwrap_or(false);
    if already_pending {
        last_activity = Instant::now();
    }

    // Track generation so a concurrent write resets idle.
    let mut seen_gen = shared.generation.load(Ordering::SeqCst);

    let mut status;

    loop {
        if cancel.is_cancelled() {
            status = SnapshotStatus::TimedOut;
            break;
        }

        let (total, exited, pending_len) = {
            let b = shared.buffer.lock().ok();
            match b {
                Some(b) => (
                    b.total_bytes,
                    b.exited,
                    b.stdout.len().saturating_add(b.stderr.len()),
                ),
                None => (last_total, false, 0),
            }
        };

        if total > last_total {
            last_total = total;
            last_activity = Instant::now();
        }

        let current_gen = shared.generation.load(Ordering::SeqCst);
        if current_gen != seen_gen {
            seen_gen = current_gen;
            last_activity = Instant::now();
        }

        if pending_len >= max_bytes {
            status = SnapshotStatus::Truncated;
            break;
        }
        if exited {
            status = SnapshotStatus::Exited;
            break;
        }
        // Idle with data (or we entered with pending and it settled).
        if last_activity.elapsed() >= idle && pending_len > 0 {
            status = SnapshotStatus::Running;
            break;
        }
        // Idle with nothing: only meaningful once we've waited; fall through to timeout.
        if last_activity.elapsed() >= idle && pending_len == 0 && already_pending {
            // Entered with pending that was drained by a concurrent collector, or exited
            // flag raced; treat as settled running/empty.
            status = SnapshotStatus::Running;
            break;
        }
        if Instant::now() >= deadline {
            status = SnapshotStatus::TimedOut;
            break;
        }

        let remaining = deadline.saturating_duration_since(Instant::now());
        let slice = remaining.min(Duration::from_millis(50)).min(idle);
        tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                status = SnapshotStatus::TimedOut;
                break;
            }
            _ = tokio::time::timeout(slice, shared.notify.notified()) => {}
        }
    }

    // Drain up to max_bytes from the buffer (stdout first, then stderr).
    let (stdout, stderr, exit_code, exit_signal, exited) = {
        let mut b = match shared.buffer.lock() {
            Ok(g) => g,
            Err(_) => {
                return SessionSnapshot {
                    session_id: session_id.to_string(),
                    owner,
                    cmdline: cmdline.to_string(),
                    status: SnapshotStatus::TimedOut,
                    exit_code: None,
                    exit_signal: None,
                    stdout: String::new(),
                    stderr: String::new(),
                    bytes_returned: 0,
                };
            }
        };

        let mut budget = max_bytes;
        let mut out_stdout = Vec::new();
        let mut out_stderr = Vec::new();

        let take_stdout = b.stdout.len().min(budget);
        out_stdout.extend_from_slice(&b.stdout[..take_stdout]);
        b.stdout.drain(..take_stdout);
        budget = budget.saturating_sub(take_stdout);

        let take_stderr = b.stderr.len().min(budget);
        out_stderr.extend_from_slice(&b.stderr[..take_stderr]);
        b.stderr.drain(..take_stderr);

        if !b.stdout.is_empty() || !b.stderr.is_empty() {
            status = SnapshotStatus::Truncated;
        } else if b.exited {
            status = SnapshotStatus::Exited;
        }

        (
            String::from_utf8_lossy(&out_stdout).into_owned(),
            String::from_utf8_lossy(&out_stderr).into_owned(),
            b.exit_code,
            b.exit_signal,
            b.exited,
        )
    };

    if exited && !matches!(status, SnapshotStatus::Truncated) {
        status = SnapshotStatus::Exited;
    }

    let bytes_returned = stdout.len().saturating_add(stderr.len());
    SessionSnapshot {
        session_id: session_id.to_string(),
        owner,
        cmdline: cmdline.to_string(),
        status,
        exit_code,
        exit_signal,
        stdout,
        stderr,
        bytes_returned,
    }
}

// --- input schema ------------------------------------------------------------

/// Wire input for the `bash` tool (flat object for Anthropic-friendly JSON Schema).
#[derive(
    Clone, Debug, schemars::JsonSchema, serde::Deserialize, serde::Serialize, PartialEq, Eq,
)]
pub struct Input {
    /// Action to perform. Defaults to `exec` when omitted (or when only `command` is set).
    #[serde(default)]
    action: Option<ActionKind>,
    /// Command line. For `exec`: run via `bash -c`. For `start`: program line (default `bash -i`).
    ///
    /// Must not start with `cd` — use [`Self::cwd`] instead.
    #[serde(default)]
    command: Option<String>,
    /// Working directory for `exec` / `start` (process `current_dir`). Prefer this over
    /// prefixing `command` with `cd … &&`.
    #[serde(default)]
    cwd: Option<String>,
    /// Agent-chosen session name for start/write/read/close.
    #[serde(default)]
    session_id: Option<String>,
    /// Bytes to write to the session's stdin (`start` / `write`).
    #[serde(default)]
    stdin: Option<String>,
    /// Hard wait ceiling in milliseconds.
    /// - start/write/read: default 30000 (30s), max 1800000 (30 min); early return on idle/byte cap.
    /// - exec: default 60000 (60s), max 1800000 (30 min); waits for process exit.
    #[serde(default)]
    timeout_ms: Option<u64>,
    /// Idle gap in milliseconds with no new output before returning (start/write/read). Default 300.
    #[serde(default)]
    idle_ms: Option<u64>,
    /// Max bytes of output to return. Default 32768.
    /// - start/write/read: combined stdout+stderr drain cap; excess stays buffered — `read` again for more.
    /// - exec: per-stream cap; head and tail are kept, the middle elided. The full
    ///   stream is saved verbatim on the executing host (path in the marker).
    #[serde(default)]
    max_bytes: Option<usize>,
}

#[derive(
    Clone, Debug, schemars::JsonSchema, serde::Deserialize, serde::Serialize, PartialEq, Eq,
)]
#[serde(rename_all = "snake_case")]
enum ActionKind {
    Exec,
    Start,
    Write,
    Read,
    Close,
    List,
}

/// Internal validated action after parsing [`Input`].
#[derive(Debug)]
enum Action {
    Exec {
        command: String,
        cwd: Option<String>,
        timeout_ms: u64,
        max_bytes: usize,
    },
    Start {
        session_id: String,
        command: Option<String>,
        cwd: Option<String>,
        stdin: Option<String>,
        timeout_ms: u64,
        idle_ms: u64,
        max_bytes: usize,
    },
    Write {
        session_id: String,
        stdin: String,
        timeout_ms: u64,
        idle_ms: u64,
        max_bytes: usize,
    },
    Read {
        session_id: String,
        timeout_ms: u64,
        idle_ms: u64,
        max_bytes: usize,
    },
    Close {
        session_id: String,
    },
    List,
}

/// True when `command` begins with a shell `cd` (after optional whitespace).
///
/// Models should use the `cwd` param instead of prefixing with `cd … &&`.
fn command_starts_with_cd(command: &str) -> bool {
    let trimmed = command.trim_start();
    // Match `cd` as a shell word: `cd`, `cd …`, `cd\t…`, not `cdo` / `cdpath`.
    matches!(trimmed.as_bytes(), [b'c', b'd'])
        || trimmed.starts_with("cd ")
        || trimmed.starts_with("cd\t")
        || trimmed.starts_with("cd\n")
}

fn reject_if_command_starts_with_cd(command: &str) -> Result<(), String> {
    if command_starts_with_cd(command) {
        return Err(
            "command must not start with `cd`; pass the directory via the `cwd` parameter instead \
             (e.g. {\"command\": \"ls\", \"cwd\": \"/path\"} rather than \"cd /path && ls\")"
                .into(),
        );
    }
    Ok(())
}

fn normalize_cwd(cwd: Option<&String>) -> Result<Option<String>, String> {
    match cwd {
        None => Ok(None),
        Some(s) => {
            let s = s.trim();
            if s.is_empty() {
                Err("`cwd` must be a non-empty path when provided".into())
            } else {
                Ok(Some(s.to_string()))
            }
        }
    }
}

fn resolve_action(input: &Input) -> Result<Action, String> {
    let idle_ms = input.idle_ms.unwrap_or(DEFAULT_IDLE_MS);
    let max_bytes = input.max_bytes.unwrap_or(DEFAULT_MAX_BYTES);
    let cwd = normalize_cwd(input.cwd.as_ref())?;

    let kind = match &input.action {
        Some(k) => k.clone(),
        None => {
            // Backward compatible: bare `{command: ...}` ⇒ exec.
            if input.command.is_some() {
                ActionKind::Exec
            } else if input.session_id.is_some() && input.stdin.is_some() {
                ActionKind::Write
            } else if input.session_id.is_some() {
                ActionKind::Read
            } else {
                return Err("missing action (and no command/session_id to infer one from)".into());
            }
        }
    };

    // Explicit timeout_ms above the safety ceiling is rejected (not silently
    // clamped). Defaults are generous enough for normal interactive work;
    // raise timeout_ms for longer jobs. Cancel still aborts mid-wait.
    fn session_timeout(input: &Input) -> Result<u64, String> {
        match input.timeout_ms {
            None => Ok(DEFAULT_TIMEOUT_MS),
            Some(t) if t > MAX_TIMEOUT_MS => Err(format!(
                "session timeout_ms={t} exceeds max {MAX_TIMEOUT_MS}ms (30 min); pass ≤{MAX_TIMEOUT_MS}"
            )),
            Some(t) => Ok(t.max(1)),
        }
    }
    fn exec_timeout(input: &Input) -> Result<u64, String> {
        match input.timeout_ms {
            None => Ok(DEFAULT_EXEC_TIMEOUT_MS),
            Some(t) if t > MAX_EXEC_TIMEOUT_MS => Err(format!(
                "exec timeout_ms={t} exceeds max {MAX_EXEC_TIMEOUT_MS}ms (30 min); pass ≤{MAX_EXEC_TIMEOUT_MS}"
            )),
            Some(t) => Ok(t.max(1)),
        }
    }

    match kind {
        ActionKind::Exec => {
            let command = input
                .command
                .clone()
                .ok_or_else(|| "exec requires `command`".to_string())?;
            reject_if_command_starts_with_cd(&command)?;
            Ok(Action::Exec {
                command,
                cwd,
                timeout_ms: exec_timeout(input)?,
                max_bytes,
            })
        }
        ActionKind::Start => {
            let session_id = input
                .session_id
                .clone()
                .ok_or_else(|| "start requires `session_id`".to_string())?;
            if let Some(command) = input.command.as_deref() {
                reject_if_command_starts_with_cd(command)?;
            }
            Ok(Action::Start {
                session_id,
                command: input.command.clone(),
                cwd,
                stdin: input.stdin.clone(),
                timeout_ms: session_timeout(input)?,
                idle_ms,
                max_bytes,
            })
        }
        ActionKind::Write => {
            if cwd.is_some() {
                return Err("`cwd` is only valid on `exec` / `start`".into());
            }
            let session_id = input
                .session_id
                .clone()
                .ok_or_else(|| "write requires `session_id`".to_string())?;
            let stdin = input
                .stdin
                .clone()
                .ok_or_else(|| "write requires `stdin`".to_string())?;
            Ok(Action::Write {
                session_id,
                stdin,
                timeout_ms: session_timeout(input)?,
                idle_ms,
                max_bytes,
            })
        }
        ActionKind::Read => {
            if cwd.is_some() {
                return Err("`cwd` is only valid on `exec` / `start`".into());
            }
            let session_id = input
                .session_id
                .clone()
                .ok_or_else(|| "read requires `session_id`".to_string())?;
            Ok(Action::Read {
                session_id,
                timeout_ms: session_timeout(input)?,
                idle_ms,
                max_bytes,
            })
        }
        ActionKind::Close => {
            if cwd.is_some() {
                return Err("`cwd` is only valid on `exec` / `start`".into());
            }
            let session_id = input
                .session_id
                .clone()
                .ok_or_else(|| "close requires `session_id`".to_string())?;
            Ok(Action::Close { session_id })
        }
        ActionKind::List => {
            if cwd.is_some() {
                return Err("`cwd` is only valid on `exec` / `start`".into());
            }
            Ok(Action::List)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::HostWorker;
    use serde_json::json;

    fn tool_use(input: Input) -> generative_model::ToolUse {
        generative_model::ToolUse {
            id: "test".into(),
            name: "bash".into(),
            input: serde_json::to_value(input).unwrap(),
        }
    }

    fn tool_use_json(value: serde_json::Value) -> generative_model::ToolUse {
        generative_model::ToolUse {
            id: "test".into(),
            name: "bash".into(),
            input: value,
        }
    }

    fn result_text(result: &generative_model::ToolResult) -> String {
        result
            .content
            .iter()
            .filter_map(|c| match c {
                generative_model::Content::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Worker with a temp dump root so flood tests never write under the
    /// developer's real `~/.myco`.
    fn harness() -> Arc<HostWorker> {
        Arc::new(HostWorker::new(
            "test",
            vec![Arc::new(BashService::with_dump_root(unique_dump_root())) as Arc<dyn ToolService>],
        ))
    }

    fn unique_dump_root() -> std::path::PathBuf {
        std::env::temp_dir().join(format!("myco-dump-{}", uuid::Uuid::new_v4()))
    }

    fn dispatch_ctx(agent_id: uuid::Uuid) -> HostDispatchContext {
        HostDispatchContext {
            agent_id,
            cancel: crate::core::CancelToken::new(),
            agent_root: None,
        }
    }

    async fn dispatch(harness: Arc<HostWorker>, input: Input) -> generative_model::ToolResult {
        harness
            .dispatch_tool_use(tool_use(input), dispatch_ctx(uuid::Uuid::nil()))
            .await
    }

    async fn dispatch_json(
        harness: Arc<HostWorker>,
        value: serde_json::Value,
    ) -> generative_model::ToolResult {
        harness
            .dispatch_tool_use(tool_use_json(value), dispatch_ctx(uuid::Uuid::nil()))
            .await
    }

    fn unique_id(prefix: &str) -> String {
        format!("{prefix}-{}", uuid::Uuid::new_v4().as_simple())
    }

    #[test]
    fn input_roundtrip_exec() {
        let input = Input {
            action: None,
            command: Some("echo hi".into()),
            cwd: Some("/tmp".into()),
            session_id: None,
            stdin: None,
            timeout_ms: None,
            idle_ms: None,
            max_bytes: None,
        };
        let value = serde_json::to_value(&input).unwrap();
        assert_eq!(value["command"], "echo hi");
        assert_eq!(value["cwd"], "/tmp");
        let parsed: Input = serde_json::from_value(value).unwrap();
        assert_eq!(parsed.command.as_deref(), Some("echo hi"));
        assert_eq!(parsed.cwd.as_deref(), Some("/tmp"));
    }

    #[test]
    fn bare_command_resolves_to_exec() {
        let input: Input = serde_json::from_value(json!({"command": "echo hi"})).unwrap();
        let action = resolve_action(&input).unwrap();
        match action {
            Action::Exec {
                command,
                cwd,
                timeout_ms,
                max_bytes,
            } => {
                assert_eq!(command, "echo hi");
                assert_eq!(cwd, None);
                assert_eq!(timeout_ms, DEFAULT_EXEC_TIMEOUT_MS);
                assert_eq!(max_bytes, DEFAULT_MAX_BYTES);
            }
            _ => panic!("expected Exec"),
        }
    }

    #[test]
    fn rejects_empty_input() {
        let input: Input = serde_json::from_value(json!({})).unwrap();
        assert!(resolve_action(&input).is_err());
    }

    /// The tool description is the model-facing contract: it must state the
    /// defaults/limits actually enforced, not stale hardcoded copies.
    #[test]
    fn tool_description_states_actual_defaults() {
        let specs = BashService::new().tool_specs();
        let d = &specs[0].description;
        for needle in [
            DEFAULT_EXEC_TIMEOUT_MS.to_string(),
            MAX_EXEC_TIMEOUT_MS.to_string(),
            DEFAULT_TIMEOUT_MS.to_string(),
            MAX_TIMEOUT_MS.to_string(),
            DEFAULT_IDLE_MS.to_string(),
            DEFAULT_MAX_BYTES.to_string(),
        ] {
            assert!(d.contains(&needle), "description missing {needle}: {d}");
        }
    }

    #[test]
    fn rejects_command_starting_with_cd() {
        for command in [
            "cd /tmp && ls",
            "  cd /tmp",
            "cd\t/tmp",
            "cd",
            "cd /tmp; ls",
        ] {
            let input: Input = serde_json::from_value(json!({"command": command})).unwrap();
            let err = resolve_action(&input).unwrap_err();
            assert!(
                err.contains("must not start with `cd`") && err.contains("`cwd`"),
                "command={command:?} err={err}"
            );
        }

        // Not a leading shell `cd` word — allowed.
        for command in ["cdo something", "echo cd /tmp", "true && cd /tmp"] {
            let input: Input = serde_json::from_value(json!({"command": command})).unwrap();
            assert!(
                resolve_action(&input).is_ok(),
                "should allow command={command:?}"
            );
        }
    }

    #[test]
    fn rejects_cwd_on_non_spawn_actions() {
        let input: Input = serde_json::from_value(json!({
            "action": "list",
            "cwd": "/tmp",
        }))
        .unwrap();
        let err = resolve_action(&input).unwrap_err();
        assert!(err.contains("`cwd` is only valid"), "{err}");
    }

    #[test]
    fn cwd_resolves_on_exec_and_start() {
        let input: Input = serde_json::from_value(json!({
            "command": "pwd",
            "cwd": " /tmp ",
        }))
        .unwrap();
        match resolve_action(&input).unwrap() {
            Action::Exec { cwd, .. } => assert_eq!(cwd.as_deref(), Some("/tmp")),
            _ => panic!("expected Exec"),
        }

        let input: Input = serde_json::from_value(json!({
            "action": "start",
            "session_id": "s",
            "command": "bash --noprofile --norc",
            "cwd": "/var",
        }))
        .unwrap();
        match resolve_action(&input).unwrap() {
            Action::Start { cwd, .. } => assert_eq!(cwd.as_deref(), Some("/var")),
            _ => panic!("expected Start"),
        }
    }

    #[test]
    fn timeout_ms_defaults_and_rejects_above_session_max() {
        // Default when omitted.
        let input: Input = serde_json::from_value(json!({
            "action": "read",
            "session_id": "s",
        }))
        .unwrap();
        match resolve_action(&input).unwrap() {
            Action::Read { timeout_ms, .. } => assert_eq!(timeout_ms, DEFAULT_TIMEOUT_MS),
            _ => panic!("expected Read"),
        }
        assert_eq!(DEFAULT_TIMEOUT_MS, 30_000);
        assert_eq!(MAX_TIMEOUT_MS, 1_800_000);

        // Explicit multi-minute value under the ceiling is preserved.
        let input: Input = serde_json::from_value(json!({
            "action": "read",
            "session_id": "s",
            "timeout_ms": 120_000,
        }))
        .unwrap();
        match resolve_action(&input).unwrap() {
            Action::Read { timeout_ms, .. } => assert_eq!(timeout_ms, 120_000),
            _ => panic!("expected Read"),
        }

        // Values under the cap are preserved.
        let input: Input = serde_json::from_value(json!({
            "action": "read",
            "session_id": "s",
            "timeout_ms": 250,
        }))
        .unwrap();
        match resolve_action(&input).unwrap() {
            Action::Read { timeout_ms, .. } => assert_eq!(timeout_ms, 250),
            _ => panic!("expected Read"),
        }

        // Above the safety ceiling is rejected (not clamped).
        let input: Input = serde_json::from_value(json!({
            "action": "read",
            "session_id": "s",
            "timeout_ms": 1_800_001,
        }))
        .unwrap();
        let err = resolve_action(&input).unwrap_err();
        assert!(
            err.contains("exceeds max") && err.contains(&MAX_TIMEOUT_MS.to_string()),
            "{err}"
        );
    }

    #[test]
    fn exec_timeout_ms_defaults_to_60s_and_rejects_above_max() {
        assert_eq!(DEFAULT_EXEC_TIMEOUT_MS, 60_000);
        assert_eq!(MAX_EXEC_TIMEOUT_MS, 1_800_000);

        let input: Input = serde_json::from_value(json!({
            "action": "exec",
            "command": "true",
        }))
        .unwrap();
        match resolve_action(&input).unwrap() {
            Action::Exec { timeout_ms, .. } => assert_eq!(timeout_ms, DEFAULT_EXEC_TIMEOUT_MS),
            _ => panic!("expected Exec"),
        }

        // Explicit multi-minute value under the ceiling is preserved.
        let input: Input = serde_json::from_value(json!({
            "action": "exec",
            "command": "true",
            "timeout_ms": 120_000,
        }))
        .unwrap();
        match resolve_action(&input).unwrap() {
            Action::Exec { timeout_ms, .. } => assert_eq!(timeout_ms, 120_000),
            _ => panic!("expected Exec"),
        }

        // Under the max is preserved.
        let input: Input = serde_json::from_value(json!({
            "action": "exec",
            "command": "true",
            "timeout_ms": 5_000,
        }))
        .unwrap();
        match resolve_action(&input).unwrap() {
            Action::Exec { timeout_ms, .. } => assert_eq!(timeout_ms, 5_000),
            _ => panic!("expected Exec"),
        }

        // Above the safety ceiling is rejected.
        let input: Input = serde_json::from_value(json!({
            "action": "exec",
            "command": "true",
            "timeout_ms": 1_800_001,
        }))
        .unwrap();
        let err = resolve_action(&input).unwrap_err();
        assert!(
            err.contains("exceeds max") && err.contains(&MAX_EXEC_TIMEOUT_MS.to_string()),
            "{err}"
        );
    }

    /// No dump target: capping still keeps head + tail with an honest marker.
    #[tokio::test]
    async fn cap_keeps_head_and_tail_without_dump() {
        // At or under the cap: untouched, no marker.
        assert_eq!(cap_exec_stream(b"hello", 5, None, "stdout").await, "hello");
        assert_eq!(cap_exec_stream(b"", 5, None, "stdout").await, "");

        // Over the cap: head + marker + tail, omitted count exact.
        let bytes: Vec<u8> = (0..26).map(|i| b'a' + i).collect();
        let text = cap_exec_stream(&bytes, 10, None, "stdout").await;
        assert!(text.starts_with("abcde"), "{text}");
        assert!(text.ends_with("vwxyz"), "{text}");
        assert!(text.contains("16 bytes omitted"), "{text}");
        assert!(text.contains("26 bytes total"), "{text}");
        assert!(text.contains("max_bytes=10"), "{text}");

        // Odd cap: head gets the extra byte; total kept is still the cap.
        let text = cap_exec_stream(&bytes, 5, None, "stdout").await;
        assert!(text.starts_with("abc"), "{text}");
        assert!(text.ends_with("yz"), "{text}");
        assert!(text.contains("21 bytes omitted"), "{text}");
    }

    #[test]
    fn sanitize_file_stem_keeps_safe_chars_only() {
        assert_eq!(sanitize_file_stem("toolu_01AbC-x.y"), "toolu_01AbC-x.y");
        assert_eq!(sanitize_file_stem("a/b\\c d"), "a_b_c_d");
        assert_eq!(sanitize_file_stem(""), "tool_use");
        assert!(sanitize_file_stem(&"x".repeat(300)).len() <= 128);
    }

    /// Over-cap exec streams are dumped verbatim to the session folder and the
    /// marker names the file, so the agent can search it instead of re-running.
    #[tokio::test]
    async fn exec_truncation_dumps_full_stream_and_names_path() {
        let root = unique_dump_root();
        let harness = Arc::new(HostWorker::new(
            "test",
            vec![Arc::new(BashService::with_dump_root(root.clone())) as Arc<dyn ToolService>],
        ));
        let result = dispatch_json(
            harness,
            json!({
                "action": "exec",
                "command": "seq 1 100000",
                "max_bytes": 2000,
            }),
        )
        .await;
        assert!(!result.is_error, "{}", result_text(&result));
        let text = result_text(&result);

        // Dumped under <root>/<agent-hex>/tool_output/<tool_use_id>.stdout;
        // dispatch_json uses the nil agent and tool_use id "test".
        let dir = root
            .join(crate::session::uuid_simple_hex(uuid::Uuid::nil()))
            .join("tool_output");
        let stdout_path = dir.join("test.stdout");
        let dumped = std::fs::read_to_string(&stdout_path).expect("dump file written");
        let expected: String = (1..=100_000).map(|i| format!("{i}\n")).collect();
        assert_eq!(dumped, expected, "dump must be verbatim");
        // The marker names the exact path.
        assert!(text.contains(&stdout_path.display().to_string()), "{text}");
        // stderr stayed under the cap: no file for it.
        assert!(!dir.join("test.stderr").exists());

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Under the cap nothing is dumped — no per-call disk litter.
    #[tokio::test]
    async fn exec_under_cap_writes_no_dump() {
        let root = unique_dump_root();
        let harness = Arc::new(HostWorker::new(
            "test",
            vec![Arc::new(BashService::with_dump_root(root.clone())) as Arc<dyn ToolService>],
        ));
        let result = dispatch_json(harness, json!({"command": "echo small"})).await;
        assert!(!result.is_error, "{}", result_text(&result));
        assert!(result_text(&result).contains("small"));
        assert!(!root.exists(), "no dump dir should be created: {root:?}");
    }

    /// A flooding exec must come back capped — head and tail survive, the
    /// middle is elided — instead of saturating the model context.
    #[tokio::test]
    async fn exec_truncates_flood_keeping_head_and_tail() {
        let harness = harness();
        let result = dispatch_json(
            harness,
            json!({
                "action": "exec",
                "command": "seq 1 100000",
                "max_bytes": 2000,
            }),
        )
        .await;
        assert!(!result.is_error, "{}", result_text(&result));
        let text = result_text(&result);
        assert!(text.contains("Exit code: Some(0)"), "{text}");
        // First lines survive (root-cause errors live at the head)…
        assert!(text.contains("stdout:\n1\n2\n"), "{text}");
        // …last lines survive (summaries live at the tail)…
        assert!(text.contains("100000"), "{text}");
        // …and the middle is elided with an honest marker.
        assert!(text.contains("bytes omitted"), "{text}");
        assert!(
            text.len() < 4_000,
            "result should be near the 2000-byte cap, got {} bytes: {text}",
            text.len()
        );
    }

    /// The default cap applies when `max_bytes` is omitted: unbounded floods
    /// must not pass through whole.
    #[tokio::test]
    async fn exec_default_max_bytes_caps_output() {
        let harness = harness();
        let result = dispatch_json(
            harness,
            json!({
                "action": "exec",
                // ~1.2 MB of stdout, far over the 32 KiB default.
                "command": "seq 1 200000",
            }),
        )
        .await;
        assert!(!result.is_error, "{}", result_text(&result));
        let text = result_text(&result);
        assert!(text.contains("bytes omitted"), "{text}");
        assert!(
            text.len() < DEFAULT_MAX_BYTES + 2_048,
            "result should be near the {DEFAULT_MAX_BYTES}-byte default cap, got {} bytes",
            text.len()
        );
    }

    /// stdout and stderr are capped independently: a flooding stdout must not
    /// starve stderr diagnostics, and both keep their ends.
    #[tokio::test]
    async fn exec_truncates_streams_independently() {
        let harness = harness();
        let result = dispatch_json(
            harness,
            json!({
                "action": "exec",
                "command": "seq 1 50000; { echo ERR-HEAD; seq 1 50000; echo ERR-TAIL; } 1>&2",
                "max_bytes": 1000,
            }),
        )
        .await;
        assert!(!result.is_error, "{}", result_text(&result));
        let text = result_text(&result);
        assert_eq!(
            text.matches("bytes omitted").count(),
            2,
            "both streams should be truncated: {text}"
        );
        assert!(text.contains("ERR-HEAD"), "{text}");
        assert!(text.contains("ERR-TAIL"), "{text}");
    }

    /// A runaway that floods then hangs: the timeout path must return capped
    /// output too — the flood is exactly when the cap matters most.
    #[tokio::test]
    async fn exec_timeout_output_is_truncated() {
        let harness = harness();
        let result = dispatch_json(
            harness,
            json!({
                "action": "exec",
                "command": "seq 1 100000; sleep 30",
                "timeout_ms": 2_000,
                "max_bytes": 1000,
            }),
        )
        .await;
        assert!(!result.is_error, "{}", result_text(&result));
        let text = result_text(&result);
        assert!(text.contains("timed_out"), "{text}");
        assert!(text.contains("bytes omitted"), "{text}");
        assert!(
            text.len() < 3_000,
            "timed-out result should be capped, got {} bytes: {text}",
            text.len()
        );
    }

    /// Silent long-lived child: tool must return quickly with timed_out while
    /// the process stays alive in the background for later read/close.
    #[tokio::test]
    async fn session_returns_while_process_still_running() {
        let harness = harness();
        let id = unique_id("bg");

        let t0 = Instant::now();
        let start = dispatch_json(
            harness.clone(),
            json!({
                "action": "start",
                "session_id": id,
                // No output for 30s — must not block the tool call that long.
                "command": "bash -c 'sleep 30; echo late'",
                "timeout_ms": 1_000,
                "idle_ms": 200,
            }),
        )
        .await;
        let elapsed = t0.elapsed();
        assert!(!start.is_error, "start: {}", result_text(&start));
        assert!(
            elapsed < Duration::from_secs(3),
            "start should return in ~1s (session max), took {elapsed:?}: {}",
            result_text(&start)
        );
        let text = result_text(&start);
        assert!(
            text.contains("timed_out") || text.contains("status: running"),
            "expected timed_out/running for silent child: {text}"
        );
        assert!(
            !text.contains("stdout:\nlate"),
            "must not wait for late output: {text}"
        );

        // Process must still be live in the session table.
        let list = dispatch_json(harness.clone(), json!({"action": "list"})).await;
        assert!(
            result_text(&list).contains(&id) && result_text(&list).contains("running"),
            "session should still be running in background: {}",
            result_text(&list)
        );

        let _ = dispatch_json(harness, json!({"action": "close", "session_id": id})).await;
    }

    /// One-shot exec waits for exit but must not hang forever on a long sleep.
    #[tokio::test]
    async fn exec_timeout_kills_runaway() {
        let harness = harness();
        let t0 = Instant::now();
        let result = dispatch_json(
            harness,
            json!({
                "action": "exec",
                "command": "sleep 30",
                "timeout_ms": 500,
            }),
        )
        .await;
        let elapsed = t0.elapsed();
        assert!(!result.is_error, "{}", result_text(&result));
        assert!(
            elapsed < Duration::from_secs(3),
            "exec should time out near 500ms, took {elapsed:?}: {}",
            result_text(&result)
        );
        let text = result_text(&result);
        assert!(
            text.contains("timed_out") || text.contains("timed out"),
            "expected timeout status: {text}"
        );
    }

    /// A backgrounded grandchild inherits the stdout/stderr pipes, so the
    /// readers see no EOF at child exit. Exec must still return promptly
    /// (bounded drain), with the output the child did produce.
    #[tokio::test]
    async fn exec_returns_at_exit_despite_backgrounded_pipe_holder() {
        let harness = harness();
        let t0 = Instant::now();
        let result = dispatch_json(
            harness,
            json!({
                "action": "exec",
                "command": "sleep 15 & echo hi",
                "timeout_ms": 30_000,
            }),
        )
        .await;
        let elapsed = t0.elapsed();
        assert!(!result.is_error, "{}", result_text(&result));
        let text = result_text(&result);
        assert!(
            elapsed < Duration::from_secs(5),
            "exec should return at child exit + drain grace, took {elapsed:?}: {text}"
        );
        assert!(text.contains("hi"), "partial output must survive: {text}");
        assert!(text.contains("Exit code: Some(0)"), "{text}");
    }

    /// Exec children get a null stdin. Inheriting ours is never right: in
    /// `--mode host` it is the NDJSON protocol pipe, and a child that reads
    /// it desyncs the whole host connection.
    #[tokio::test]
    async fn exec_stdin_is_null_not_inherited() {
        let harness = harness();
        let t0 = Instant::now();
        let result = dispatch_json(
            harness,
            json!({
                "action": "exec",
                "command": "wc -c",
                "timeout_ms": 10_000,
            }),
        )
        .await;
        let elapsed = t0.elapsed();
        assert!(!result.is_error, "{}", result_text(&result));
        let text = result_text(&result);
        assert!(
            elapsed < Duration::from_secs(3),
            "stdin must be closed (instant EOF), took {elapsed:?}: {text}"
        );
        assert!(
            text.contains("0"),
            "wc -c on null stdin reads 0 bytes: {text}"
        );
    }

    /// Timeout must kill the whole process group, not just the outer `bash -c`.
    ///
    /// Without process-group kill, a command like `bash -c 'sleep 30; …'` leaves
    /// the grandchild `sleep` orphaned under init after we SIGKILL only bash.
    #[tokio::test]
    async fn exec_timeout_kills_process_group_not_just_bash() {
        let harness = harness();
        let marker = std::env::temp_dir().join(format!(
            "myco-timeout-orphan-{}.marker",
            uuid::Uuid::new_v4()
        ));
        let marker_s = marker.to_string_lossy().into_owned();
        // Unique sleep arg so we can find the grandchild without matching other tests.
        let sleep_tag = format!("17.{}", uuid::Uuid::new_v4().as_u128() % 100_000);
        let command = format!("sleep {sleep_tag}; echo still-alive > {marker_s}");

        let t0 = Instant::now();
        let result = dispatch_json(
            harness,
            json!({
                "action": "exec",
                "command": command,
                "timeout_ms": 400,
            }),
        )
        .await;
        let elapsed = t0.elapsed();
        assert!(!result.is_error, "{}", result_text(&result));
        assert!(
            elapsed < Duration::from_secs(3),
            "exec should time out near 400ms, took {elapsed:?}"
        );
        assert!(
            result_text(&result).contains("timed_out")
                || result_text(&result).contains("timed out"),
            "{}",
            result_text(&result)
        );

        // Give a reaped orphan a moment to reparent / finish if kill failed.
        tokio::time::sleep(Duration::from_millis(300)).await;

        // Grandchild must not still be running.
        let ps = std::process::Command::new("ps")
            .args(["-ax", "-o", "pid=,command="])
            .output()
            .expect("ps");
        let ps_text = String::from_utf8_lossy(&ps.stdout);
        assert!(
            !ps_text
                .lines()
                .any(|l| l.contains(&format!("sleep {sleep_tag}"))),
            "grandchild sleep should have been process-group killed; still running:\n{ps_text}"
        );
        assert!(
            !marker.exists(),
            "marker must not be written after timeout (orphan finished the command)"
        );
        let _ = std::fs::remove_file(&marker);
    }

    #[tokio::test]
    async fn exec_cancel_kills_runaway() {
        let service = Arc::new(BashService::new());
        let harness = Arc::new(HostWorker::new(
            "test",
            vec![service.clone() as Arc<dyn ToolService>],
        ));
        let cancel = crate::core::CancelToken::new();
        let cancel2 = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            cancel2.cancel();
        });
        let t0 = Instant::now();
        let result = harness
            .dispatch_tool_use(
                tool_use_json(json!({
                    "action": "exec",
                    "command": "sleep 30",
                    "timeout_ms": 10_000,
                })),
                HostDispatchContext {
                    agent_id: uuid::Uuid::nil(),
                    cancel,
                    agent_root: None,
                },
            )
            .await;
        let elapsed = t0.elapsed();
        assert!(
            elapsed < Duration::from_secs(3),
            "cancel should kill exec quickly, took {elapsed:?}: {}",
            result_text(&result)
        );
        assert!(result.is_error, "cancelled exec should be an error result");
        assert!(
            result_text(&result).contains("cancelled"),
            "{}",
            result_text(&result)
        );
    }

    #[tokio::test]
    async fn echo_stdout() {
        let harness = harness();
        let result = dispatch(
            harness,
            Input {
                action: None,
                command: Some("echo hello-from-bash".into()),
                cwd: None,
                session_id: None,
                stdin: None,
                timeout_ms: None,
                idle_ms: None,
                max_bytes: None,
            },
        )
        .await;
        assert!(!result.is_error, "{}", result_text(&result));
        let text = result_text(&result);
        assert!(text.contains("hello-from-bash"), "{text}");
        assert!(text.contains("Exit code: Some(0)"), "{text}");
        assert!(text.contains("stdout:"), "{text}");
    }

    #[tokio::test]
    async fn nonzero_exit_still_ok_result() {
        let harness = harness();
        let result = dispatch(
            harness,
            Input {
                action: None,
                command: Some("exit 7".into()),
                cwd: None,
                session_id: None,
                stdin: None,
                timeout_ms: None,
                idle_ms: None,
                max_bytes: None,
            },
        )
        .await;
        assert!(!result.is_error, "{}", result_text(&result));
        let text = result_text(&result);
        assert!(text.contains("Exit code: Some(7)"), "{text}");
    }

    #[tokio::test]
    async fn stderr_captured() {
        let harness = harness();
        let result = dispatch(
            harness,
            Input {
                action: None,
                command: Some("echo err-msg 1>&2".into()),
                cwd: None,
                session_id: None,
                stdin: None,
                timeout_ms: None,
                idle_ms: None,
                max_bytes: None,
            },
        )
        .await;
        assert!(!result.is_error, "{}", result_text(&result));
        let text = result_text(&result);
        assert!(text.contains("err-msg"), "{text}");
        assert!(text.contains("stderr:"), "{text}");
    }

    #[tokio::test]
    async fn exec_respects_cwd() {
        let harness = harness();
        let dir = std::env::temp_dir().join(format!("myco-cwd-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let dir_str = dir.to_string_lossy().into_owned();

        let result = dispatch_json(
            harness,
            json!({
                "command": "pwd",
                "cwd": dir_str,
            }),
        )
        .await;
        assert!(!result.is_error, "{}", result_text(&result));
        let text = result_text(&result);
        // macOS /var is often a symlink to /private/var; compare canonical paths.
        let expected = std::fs::canonicalize(&dir).unwrap();
        let expected_s = expected.to_string_lossy();
        assert!(
            text.contains(expected_s.as_ref()) || text.contains(&dir_str),
            "expected pwd under {expected_s} or {dir_str}: {text}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn rejects_cd_prefix_at_dispatch() {
        let harness = harness();
        let result = dispatch_json(harness, json!({"command": "cd /tmp && pwd"})).await;
        assert!(result.is_error, "cd-prefixed command should fail");
        let text = result_text(&result);
        assert!(
            text.contains("must not start with `cd`") && text.contains("`cwd`"),
            "{text}"
        );
    }

    /// Blocking dispatch path: over-max exec timeout must error immediately
    /// (not clamp / not start the process).
    #[tokio::test]
    async fn dispatch_rejects_exec_timeout_above_max() {
        let harness = harness();
        let t0 = Instant::now();
        let result = dispatch_json(
            harness,
            json!({
                "action": "exec",
                "command": "sleep 30",
                "timeout_ms": 1_800_001,
            }),
        )
        .await;
        let elapsed = t0.elapsed();
        assert!(
            result.is_error,
            "expected tool error, got: {}",
            result_text(&result)
        );
        let text = result_text(&result);
        assert!(
            text.contains("exceeds max") && text.contains(&MAX_EXEC_TIMEOUT_MS.to_string()),
            "{text}"
        );
        assert!(
            elapsed < Duration::from_millis(500),
            "reject must be immediate, took {elapsed:?}: {text}"
        );
    }

    /// Blocking dispatch path: over-max session timeout must error.
    #[tokio::test]
    async fn dispatch_rejects_session_timeout_above_max() {
        let harness = harness();
        let t0 = Instant::now();
        let result = dispatch_json(
            harness,
            json!({
                "action": "start",
                "session_id": unique_id("tmax"),
                "command": "bash --noprofile --norc",
                "timeout_ms": 1_800_001,
            }),
        )
        .await;
        let elapsed = t0.elapsed();
        assert!(
            result.is_error,
            "expected tool error, got: {}",
            result_text(&result)
        );
        let text = result_text(&result);
        assert!(
            text.contains("exceeds max") && text.contains(&MAX_TIMEOUT_MS.to_string()),
            "{text}"
        );
        assert!(
            elapsed < Duration::from_millis(500),
            "reject must be immediate, took {elapsed:?}: {text}"
        );
    }

    #[tokio::test]
    async fn session_start_respects_cwd() {
        let harness = harness();
        let id = unique_id("cwd");
        let dir = std::env::temp_dir().join(format!("myco-sess-cwd-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let dir_str = dir.to_string_lossy().into_owned();

        let start = dispatch_json(
            harness.clone(),
            json!({
                "action": "start",
                "session_id": id,
                "command": "bash --noprofile --norc",
                "cwd": dir_str,
                "idle_ms": 200,
                "timeout_ms": 1000,
            }),
        )
        .await;
        assert!(!start.is_error, "start: {}", result_text(&start));

        let write = dispatch_json(
            harness.clone(),
            json!({
                "action": "write",
                "session_id": id,
                "stdin": "pwd\n",
                "idle_ms": 300,
                "timeout_ms": 1000,
            }),
        )
        .await;
        assert!(!write.is_error, "write: {}", result_text(&write));
        let text = result_text(&write);
        let expected = std::fs::canonicalize(&dir).unwrap();
        let expected_s = expected.to_string_lossy();
        assert!(
            text.contains(expected_s.as_ref()) || text.contains(&dir_str),
            "session should start in cwd: {text}"
        );

        let _ = dispatch_json(harness, json!({"action": "close", "session_id": id})).await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn session_cat_roundtrip() {
        let harness = harness();
        let id = unique_id("cat");

        let start = dispatch_json(
            harness.clone(),
            json!({
                "action": "start",
                "session_id": id,
                "command": "cat",
                "idle_ms": 200,
                "timeout_ms": 1000,
            }),
        )
        .await;
        assert!(!start.is_error, "start: {}", result_text(&start));
        let start_text = result_text(&start);
        assert!(
            start_text.contains(&format!("session_id: {id}")),
            "{start_text}"
        );

        let write = dispatch_json(
            harness.clone(),
            json!({
                "action": "write",
                "session_id": id,
                "stdin": "hello-session\n",
                "idle_ms": 200,
                "timeout_ms": 1000,
            }),
        )
        .await;
        assert!(!write.is_error, "write: {}", result_text(&write));
        let write_text = result_text(&write);
        assert!(
            write_text.contains("hello-session"),
            "expected echo from cat: {write_text}"
        );

        let list = dispatch_json(harness.clone(), json!({"action": "list"})).await;
        assert!(!list.is_error, "list: {}", result_text(&list));
        assert!(result_text(&list).contains(&id), "{}", result_text(&list));

        let close = dispatch_json(
            harness.clone(),
            json!({"action": "close", "session_id": id}),
        )
        .await;
        assert!(!close.is_error, "close: {}", result_text(&close));
        assert!(
            result_text(&close).contains("session closed"),
            "{}",
            result_text(&close)
        );

        let list2 = dispatch_json(harness, json!({"action": "list"})).await;
        assert!(
            result_text(&list2).contains("(no live sessions)")
                || !result_text(&list2).contains(&id),
            "{}",
            result_text(&list2)
        );
    }

    /// Interactive shell across multiple tool turns: state must persist.
    #[tokio::test]
    async fn session_interactive_shell_multi_turn() {
        let harness = harness();
        let id = unique_id("sh");

        // Non-interactive bash reading commands from stdin still keeps shell state.
        // Avoid `bash -i` here: prompts/job-control noise makes idle detection flaky in CI.
        let start = dispatch_json(
            harness.clone(),
            json!({
                "action": "start",
                "session_id": id,
                "command": "bash --noprofile --norc",
                "idle_ms": 200,
                "timeout_ms": 1000,
            }),
        )
        .await;
        assert!(!start.is_error, "start: {}", result_text(&start));

        let turn1 = dispatch_json(
            harness.clone(),
            json!({
                "action": "write",
                "session_id": id,
                "stdin": "export MYCO_MULTI_TURN=alive-from-turn-1\n",
                "idle_ms": 200,
                "timeout_ms": 1000,
            }),
        )
        .await;
        assert!(!turn1.is_error, "turn1: {}", result_text(&turn1));

        let turn2 = dispatch_json(
            harness.clone(),
            json!({
                "action": "write",
                "session_id": id,
                "stdin": "printf 'saw=%s\\n' \"$MYCO_MULTI_TURN\"\n",
                "idle_ms": 300,
                "timeout_ms": 1000,
            }),
        )
        .await;
        assert!(!turn2.is_error, "turn2: {}", result_text(&turn2));
        let turn2_text = result_text(&turn2);
        assert!(
            turn2_text.contains("saw=alive-from-turn-1"),
            "shell state must persist across writes: {turn2_text}"
        );

        let turn3 = dispatch_json(
            harness.clone(),
            json!({
                "action": "write",
                "session_id": id,
                "stdin": "echo turn-3-still-here\n",
                "idle_ms": 300,
                "timeout_ms": 1000,
            }),
        )
        .await;
        assert!(!turn3.is_error, "turn3: {}", result_text(&turn3));
        assert!(
            result_text(&turn3).contains("turn-3-still-here"),
            "third turn should still talk to the same shell: {}",
            result_text(&turn3)
        );

        let list = dispatch_json(harness.clone(), json!({"action": "list"})).await;
        assert!(
            result_text(&list).contains(&id) && result_text(&list).contains("running"),
            "session should still be live after multi-turn use: {}",
            result_text(&list)
        );

        let close = dispatch_json(harness, json!({"action": "close", "session_id": id})).await;
        assert!(!close.is_error, "close: {}", result_text(&close));
    }

    #[tokio::test]
    async fn session_python_repl() {
        let harness = harness();
        let id = unique_id("py");

        let start = dispatch_json(
            harness.clone(),
            json!({
                "action": "start",
                "session_id": id,
                "command": "python3 -u -i",
                "idle_ms": 400,
                "timeout_ms": 1000,
            }),
        )
        .await;
        assert!(!start.is_error, "start: {}", result_text(&start));
        // Banner / prompt may land on stderr for python -i.
        let start_text = result_text(&start);
        assert!(
            start_text.contains("Python")
                || start_text.contains(">>>")
                || start_text.contains("status:"),
            "{start_text}"
        );

        let write = dispatch_json(
            harness.clone(),
            json!({
                "action": "write",
                "session_id": id,
                "stdin": "print(2+2)\n",
                "idle_ms": 400,
                "timeout_ms": 1000,
            }),
        )
        .await;
        assert!(!write.is_error, "write: {}", result_text(&write));
        let write_text = result_text(&write);
        assert!(
            write_text.contains('4'),
            "expected python to print 4: {write_text}"
        );

        let close = dispatch_json(harness, json!({"action": "close", "session_id": id})).await;
        assert!(!close.is_error, "close: {}", result_text(&close));
    }

    #[tokio::test]
    async fn session_timeout_returns_partial() {
        let harness = harness();
        let id = unique_id("sleep");

        let start = dispatch_json(
            harness.clone(),
            json!({
                "action": "start",
                "session_id": id,
                // Prints once after 5s; our timeout is much shorter.
                "command": "bash -c 'sleep 5; echo late'",
                "idle_ms": 100,
                "timeout_ms": 400,
            }),
        )
        .await;
        assert!(!start.is_error, "start: {}", result_text(&start));
        let text = result_text(&start);
        assert!(
            text.contains("timed_out") || text.contains("status: running"),
            "expected timeout/running before output: {text}"
        );
        // The status hint contains the word "late" ("still live"); check the stdout body.
        assert!(
            !text.contains("stdout:\nlate") && !text.contains("stdout:\nlate\n"),
            "should not have late output yet: {text}"
        );
        // Stronger: the echo has not landed in the returned stdout section.
        if let Some(rest) = text.split("stdout:\n").nth(1) {
            let body = rest.split("stderr:\n").next().unwrap_or(rest);
            assert!(
                !body.contains("late"),
                "should not have late output yet: {text}"
            );
        }

        let _ = dispatch_json(harness, json!({"action": "close", "session_id": id})).await;
    }

    #[tokio::test]
    async fn session_duplicate_id_rejected() {
        let harness = harness();
        let id = unique_id("dup");

        let start = dispatch_json(
            harness.clone(),
            json!({
                "action": "start",
                "session_id": id,
                "command": "cat",
                "timeout_ms": 1000,
                "idle_ms": 100,
            }),
        )
        .await;
        assert!(!start.is_error, "start: {}", result_text(&start));

        let start2 = dispatch_json(
            harness.clone(),
            json!({
                "action": "start",
                "session_id": id,
                "command": "cat",
            }),
        )
        .await;
        assert!(start2.is_error, "duplicate should error");
        assert!(
            result_text(&start2).contains("already exists"),
            "{}",
            result_text(&start2)
        );

        let _ = dispatch_json(harness, json!({"action": "close", "session_id": id})).await;
    }

    #[tokio::test]
    async fn session_unknown_write_errors() {
        let harness = harness();
        let result = dispatch_json(
            harness,
            json!({
                "action": "write",
                "session_id": "no-such-session",
                "stdin": "x\n",
            }),
        )
        .await;
        assert!(result.is_error);
        assert!(
            result_text(&result).contains("unknown session"),
            "{}",
            result_text(&result)
        );
    }

    #[tokio::test]
    async fn session_byte_cap_truncates() {
        let harness = harness();
        let id = unique_id("big");

        let start = dispatch_json(
            harness.clone(),
            json!({
                "action": "start",
                "session_id": id,
                "command": "cat",
                "timeout_ms": 1000,
                "idle_ms": 200,
            }),
        )
        .await;
        assert!(!start.is_error, "start: {}", result_text(&start));

        // Write more than max_bytes; cat will echo it all.
        let payload = "x".repeat(200);
        let write = dispatch_json(
            harness.clone(),
            json!({
                "action": "write",
                "session_id": id,
                "stdin": payload,
                "timeout_ms": 1000,
                "idle_ms": 300,
                "max_bytes": 50,
            }),
        )
        .await;
        assert!(!write.is_error, "write: {}", result_text(&write));
        let text = result_text(&write);
        assert!(
            text.contains("truncated") || text.contains("bytes_returned"),
            "{text}"
        );

        // Follow-up read may get the rest.
        let read = dispatch_json(
            harness.clone(),
            json!({
                "action": "read",
                "session_id": id,
                "timeout_ms": 1000,
                "idle_ms": 200,
                "max_bytes": 500,
            }),
        )
        .await;
        assert!(!read.is_error, "read: {}", result_text(&read));

        let _ = dispatch_json(harness, json!({"action": "close", "session_id": id})).await;
    }

    #[tokio::test]
    async fn session_exited_process_reports_status() {
        let harness = harness();
        let id = unique_id("exit");

        let start = dispatch_json(
            harness.clone(),
            json!({
                "action": "start",
                "session_id": id,
                "command": "bash -c 'echo bye; exit 3'",
                "timeout_ms": 1000,
                "idle_ms": 200,
            }),
        )
        .await;
        assert!(!start.is_error, "start: {}", result_text(&start));
        let text = result_text(&start);
        assert!(text.contains("bye"), "{text}");
        assert!(
            text.contains("exited") || text.contains("running"),
            "{text}"
        );

        let read = dispatch_json(
            harness.clone(),
            json!({
                "action": "read",
                "session_id": id,
                "timeout_ms": 1000,
                "idle_ms": 100,
            }),
        )
        .await;
        let read_text = result_text(&read);
        assert!(
            read_text.contains("exited") || text.contains("exited"),
            "start={text}\nread={read_text}"
        );
        assert!(
            read_text.contains("exit_code: Some(3)") || text.contains("exit_code: Some(3)"),
            "start={text}\nread={read_text}"
        );

        let _ = dispatch_json(harness, json!({"action": "close", "session_id": id})).await;
    }

    #[tokio::test]
    async fn session_foreign_owner_rejected() {
        let service = Arc::new(BashService::new());
        let harness = Arc::new(HostWorker::new(
            "test",
            vec![service.clone() as Arc<dyn ToolService>],
        ));
        let owner_a = uuid::Uuid::new_v4();
        let owner_b = uuid::Uuid::new_v4();
        let id = unique_id("own");

        let start = harness
            .clone()
            .dispatch_tool_use(
                tool_use_json(json!({
                    "action": "start",
                    "session_id": id,
                    "command": "cat",
                    "timeout_ms": 1000,
                    "idle_ms": 100,
                })),
                HostDispatchContext {
                    agent_id: owner_a,
                    cancel: crate::core::CancelToken::new(),
                    agent_root: None,
                },
            )
            .await;
        assert!(!start.is_error, "start: {}", result_text(&start));
        assert!(
            result_text(&start).contains("owner:"),
            "{}",
            result_text(&start)
        );

        // Different agent cannot write.
        let write = harness
            .clone()
            .dispatch_tool_use(
                tool_use_json(json!({
                    "action": "write",
                    "session_id": id,
                    "stdin": "nope\n",
                })),
                HostDispatchContext {
                    agent_id: owner_b,
                    cancel: crate::core::CancelToken::new(),
                    agent_root: None,
                },
            )
            .await;
        assert!(write.is_error, "foreign write should fail");
        assert!(
            result_text(&write).contains("owned by another agent"),
            "{}",
            result_text(&write)
        );

        // Owner can still write.
        let write_ok = harness
            .clone()
            .dispatch_tool_use(
                tool_use_json(json!({
                    "action": "write",
                    "session_id": id,
                    "stdin": "yep\n",
                    "timeout_ms": 1000,
                    "idle_ms": 200,
                })),
                HostDispatchContext {
                    agent_id: owner_a,
                    cancel: crate::core::CancelToken::new(),
                    agent_root: None,
                },
            )
            .await;
        assert!(
            !write_ok.is_error,
            "owner write: {}",
            result_text(&write_ok)
        );
        assert!(
            result_text(&write_ok).contains("yep"),
            "{}",
            result_text(&write_ok)
        );

        let _ = harness
            .dispatch_tool_use(
                tool_use_json(json!({"action": "close", "session_id": id})),
                HostDispatchContext {
                    agent_id: owner_a,
                    cancel: crate::core::CancelToken::new(),
                    agent_root: None,
                },
            )
            .await;
    }

    #[tokio::test]
    async fn agent_drop_reaps_owned_sessions() {
        let service = Arc::new(BashService::new());
        let harness = Arc::new(HostWorker::new(
            "test",
            vec![service.clone() as Arc<dyn ToolService>],
        ));
        let agent_id = uuid::Uuid::new_v4();
        let id = unique_id("reap");

        // Start a long-lived session as this agent.
        let start = harness
            .clone()
            .dispatch_tool_use(
                tool_use_json(json!({
                    "action": "start",
                    "session_id": id,
                    "command": "cat",
                    "timeout_ms": 1000,
                    "idle_ms": 100,
                })),
                HostDispatchContext {
                    agent_id,
                    cancel: crate::core::CancelToken::new(),
                    agent_root: None,
                },
            )
            .await;
        assert!(!start.is_error, "start: {}", result_text(&start));

        // Session is live.
        {
            let sessions = service.sessions.lock().unwrap();
            assert!(sessions.contains_key(&id), "session should be live");
            assert_eq!(sessions.get(&id).unwrap().owner, agent_id);
        }

        // Dropping an Agent with this id reaps the session.
        {
            // Minimal agent: we only need Drop → notify_agent_finished.
            // Construct via with_context; model is unused on drop.
            // Use a dummy model via a zero-tool scripted path — simplest is call
            // harness.notify_agent_finished directly to unit-test the service side,
            // and separately assert Agent::drop calls it.
            // Direct service path:
            harness.notify_agent_finished(agent_id);
        }

        {
            let sessions = service.sessions.lock().unwrap();
            assert!(
                !sessions.contains_key(&id),
                "session should be reaped on agent finish"
            );
        }
    }
}
