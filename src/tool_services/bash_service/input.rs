//! The `bash` tool contract: model-facing description, wire input, and its
//! validation into an [`Action`].

use super::*;

/// Model-facing description of the `bash` tool.
pub(super) fn tool_description() -> String {
    format!(
        "Executes bash commands and manages long-lived interactive sessions \
        (shells, Python REPLs, SSH, etc.).\n\n\
        Actions:\n\
        - exec (default): one-shot `bash -c <command>`; waits for process exit \
        or `background_after`. Without background_after, `timeout_ms` defaults to \
        {DEFAULT_EXEC_TIMEOUT_MS} ms / {exec_default_s}s; an explicit timeout is \
        capped at {MAX_EXEC_TIMEOUT_MS} ms / {exec_max_min} min. \
        Returns exit code, signal, stdout, stderr. Each stream is capped at \
        `max_bytes` (default {DEFAULT_MAX_BYTES}): over the cap, the head and tail \
        are kept and the middle is elided with a `[... N bytes omitted ...]` marker. \
        Elided exec output is unrecoverable — pipe through grep/head/tail or \
        redirect to a file when you expect a flood, or raise `max_bytes` when you \
        truly need more. Prefer `exec` for finite commands (builds, tests, \
        installs). Use `background_after` in milliseconds (1..={MAX_EXEC_TIMEOUT_MS}) \
        to return a session_id after waiting while the same command keeps running. \
        Prefer this over adding shell timeout wrappers or `timeout_ms` merely to \
        stop waiting: inspect later with read and decide whether to wait or close. \
        With background_after, an omitted timeout imposes no kill deadline. \
        An explicitly supplied `timeout_ms` remains a hard kill deadline after \
        automatic backgrounding. If timeout expires before or at background_after, it kills \
        the command as usual. Without background_after, exec keeps its existing \
        foreground timeout. A user can background a running call from the browser; it then returns \
        a session_id for read/signal/close, and its original timeout no longer \
        applies. Continue other work instead of immediately waiting on it again.\n\
        - start: spawn a long-lived process **in the background**. Requires \
        `session_id`. `command` is the program line (default: `bash -i`). Optional \
        `stdin` is written after spawn. Returns a snapshot; the process keeps \
        running.\n\
        - write: write `stdin` to a session, then collect a snapshot (process stays \
        alive).\n\
        - read: collect more output without writing (process stays alive).\n\
        - signal: deliver a signal to the session's **process group**, then return \
        a snapshot like `write`. `signal` is `int` (default), `term`, or `hup`; \
        SIGKILL is `close`. `int` is a terminal Ctrl-C: a child that handles SIGINT \
        survives and stays writable — a nested `myco` cancels its in-flight turn and \
        returns to its prompt, so this interrupts a child without losing its \
        session. A child with **no** handler dies instead (`exit_signal: 2`); \
        sessions have no job control, so there is no separate foreground group to \
        signal and the whole group gets it. Either way the session is not reaped — \
        that is still `close`.\n\
        - close: kill and reap a session.\n\
        - list: list live sessions. The `start` admission limit ({MAX_SESSIONS} \
        retained sessions) is shared by every agent on the host while `list` shows \
        only yours. Promoting an already-running exec can exceed that limit. If \
        `start` reports too many sessions, close your own unused sessions and retry.\n\n\
        For start/write/read, the child runs in the background. `start` without initial \
        stdin returns after an idle gap even when the child is silent; `write` / `read` \
        wait for output first. Calls otherwise return after the output goes idle \
        (`idle_ms`, default {DEFAULT_IDLE_MS}), a hard timeout \
        (`timeout_ms`, default {DEFAULT_TIMEOUT_MS} ms / {session_default_s}s; max \
        {MAX_TIMEOUT_MS} ms / {session_max_min} min), a byte cap (`max_bytes`, \
        default {DEFAULT_MAX_BYTES}), or completion of the process and its output \
        streams — then returns partial output \
        with status timed_out / truncated / running while the session stays live. \
        Raise `timeout_ms` when you need to wait longer for quiet interactive \
        programs. `exit_code` / `exit_signal` describe the launched process; \
        `status: exited` means that process exited and both output streams closed. \
        Descendants may keep output open; use `read` for later output or `close` \
        to stop the process group.\n\n\
        **Managed jobs:** recognized nohup invocations in exec/start command fields are rejected \
        before execution. Run the program in the foreground of the command and let \
        background_after or start retain it. Avoid nohup, disown, and shell `&` for \
        detached work: managed handles keep status, output, ownership, and stop controls \
        visible. Use list to rediscover handles and read to collect later output.\n\n\
        **Terminal:** sessions use pipes, with no TTY or job control. Prefer \
        `host=<alias>` with a plain shell (`command: \"bash\"` or `\"bash -l\"`) \
        for long-lived remote work. Check whether a program supports noninteractive \
        operation before using raw `ssh -tt`; many programs work without a terminal.\n\n\
        **Working directory:** use `cd /path && command` in `exec` / `start`. \
        A directory change in an exec call affects only that call; a live shell \
        session keeps directory changes made through `write`.",
        exec_default_s = DEFAULT_EXEC_TIMEOUT_MS / 1000,
        exec_max_min = MAX_EXEC_TIMEOUT_MS / 60_000,
        session_default_s = DEFAULT_TIMEOUT_MS / 1000,
        session_max_min = MAX_TIMEOUT_MS / 60_000,
    )
}

/// Wire input for the `bash` tool (flat object for Anthropic-friendly JSON Schema).
///
/// `deny_unknown_fields` keeps this struct honest about the schema it advertises:
/// [`crate::tool_services::tool_input_schema`] declares
/// `additionalProperties: false`, so a gateway that does not enforce the schema
/// must not be the only thing standing between a typo'd field and a half-executed
/// call. The harness strips the routing `host` before dispatch, so it never
/// reaches here.
#[derive(
    Clone, Debug, schemars::JsonSchema, serde::Deserialize, serde::Serialize, PartialEq, Eq,
)]
#[serde(deny_unknown_fields)]
pub struct Input {
    /// Action to perform. Defaults to `exec` when omitted (or when only `command` is set).
    #[serde(default)]
    action: Option<ActionKind>,
    /// Command line. For `exec`: run via `bash -c`. For `start`: program line (default `bash -i`).
    #[serde(default)]
    command: Option<String>,
    /// Agent-chosen session name for start/write/read/close.
    #[serde(default)]
    session_id: Option<String>,
    /// Bytes to write to the session's stdin (`start` / `write`).
    #[serde(default)]
    stdin: Option<String>,
    /// Signal to deliver for `signal`. Default `int`.
    #[serde(default)]
    signal: Option<SignalKind>,
    /// Hard wait ceiling in milliseconds.
    /// - start/write/read: default 30000 (30s), max 1800000 (30 min); early return on idle/byte cap.
    /// - exec: default 60000 (60s) unless background_after is set, max 1800000 (30 min); kills the process group on expiry.
    ///
    /// Prefer background_after for choosing when to stop waiting without killing work.
    #[serde(default)]
    timeout_ms: Option<u64>,
    /// Exec only: yield the same command as a background session after this many milliseconds.
    /// Positive, at most 1800000 (30 min). Omit to keep waiting in the foreground.
    /// Prefer this over shell timeout wrappers or timeout_ms for deciding when to stop waiting.
    /// An explicit timeout_ms still kills the process group at its original deadline after yielding.
    #[serde(default)]
    #[schemars(range(min = 1, max = 1_800_000))]
    background_after: Option<u64>,
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
    Signal,
    Close,
    List,
}

/// Signals a supervisor may deliver to a session. Deliberately small: SIGKILL
/// is `close` (which also reaps the session), and anything outside this set has
/// no established use against a session's process group.
#[derive(
    Clone, Copy, Debug, schemars::JsonSchema, serde::Deserialize, serde::Serialize, PartialEq, Eq,
)]
#[serde(rename_all = "snake_case")]
pub(super) enum SignalKind {
    /// Interrupt, as a terminal Ctrl-C. A nested `myco` cancels its in-flight
    /// turn and stays at the prompt.
    Int,
    /// Ask the process to terminate.
    Term,
    /// Hangup.
    Hup,
}

impl SignalKind {
    pub(super) fn as_libc(self) -> libc::c_int {
        match self {
            SignalKind::Int => libc::SIGINT,
            SignalKind::Term => libc::SIGTERM,
            SignalKind::Hup => libc::SIGHUP,
        }
    }

    pub(super) fn name(self) -> &'static str {
        match self {
            SignalKind::Int => "SIGINT",
            SignalKind::Term => "SIGTERM",
            SignalKind::Hup => "SIGHUP",
        }
    }
}

/// Internal validated action after parsing [`Input`].
#[derive(Debug)]
pub(super) enum Action {
    Exec {
        command: String,
        timeout_ms: Option<u64>,
        background_after: Option<u64>,
        max_bytes: usize,
    },
    Start {
        session_id: String,
        command: Option<String>,
        stdin: Option<String>,
        wait: Wait,
    },
    /// Act on a live session, then collect a snapshot of its output.
    Interact {
        session_id: String,
        step: Step,
        wait: Wait,
    },
    Close {
        session_id: String,
    },
    List,
}

/// What one session interaction does before collecting output.
#[derive(Debug)]
pub(super) enum Step {
    Write(String),
    Read,
    Signal(SignalKind),
}

/// Bounds on collecting a session snapshot: a hard ceiling, an idle gap, and
/// a byte cap, whichever comes first.
#[derive(Clone, Copy, Debug)]
pub(super) struct Wait {
    pub timeout_ms: u64,
    pub idle_ms: u64,
    pub max_bytes: usize,
}

/// Message for a `bash` tool use whose input object carries nothing at all.
///
/// Every [`Input`] field is optional (the tool infers `exec` from a bare
/// `command`), so `{}` deserializes cleanly and only surfaces later as a
/// missing action. Name the actual problem, and show what a usable call
/// looks like.
pub(super) const EMPTY_INPUT_ERROR: &str = "empty bash input: the tool use carried no parameters. \
     Pass `command` for a one-shot run (e.g. {\"command\": \"cd /repo && ls -la\"}), \
     or `action` with its parameters (exec/start/write/read/signal/close/list; every \
     session action but `list` also needs `session_id`).";

/// JSON type name for error messages (`null`, `string`, `array`, …).
pub(super) fn json_type_name(value: &serde_json::Value) -> &'static str {
    match value {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "a boolean",
        serde_json::Value::Number(_) => "a number",
        serde_json::Value::String(_) => "a string",
        serde_json::Value::Array(_) => "an array",
        serde_json::Value::Object(_) => "an object",
    }
}

/// True when no field of `input` was set (`{}`, or explicit nulls).
fn is_empty_object(input: &Input) -> bool {
    // Destructured so a new field cannot silently fall out of this check.
    let Input {
        action,
        command,
        session_id,
        stdin,
        signal,
        timeout_ms,
        background_after,
        idle_ms,
        max_bytes,
    } = input;
    action.is_none()
        && command.is_none()
        && session_id.is_none()
        && stdin.is_none()
        && signal.is_none()
        && timeout_ms.is_none()
        && background_after.is_none()
        && idle_ms.is_none()
        && max_bytes.is_none()
}

/// Reject blank strings where a value is required: `bash -c ""` exits 0 with no
/// output, and a blank `session_id` would name an unreachable session — both
/// look like the tool did nothing rather than like a malformed call.
fn require_non_blank(field: &str, value: &str) -> Result<(), String> {
    if value.trim().is_empty() {
        return Err(format!("`{field}` must be a non-empty string"));
    }
    Ok(())
}

fn required_session_id(input: &Input, action: &str) -> Result<String, String> {
    let session_id = input
        .session_id
        .clone()
        .ok_or_else(|| format!("{action} requires `session_id`"))?;
    require_non_blank("session_id", &session_id)?;
    Ok(session_id)
}

pub(super) fn resolve_action(input: &Input) -> Result<Action, String> {
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
            } else if is_empty_object(input) {
                // A tool use with `input: {}` (or only null fields) carries no
                // work at all. Say so explicitly instead of leaving the agent to
                // guess which of the six actions it forgot to name.
                return Err(EMPTY_INPUT_ERROR.into());
            } else {
                return Err(
                    "missing action (and no command/session_id to infer one from); \
                            pass `action` (exec/start/write/read/close/list)"
                        .into(),
                );
            }
        }
    };

    let background_after = match input.background_after {
        Some(_) if !matches!(kind, ActionKind::Exec) => {
            return Err("background_after is only supported for exec; session actions already keep the process running".into());
        }
        Some(millis) if millis == 0 || millis > MAX_EXEC_TIMEOUT_MS => {
            return Err(format!(
                "background_after must be 1..={MAX_EXEC_TIMEOUT_MS} milliseconds"
            ));
        }
        Some(millis) => Some(millis),
        None => None,
    };

    // Explicit timeout_ms above the safety ceiling is rejected (not silently
    // clamped). Defaults are generous enough for normal interactive work;
    // background_after yields longer exec jobs; an explicit timeout remains a
    // hard ceiling. Cancel still aborts mid-wait.
    let wait = || -> Result<Wait, String> {
        let timeout_ms = match input.timeout_ms {
            None => DEFAULT_TIMEOUT_MS,
            Some(t) if t > MAX_TIMEOUT_MS => {
                return Err(format!(
                    "session timeout_ms={t} exceeds max {MAX_TIMEOUT_MS}ms (30 min); pass ≤{MAX_TIMEOUT_MS}"
                ));
            }
            Some(t) => t.max(1),
        };
        Ok(Wait {
            timeout_ms,
            idle_ms: input.idle_ms.unwrap_or(DEFAULT_IDLE_MS),
            max_bytes: input.max_bytes.unwrap_or(DEFAULT_MAX_BYTES),
        })
    };
    let interact = |action: &str, step: Step| -> Result<Action, String> {
        Ok(Action::Interact {
            session_id: required_session_id(input, action)?,
            step,
            wait: wait()?,
        })
    };

    match kind {
        ActionKind::Exec => {
            let command = input
                .command
                .clone()
                .ok_or_else(|| "exec requires `command`".to_string())?;
            require_non_blank("command", &command)?;
            command_policy::validate(&command)?;
            let timeout_ms = match input.timeout_ms {
                None if background_after.is_some() => None,
                None => Some(DEFAULT_EXEC_TIMEOUT_MS),
                Some(t) if t > MAX_EXEC_TIMEOUT_MS => {
                    return Err(format!(
                        "exec timeout_ms={t} exceeds max {MAX_EXEC_TIMEOUT_MS}ms (30 min); pass ≤{MAX_EXEC_TIMEOUT_MS}"
                    ));
                }
                Some(t) => Some(t.max(1)),
            };
            Ok(Action::Exec {
                command,
                timeout_ms,
                background_after,
                max_bytes: input.max_bytes.unwrap_or(DEFAULT_MAX_BYTES),
            })
        }
        ActionKind::Start => {
            let session_id = required_session_id(input, "start")?;
            if let Some(command) = input.command.as_deref() {
                require_non_blank("command", command)?;
                command_policy::validate(command)?;
            }
            Ok(Action::Start {
                session_id,
                command: input.command.clone(),
                stdin: input.stdin.clone(),
                wait: wait()?,
            })
        }
        ActionKind::Write => {
            let session_id = required_session_id(input, "write")?;
            let stdin = input
                .stdin
                .clone()
                .ok_or_else(|| "write requires `stdin`".to_string())?;
            Ok(Action::Interact {
                session_id,
                step: Step::Write(stdin),
                wait: wait()?,
            })
        }
        ActionKind::Read => interact("read", Step::Read),
        ActionKind::Signal => interact(
            "signal",
            Step::Signal(input.signal.unwrap_or(SignalKind::Int)),
        ),
        ActionKind::Close => Ok(Action::Close {
            session_id: required_session_id(input, "close")?,
        }),
        ActionKind::List => Ok(Action::List),
    }
}
