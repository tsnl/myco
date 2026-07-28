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
use crate::external_command::BASH;

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
const DEFAULT_MAX_BYTES: usize = 4_096;
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
}

impl Default for BashService {
    fn default() -> Self {
        Self {
            sessions: Mutex::new(HashMap::new()),
        }
    }
}

impl BashService {
    /// Tool schemas served by this service (static: no instance required).
    pub fn specs() -> Vec<generative_model::ToolSpec> {
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
                Elided exec output is unrecoverable — pipe through grep/head/tail or \
                redirect to a file when you expect a flood, or raise `max_bytes` when you \
                truly need more. Prefer `exec` for finite commands (builds, tests, \
                installs). Raise `timeout_ms` when the job may exceed {exec_default_s}s.\n\
                - start: spawn a long-lived process **in the background**. Requires \
                `session_id`. `command` is the program line (default: `bash -i`). Optional \
                `stdin` is written after spawn. Returns a snapshot; the process keeps \
                running.\n\
                - write: write `stdin` to a session, then collect a snapshot (process stays \
                alive).\n\
                - read: collect more output without writing (process stays alive).\n\
                - close: kill and reap a session.\n\
                - list: list live sessions. Note the session cap ({MAX_SESSIONS}) is shared \
                by every agent on the host while `list` shows only yours — if `start` \
                reports too many sessions and your list looks short, other agents own the \
                rest; close your own idle sessions and retry.\n\n\
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
            input_schema: super::tool_input_schema::<Input>(),
        }]
    }
}

impl ToolService for BashService {
    fn tool_specs(&self) -> Vec<generative_model::ToolSpec> {
        Self::specs()
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
            self.execute(action, ctx.agent_id, ctx.cancel).await
        })
    }

    fn on_agent_finished(&self, agent_id: Uuid) {
        self.reap_owner(agent_id);
    }

    fn running_tool_summaries(&self, agent_id: Uuid) -> Vec<String> {
        let sessions = self.sessions();
        let mut lines: Vec<String> = sessions
            .iter()
            .filter(|(_, s)| {
                // Exited-but-unclosed sessions are not running; skip them.
                s.owner == agent_id && !s.shared.buffer.lock().map(|b| b.exited).unwrap_or(true)
            })
            .map(|(id, s)| {
                let idle = s.last_used.lock().map(|t| t.elapsed()).unwrap_or_default();
                format!(
                    "bash session {id}: {} (up {}, idle {})",
                    summary_cmdline(&s.cmdline),
                    brief_age(s.created_at.elapsed()),
                    brief_age(idle),
                )
            })
            .collect();
        lines.sort();
        lines
    }
}

/// Cmdline for a one-line session summary: first line only, capped.
fn summary_cmdline(cmdline: &str) -> String {
    const MAX_CHARS: usize = 60;
    let first_line = cmdline.lines().next().unwrap_or_default();
    match first_line.char_indices().nth(MAX_CHARS) {
        Some((byte, _)) => format!("{}…", &first_line[..byte]),
        None => first_line.to_string(),
    }
}

/// Compact human age for one-line summaries: `42s`, `7m`, `2h05m`.
fn brief_age(d: Duration) -> String {
    let s = d.as_secs();
    match s {
        0..=59 => format!("{s}s"),
        60..=3599 => format!("{}m", s / 60),
        _ => format!("{}h{:02}m", s / 3600, (s % 3600) / 60),
    }
}

/// Lock, recovering the data from a poisoned mutex: a panicked holder leaves
/// the state intact, and refusing every later bash call would be worse.
fn lock_unpoisoned<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl BashService {
    pub fn new() -> Self {
        Self::default()
    }

    fn sessions(&self) -> std::sync::MutexGuard<'_, HashMap<String, Session>> {
        lock_unpoisoned(&self.sessions)
    }

    async fn execute(
        &self,
        action: Action,
        owner: Uuid,
        cancel: crate::core::CancelToken,
    ) -> generative_model::ToolResult {
        match action {
            Action::Exec {
                command,
                cwd,
                timeout_ms,
                max_bytes,
            } => {
                self.run_oneshot(&command, cwd.as_deref(), timeout_ms, max_bytes, cancel)
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

    /// Run `command` in a fresh bash process (`bash -c`).
    ///
    /// Unlike sessions, exec **waits for the process to exit**. Bounded by
    /// `timeout_ms` so a runaway command cannot hang the agent forever; on
    /// timeout or cancel the child is killed and partial stdout/stderr are
    /// returned. The final pipe drain is bounded too (`EXEC_DRAIN_GRACE`):
    /// a backgrounded grandchild that inherits the pipes must not hold the
    /// result hostage past the child's own exit. Every return path caps each
    /// stream at `max_bytes` (see `truncate_middle_lossy`) so one flooding
    /// command cannot saturate the model context.
    async fn run_oneshot(
        &self,
        command: &str,
        cwd: Option<&str>,
        timeout_ms: u64,
        max_bytes: usize,
        cancel: crate::core::CancelToken,
    ) -> generative_model::ToolResult {
        let mut cmd = BASH.tokio_command();
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
        let stdout_buf = Arc::new(Mutex::new(CappedCapture::default()));
        let stderr_buf = Arc::new(Mutex::new(CappedCapture::default()));
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
                let out = render_locked_capture(&stdout_buf, max_bytes);
                let err = render_locked_capture(&stderr_buf, max_bytes);
                generative_model::ToolResult::err(format!(
                    "exec cancelled\n\
                     stdout:\n{out}\n\
                     stderr:\n{err}"
                ))
            }
            Outcome::TimedOut => {
                kill_process_group(child_pid);
                let _ = child.start_kill();
                let _ = child.wait().await;
                drain_capture(stdout_task, stderr_task).await;
                let out = render_locked_capture(&stdout_buf, max_bytes);
                let err = render_locked_capture(&stderr_buf, max_bytes);
                generative_model::ToolResult::text(format!(
                    "Exit code: None\n\
                     Termination signal: None\n\
                     status: timed_out\n\
                     timeout_ms: {timeout_ms}\n\
                     stdout:\n{out}\n\
                     stderr:\n{err}\n\
                     (exec timed out after {timeout_ms}ms; process group killed)\n"
                ))
            }
            Outcome::Status(status) => {
                drain_capture(stdout_task, stderr_task).await;
                let out = render_locked_capture(&stdout_buf, max_bytes);
                let err = render_locked_capture(&stderr_buf, max_bytes);
                match status {
                    Ok(status) => generative_model::ToolResult::text(format!(
                        "Exit code: {:?}\n\
                         Termination signal: {:?}\n\
                         stdout:\n{out}\n\
                         stderr:\n{err}",
                        status.code(),
                        status.signal(),
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
            let sessions = self.sessions();
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
        let mut cmd = BASH.tokio_command();
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
            let mut sessions = self.sessions();
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
            let mut sessions = self.sessions();
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
        *lock_unpoisoned(&session.stdin) = None;

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
        let sessions = self.sessions();
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
        let sessions = self.sessions();
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
            let mut sessions = self.sessions();
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
            *lock_unpoisoned(&session.stdin) = None;
            kill_session_process(&session);
        }
    }

    async fn write_to_session(&self, session_id: &str, data: &str) -> Result<(), String> {
        // Bump generation so an in-flight collect doesn't treat pre-write idle as done.
        // Take stdin out briefly to write without holding the sessions map lock across await.
        let (stdin_slot, shared) = {
            let sessions = self.sessions();
            let session = sessions
                .get(session_id)
                .ok_or_else(|| format!("unknown session {session_id:?}"))?;
            session.shared.generation.fetch_add(1, Ordering::SeqCst);
            let shared = Arc::clone(&session.shared);
            let stdin = lock_unpoisoned(&session.stdin).take();
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
        {
            let sessions = self.sessions();
            if let Some(session) = sessions.get(session_id)
                && let Ok(mut t) = session.last_used.lock()
            {
                *t = Instant::now();
            }
        }
        shared.notify.notify_waiters();
        Ok(())
    }

    fn return_stdin(&self, session_id: &str, stdin: ChildStdin) {
        let sessions = self.sessions();
        let Some(session) = sessions.get(session_id) else {
            return;
        };
        *lock_unpoisoned(&session.stdin) = Some(stdin);
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
            let sessions = self.sessions();
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
    /// Oldest bytes dropped since the last read because a stream exceeded
    /// [`SESSION_STREAM_CAP`] (surfaced in the next snapshot, then reset).
    dropped_bytes: usize,
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
    /// Oldest buffered bytes dropped (stream over the in-memory cap) since
    /// the previous read.
    bytes_dropped: usize,
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
        if self.bytes_dropped > 0 {
            out.push_str(&format!(
                "(oldest {} buffered bytes dropped: output exceeded the in-memory session cap; read more often or filter at the source)\n",
                self.bytes_dropped
            ));
        }
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

/// In-memory accumulation cap per exec stream. `max_bytes` caps what a call
/// *returns*; without this a flooding child (`yes`, a verbose build) grows
/// the capture buffer without bound for the exec's whole lifetime. Head and
/// rolling tail are kept so the final middle-elision still sees both ends.
const EXEC_CAPTURE_CAP: usize = 512 * 1024;

/// One exec stream captured under the accumulation cap: the first
/// `EXEC_CAPTURE_CAP / 2` bytes, a rolling tail of the same size, and a count
/// of bytes dropped at the seam between them.
#[derive(Default)]
struct CappedCapture {
    head: Vec<u8>,
    tail: Vec<u8>,
    omitted: usize,
}

impl CappedCapture {
    fn push(&mut self, chunk: &[u8]) {
        let head_cap = EXEC_CAPTURE_CAP / 2;
        let tail_cap = EXEC_CAPTURE_CAP - head_cap;
        let mut rest = chunk;
        if self.head.len() < head_cap {
            let take = (head_cap - self.head.len()).min(rest.len());
            self.head.extend_from_slice(&rest[..take]);
            rest = &rest[take..];
        }
        if rest.is_empty() {
            return;
        }
        self.tail.extend_from_slice(rest);
        // Drop-oldest in blocks (not per chunk) so a flood costs a few
        // memmoves, not one per 4 KiB read.
        if self.tail.len() > tail_cap + 64 * 1024 {
            let excess = self.tail.len() - tail_cap;
            self.tail.drain(..excess);
            self.omitted += excess;
        }
    }
}

/// Read `reader` to EOF, appending to `buf` as data arrives (not just at EOF),
/// so a bounded drain still observes everything read so far.
fn spawn_capture<R>(mut reader: R, buf: Arc<Mutex<CappedCapture>>) -> tokio::task::JoinHandle<()>
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
                        g.push(&chunk[..n]);
                    }
                }
            }
        }
    })
}

/// Render a shared capture buffer under the return cap (empty on a poisoned
/// lock — the capture task panicked, so there is nothing better to report).
fn render_locked_capture(buf: &Arc<Mutex<CappedCapture>>, max_bytes: usize) -> String {
    buf.lock()
        .map(|g| render_capture(&g, max_bytes))
        .unwrap_or_default()
}

/// The shared middle-elision shape: verbatim head, one honest marker, tail.
fn middle_elision(
    head: &[u8],
    tail: &[u8],
    omitted: usize,
    total: usize,
    max_bytes: usize,
) -> String {
    format!(
        "{}\n[... {omitted} bytes omitted ({total} bytes total, max_bytes={max_bytes}); \
         filter with grep/head/tail, redirect to a file, or raise max_bytes ...]\n{}",
        String::from_utf8_lossy(head),
        String::from_utf8_lossy(tail),
    )
}

/// Render a captured exec stream under the *return* cap `max_bytes`, folding
/// any capture-time loss into the same middle-elision marker.
///
/// Unlike session output (buffered; the agent can `read` the rest later),
/// exec output is gone once the call returns, so the cut must keep the most
/// informative parts — and those are the ends: build/test tools print the
/// root-cause error first and the failure summary last. Cuts are byte-exact;
/// a split UTF-8 char renders as U+FFFD at the seam.
fn render_capture(capture: &CappedCapture, max_bytes: usize) -> String {
    if capture.omitted == 0 {
        let mut all = Vec::with_capacity(capture.head.len() + capture.tail.len());
        all.extend_from_slice(&capture.head);
        all.extend_from_slice(&capture.tail);
        return truncate_middle_lossy(&all, max_bytes);
    }
    let total = capture.head.len() + capture.omitted + capture.tail.len();
    let head_keep = max_bytes.div_ceil(2).min(capture.head.len());
    let tail_keep = (max_bytes / 2).min(capture.tail.len());
    let omitted = total - head_keep - tail_keep;
    middle_elision(
        &capture.head[..head_keep],
        &capture.tail[capture.tail.len() - tail_keep..],
        omitted,
        total,
        max_bytes,
    )
}

/// Cap contiguous bytes at `max_bytes`, keeping the head and tail halves and
/// eliding the middle with a marker (see [`render_capture`]).
fn truncate_middle_lossy(bytes: &[u8], max_bytes: usize) -> String {
    if bytes.len() <= max_bytes {
        return String::from_utf8_lossy(bytes).into_owned();
    }
    let head_len = max_bytes.div_ceil(2);
    let tail_len = max_bytes / 2;
    let omitted = bytes.len() - head_len - tail_len;
    middle_elision(
        &bytes[..head_len],
        &bytes[bytes.len() - tail_len..],
        omitted,
        bytes.len(),
        max_bytes,
    )
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

/// Per-stream in-memory cap for session output. A long-lived session nobody
/// `read`s (a dev server left running for hours) must not grow resident
/// memory without bound; the newest output is what a late reader wants, so
/// the oldest is dropped — in blocks, so a flood costs a few memmoves.
const SESSION_STREAM_CAP: usize = 2 * 1024 * 1024;

/// Enforce [`SESSION_STREAM_CAP`] on one stream; returns bytes dropped.
fn cap_session_stream(stream: &mut Vec<u8>) -> usize {
    if stream.len() <= SESSION_STREAM_CAP {
        return 0;
    }
    // Drop down to 3/4 of the cap so the next overflows are amortized.
    let excess = stream.len() - (SESSION_STREAM_CAP - SESSION_STREAM_CAP / 4);
    stream.drain(..excess);
    excess
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
                        let dropped = match kind {
                            StreamKind::Stdout => {
                                b.stdout.extend_from_slice(&buf[..n]);
                                cap_session_stream(&mut b.stdout)
                            }
                            StreamKind::Stderr => {
                                b.stderr.extend_from_slice(&buf[..n]);
                                cap_session_stream(&mut b.stderr)
                            }
                        };
                        b.dropped_bytes = b.dropped_bytes.saturating_add(dropped);
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
    let (stdout, stderr, exit_code, exit_signal, exited, bytes_dropped) = {
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
                    bytes_dropped: 0,
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
            std::mem::take(&mut b.dropped_bytes),
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
        bytes_dropped,
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
    /// Max bytes of output to return. Default 4096.
    /// - start/write/read: combined stdout+stderr drain cap; excess stays buffered — `read` again for more.
    /// - exec: per-stream cap; head and tail are kept, the middle elided. Elided bytes are unrecoverable.
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
mod tests;
