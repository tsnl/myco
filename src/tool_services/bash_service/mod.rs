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

mod background;
mod capture;
mod command_policy;
mod input;
mod operation;
mod process;
mod snapshot;

use background::{BackgroundReason, Promotion};
use capture::{
    CappedCapture, EXEC_CAPTURE_CAP, cap_session_stream, drain_capture, kill_and_capture,
    render_capture, render_exec_capture, spawn_reader,
};
#[cfg(test)]
use capture::{SESSION_STREAM_CAP, truncate_middle_lossy};
use input::{
    Action, EMPTY_INPUT_ERROR, Input, SignalKind, Step, Wait, json_type_name, resolve_action,
};
use operation::SessionOperation;
use process::ProcessOwner;
use snapshot::{SessionSnapshot, SnapshotStatus, process_status};

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
/// Final drain when a session is closed.
const CLOSE_DRAIN: Wait = Wait {
    timeout_ms: 1_000,
    idle_ms: 100,
    max_bytes: DEFAULT_MAX_BYTES,
};

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
            description: input::tool_description(),
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
            // Anything but a JSON object (missing input, a bare string, a list)
            // is malformed: report it as such rather than letting serde's
            // "invalid type" message stand in for the real problem.
            if !tool_use.input.is_object() {
                return generative_model::ToolResult::err(format!(
                    "bash input must be a JSON object, got {}. {EMPTY_INPUT_ERROR}",
                    json_type_name(&tool_use.input),
                ));
            }
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
            self.execute(action, ctx).await
        })
    }

    fn on_agent_finished(&self, agent_id: Uuid) {
        self.reap_owner(agent_id);
    }

    fn resources(&self, agent_id: Uuid) -> Vec<crate::core::ToolResource> {
        self.sessions()
            .iter()
            .filter(|(_, session)| session.owner == agent_id)
            .map(|(id, session)| {
                let buffer = lock_unpoisoned(&session.shared.buffer);
                crate::core::ToolResource {
                    tool: "bash".into(),
                    id: id.clone(),
                    details: serde_json::json!({
                        "command": session.cmdline, "pid": session.process.pid(),
                        "instance_id": session.shared.instance_id,
                        "started_at": session.shared.started_at,
                        "duration_ms": buffer.finished.map(|end| end.duration_since(session.shared.started).as_millis() as u64),
                        "process_exited": buffer.exited, "output_closed": buffer.is_finished(),
                        "exit_code": buffer.exit_code, "exit_signal": buffer.exit_signal,
                        "exec_timeout_ms": buffer.exec_timeout_ms,
                    }),
                }
            })
            .collect()
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
        ctx: HostDispatchContext,
    ) -> generative_model::ToolResult {
        let owner = ctx.agent_id;
        match action {
            Action::Exec {
                command,
                timeout_ms,
                background_after,
                max_bytes,
            } => {
                self.run_oneshot(&command, timeout_ms, background_after, max_bytes, ctx)
                    .await
            }
            Action::Start {
                session_id,
                command,
                stdin,
                wait,
            } => {
                self.session_start(&session_id, command.as_deref(), stdin.as_deref(), wait, ctx)
                    .await
            }
            Action::Interact {
                session_id,
                step,
                wait,
            } => self.session_interact(&session_id, step, wait, ctx).await,
            Action::Close { session_id } => self.session_close(&session_id, owner).await,
            Action::List => self.session_list(owner),
        }
    }

    /// Run `command` in a fresh bash process (`bash -c`).
    ///
    /// Exec waits for exit, or yields a retained session on background_after
    /// or an explicit background request. Foreground waiting is bounded by
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
        timeout_ms: Option<u64>,
        background_after: Option<u64>,
        max_bytes: usize,
        ctx: HostDispatchContext,
    ) -> generative_model::ToolResult {
        let deadline =
            timeout_ms.map(|millis| tokio::time::Instant::now() + Duration::from_millis(millis));
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
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(error) => {
                return generative_model::ToolResult::err(format!(
                    "Error spawning command: {error}"
                ));
            }
        };
        let child_pid = child.id();

        let stdout = child.stdout.take().expect("stdout piped");
        let stderr = child.stderr.take().expect("stderr piped");
        // Shared buffers, appended to as data arrives, so the bounded drain
        // below can report partial output even when a reader never hits EOF.
        let shared = Arc::new(SessionShared {
            instance_id: Uuid::new_v4().to_string(),
            started_at: chrono::Utc::now().timestamp_millis(),
            started: Instant::now(),
            buffer: Mutex::new(OutputBuffer {
                exec_capture: Some(Default::default()),
                ..Default::default()
            }),
            notify: Notify::new(),
            generation: AtomicU64::new(0),
        });
        let readers = [
            spawn_reader(stdout, StreamKind::Stdout, shared.clone()),
            spawn_reader(stderr, StreamKind::Stderr, shared.clone()),
        ];

        // When cancel/timeout wins, select drops the wait future so we can
        // kill the process group + wait without a conflicting &mut Child borrow.
        enum Outcome {
            Cancelled,
            TimedOut,
            Background(BackgroundReason),
            Status(std::io::Result<std::process::ExitStatus>),
        }
        let outcome = tokio::select! {
            biased;
            _ = ctx.cancel.cancelled() => Outcome::Cancelled,
            _ = background::until(deadline) => Outcome::TimedOut,
            status = child.wait() => Outcome::Status(status),
            _ = ctx.background.cancelled() => Outcome::Background(BackgroundReason::User),
            reason = background::after(background_after) => Outcome::Background(reason),
        };

        match outcome {
            Outcome::Background(reason) => {
                let promotion = Promotion {
                    reason,
                    deadline: if matches!(reason, BackgroundReason::AfterMillis(_)) {
                        deadline.zip(timeout_ms)
                    } else {
                        None
                    },
                };
                self.background_exec(command, ctx.agent_id, child, shared, max_bytes, promotion)
            }
            Outcome::Cancelled => {
                let (out, err) =
                    kill_and_capture(&mut child, child_pid, readers, &shared, max_bytes).await;
                generative_model::ToolResult::err(format!(
                    "exec cancelled\n\
                     stdout:\n{out}\n\
                     stderr:\n{err}"
                ))
            }
            Outcome::TimedOut => {
                let timeout_ms =
                    timeout_ms.expect("no deadline without an explicit or legacy timeout");
                let (out, err) =
                    kill_and_capture(&mut child, child_pid, readers, &shared, max_bytes).await;
                generative_model::ToolResult::text(format!(
                    "Exit code: None\n\
                     Termination signal: None\n\
                     status: timed_out\n\
                     timeout_ms: {timeout_ms}\n\
                     stdout:\n{out}\n\
                     stderr:\n{err}\n\
                     (exec timed out after {timeout_ms}ms; process group killed)\n"
                ))
                .with_status(format!(
                    "timed out after {timeout_ms}ms; process group killed"
                ))
            }
            Outcome::Status(status) => {
                drain_capture(readers).await;
                let (out, err) = render_exec_capture(&shared, max_bytes);
                match status {
                    Ok(status) => generative_model::ToolResult::text(format!(
                        "Exit code: {:?}\n\
                         Termination signal: {:?}\n\
                         stdout:\n{out}\n\
                         stderr:\n{err}",
                        status.code(),
                        status.signal(),
                    ))
                    .with_status(
                        process_status(status.code(), status.signal())
                            .unwrap_or_else(|| "process exited".into()),
                    ),
                    Err(error) => generative_model::ToolResult::err(format!(
                        "Error executing command: {error}"
                    )),
                }
            }
        }
    }

    async fn session_start(
        &self,
        session_id: &str,
        command: Option<&str>,
        stdin: Option<&str>,
        wait: Wait,
        ctx: HostDispatchContext,
    ) -> generative_model::ToolResult {
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
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                return generative_model::ToolResult::err(format!(
                    "failed to spawn session command {cmdline:?}: {e}"
                ));
            }
        };

        let child_stdin = child.stdin.take().expect("stdin piped");
        let stdout = child.stdout.take().expect("stdout piped");
        let stderr = child.stderr.take().expect("stderr piped");

        let shared = Arc::new(SessionShared {
            instance_id: Uuid::new_v4().to_string(),
            started_at: chrono::Utc::now().timestamp_millis(),
            started: Instant::now(),
            buffer: Mutex::new(OutputBuffer::default()),
            notify: Notify::new(),
            generation: AtomicU64::new(0),
        });

        // Reader tasks push into the shared buffer and notify waiters.
        spawn_reader(stdout, StreamKind::Stdout, Arc::clone(&shared));
        spawn_reader(stderr, StreamKind::Stderr, Arc::clone(&shared));
        let process = ProcessOwner::retain(child, Arc::clone(&shared));

        let session = Session {
            owner: ctx.agent_id,
            cmdline: cmdline.to_string(),
            stdin: Arc::new(Mutex::new(Some(child_stdin))),
            shared,
            created_at: Instant::now(),
            last_used: Arc::new(Mutex::new(Instant::now())),
            process,
        };
        let operation = session.operation(session_id);

        {
            let mut sessions = self.sessions();
            // Re-check under lock in case of concurrent start with same id.
            // A losing start drops its process owner, killing the group.
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
            sessions.insert(session_id.to_string(), session);
        }

        // Optional initial stdin, then collect a first snapshot.
        if let Some(data) = stdin
            && let Err(e) = operation.write(data).await
        {
            return generative_model::ToolResult::err(format!(
                "session {session_id:?} started but initial stdin write failed: {e}"
            ));
        }

        match operation
            .collect(wait, stdin.is_none_or(str::is_empty), &ctx)
            .await
        {
            Ok(s) => s.tool_result(),
            Err(e) => generative_model::ToolResult::err(e),
        }
    }

    /// Write, read, or signal a live session, then collect a snapshot. A
    /// signal's interesting output is the child's *reaction*, so it is
    /// collected like a write's instead of costing the caller a round trip.
    async fn session_interact(
        &self,
        session_id: &str,
        step: Step,
        wait: Wait,
        ctx: HostDispatchContext,
    ) -> generative_model::ToolResult {
        let operation = match self.operation(session_id, ctx.agent_id) {
            Ok(operation) => operation,
            Err(error) => return generative_model::ToolResult::err(error),
        };
        let acted = match &step {
            Step::Write(stdin) => operation.write(stdin).await,
            Step::Read => Ok(()),
            Step::Signal(signal) => operation.signal(*signal),
        };
        if let Err(error) = acted {
            return generative_model::ToolResult::err(error);
        }
        let snapshot = match operation.collect(wait, false, &ctx).await {
            Ok(snapshot) => snapshot,
            Err(error) => return generative_model::ToolResult::err(error),
        };
        let mut result = snapshot.tool_result();
        if let Step::Signal(signal) = step {
            let text = format!(
                "{}\n(sent {} to the session process group)\n",
                snapshot.format(),
                signal.name()
            );
            result.content = vec![generative_model::Content::Text { text }];
        }
        result
    }

    async fn session_close(&self, session_id: &str, owner: Uuid) -> generative_model::ToolResult {
        let session = {
            let mut sessions = self.sessions();
            match sessions.get(session_id) {
                Some(s) if s.owner != owner => {
                    return generative_model::ToolResult::err(format!(
                        "session {session_id:?} is owned by another myco session"
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

        // Drop stdin (EOF), keeping the process owner until output is drained.
        *lock_unpoisoned(&session.stdin) = None;

        // Drain any final buffered output (short wait).
        let snapshot = session
            .operation(session_id)
            .collect_output(CLOSE_DRAIN, false, &CancelToken::new(), &CancelToken::new())
            .await;

        drop(session);

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
            let exited = {
                let b = lock_unpoisoned(&s.shared.buffer);
                match (b.exited, b.is_finished()) {
                    (false, _) => "running".into(),
                    (true, true) => format!("exited({:?})", b.exit_code),
                    (true, false) => format!("exited({:?}); output still open", b.exit_code),
                }
            };
            let last_used = lock_unpoisoned(&s.last_used).elapsed().as_secs();
            lines.push(format!(
                "- id={id} owner={} cmdline={:?} status={exited} last_used_s_ago={last_used} created_s_ago={}",
                crate::core::uuid_simple_hex(s.owner),
                s.cmdline,
                s.created_at.elapsed().as_secs()
            ));
        }
        lines.push(String::new());
        generative_model::ToolResult::text(lines.join("\n"))
    }

    fn operation(&self, session_id: &str, owner: Uuid) -> Result<SessionOperation, String> {
        let sessions = self.sessions();
        let session = sessions
            .get(session_id)
            .ok_or_else(|| format!("unknown session {session_id:?}"))?;
        if session.owner != owner {
            return Err(format!(
                "session {session_id:?} is owned by another myco session"
            ));
        }
        Ok(session.operation(session_id))
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
        drop(victims);
    }
}

// --- session internals -------------------------------------------------------

struct Session {
    /// Agent that started this session; only this agent may write/read/close it.
    owner: Uuid,
    cmdline: String,
    stdin: Arc<Mutex<Option<ChildStdin>>>,
    shared: Arc<SessionShared>,
    created_at: Instant,
    last_used: Arc<Mutex<Instant>>,
    /// Own the group identity until the handle is closed, including after exit.
    process: Arc<ProcessOwner>,
}

struct SessionShared {
    instance_id: String,
    started_at: i64,
    started: Instant,
    buffer: Mutex<OutputBuffer>,
    notify: Notify,
    /// Bumped on each write so waiters reset their idle clock.
    generation: AtomicU64,
}

#[derive(Default)]
struct OutputBuffer {
    exec_capture: Option<(CappedCapture, CappedCapture)>,
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
    /// Captured output streams whose readers have finished (stdout and stderr).
    eof_streams: u8,
    finished: Option<Instant>,
    /// Explicit exec ceiling retained after an automatic background yield.
    exec_timeout_ms: Option<u64>,
}

impl OutputBuffer {
    fn record_completion(&mut self) {
        if self.is_finished() {
            self.finished.get_or_insert_with(Instant::now);
        }
    }

    fn is_finished(&self) -> bool {
        self.exited && self.eof_streams >= 2
    }
}

#[derive(Clone, Copy)]
enum StreamKind {
    Stdout,
    Stderr,
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod background_after_tests;
