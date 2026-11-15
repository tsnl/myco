//! Session snapshots: what one interaction drained from a retained
//! process, and how it is rendered for the model.

use super::*;

pub(super) struct SessionSnapshot {
    pub(super) instance_id: String,
    pub(super) session_id: String,
    pub(super) owner: Uuid,
    pub(super) cmdline: String,
    pub(super) status: SnapshotStatus,
    pub(super) exit_code: Option<i32>,
    pub(super) exit_signal: Option<i32>,
    pub(super) background_output: bool,
    pub(super) exec_timeout_ms: Option<u64>,
    pub(super) stdout: String,
    pub(super) stderr: String,
    pub(super) bytes_returned: usize,
    /// Oldest buffered bytes dropped (stream over the in-memory cap) since
    /// the previous read.
    pub(super) bytes_dropped: usize,
}

#[derive(Clone, Copy)]
pub(super) enum SnapshotStatus {
    Running,
    Backgrounded(BackgroundReason),
    Exited,
    TimedOut,
    Truncated,
}

impl SnapshotStatus {
    fn as_str(self) -> &'static str {
        match self {
            SnapshotStatus::Running => "running",
            SnapshotStatus::Backgrounded(_) => "backgrounded",
            SnapshotStatus::Exited => "exited",
            SnapshotStatus::TimedOut => "timed_out",
            SnapshotStatus::Truncated => "truncated",
        }
    }
}

pub(super) fn process_status(code: Option<i32>, signal: Option<i32>) -> Option<String> {
    signal
        .map(|signal| format!("signal {signal}"))
        .or_else(|| code.map(|code| format!("exit {code}")))
}

impl SessionSnapshot {
    pub(super) fn tool_result(&self) -> generative_model::ToolResult {
        let mut result = generative_model::ToolResult::text(self.format());
        result.resource = Some(generative_model::ToolResourceRef {
            id: self.session_id.clone(),
            instance_id: self.instance_id.clone(),
        });
        result.status = if let Some(timeout_ms) = self.exec_timeout_ms {
            Some(format!(
                "timed out after {timeout_ms}ms; process group killed"
            ))
        } else if matches!(self.status, SnapshotStatus::Backgrounded(_)) {
            Some("backgrounded".into())
        } else {
            process_status(self.exit_code, self.exit_signal)
        };
        result
    }

    pub(super) fn format(&self) -> String {
        let mut out = String::new();
        if let SnapshotStatus::Backgrounded(reason) = self.status {
            match reason {
                BackgroundReason::User => out.push_str("User backgrounded this call. "),
                BackgroundReason::AfterMillis(millis) => out.push_str(&format!(
                    "background_after={millis}ms elapsed; command continues in the background. "
                )),
            }
            out.push_str("Continue with other work; use bash read on this session_id and the same host for later output, or bash close to stop it.\n");
        }
        out.push_str(&format!("session_id: {}\n", self.session_id));
        out.push_str(&format!(
            "owner: {}\n",
            crate::core::uuid_simple_hex(self.owner)
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
        if self.background_output {
            out.push_str("(process exited; output still open, descendants may still be running — call read/close)\n");
        }
        if let Some(timeout_ms) = self.exec_timeout_ms {
            out.push_str(&format!("(exec reached its explicit timeout_ms={timeout_ms}; process group killed; read remaining output or close the session)\n"));
            return out;
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
            SnapshotStatus::Running | SnapshotStatus::Backgrounded(_) => {
                out.push_str("(session still running; call read/write/close as needed)\n");
            }
            SnapshotStatus::Exited => {
                out.push_str("(process exited; call close to reap the session)\n");
            }
        }
        out
    }
}
