//! NDJSON protocol between [`crate::machines::host::HostController`] and [`crate::machines::host::HostWorker`].
//!
//! Direction is controller-initiated:
//! - [`Request`]  — controller → worker
//! - [`Response`] — worker → controller

use crate::machines::tool_services::{ShellLock, ShellOverview, ShellScreen};
use myco_api::{ToolResult, ToolUse};

/// Controller → worker message.
///
/// The `Shell*` requests are the **observer surface**: a person watching or
/// driving a live bash session from the web rail. They carry no `agent_id` —
/// watching is not ownership — and every one is answered under its `id` like
/// a tool call, so remote shells behave exactly like local ones.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Request {
    /// First message after connect; worker replies with [`Response::HelloOk`].
    Hello,
    /// Execute one tool use. Worker responds with [`Response::ToolResult`].
    ToolCall {
        /// Correlation id (unique per in-flight call on this pipe).
        id: String,
        /// Agent that owns this call (session ownership on the host).
        agent_id: uuid::Uuid,
        tool_use: ToolUse,
    },
    /// Reap agent-owned host state (bash sessions, …). Fire-and-forget: the
    /// worker does not reply (host process exit is the hard guarantee).
    AgentFinished { agent_id: uuid::Uuid },
    /// List live shells → [`Response::Shells`].
    ShellList { id: String },
    /// Scrollback from `from` → [`Response::ShellTail`].
    ShellTail {
        id: String,
        shell: String,
        from: u64,
        max_bytes: u64,
    },
    /// A user keystroke line (requires the user keyboard lock on the host) →
    /// [`Response::Shell`].
    ShellInput {
        id: String,
        shell: String,
        data: String,
    },
    /// Move the keyboard → [`Response::Shell`] with `previous_lock` set.
    ShellLock {
        id: String,
        shell: String,
        lock: ShellLock,
    },
    /// Rendered terminal screen → [`Response::ShellScreen`].
    ShellScreenshot { id: String, shell: String },
}

impl Request {
    /// Encode as one NDJSON line (including trailing newline).
    pub fn encode(&self) -> Result<Vec<u8>, String> {
        let mut line = serde_json::to_string(self).map_err(|e| e.to_string())?;
        line.push('\n');
        Ok(line.into_bytes())
    }

    /// Parse one NDJSON line into a request.
    pub fn decode(line: &str) -> Result<Self, String> {
        serde_json::from_str(line.trim()).map_err(|e| format!("parse request {line:?}: {e}"))
    }
}

/// Worker → controller message.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Response {
    HelloOk {
        version: String,
    },
    ToolResult {
        id: String,
        result: ToolResult,
    },
    /// Reply to [`Request::ShellList`].
    Shells {
        id: String,
        shells: Vec<ShellOverview>,
    },
    /// Reply to [`Request::ShellTail`]. `data` is the scrollback lossily
    /// UTF-8 — same shape the HTTP layer serves a terminal.
    ShellTail {
        id: String,
        from: u64,
        end: u64,
        data: String,
        running: bool,
        lock: ShellLock,
    },
    /// Reply to [`Request::ShellInput`] / [`Request::ShellLock`]: the shell's
    /// fresh overview. For a lock move, `previous_lock` says who held the
    /// keyboard before, so the caller can tell a transition from a re-take.
    Shell {
        id: String,
        shell: ShellOverview,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        previous_lock: Option<ShellLock>,
    },
    /// Reply to [`Request::ShellScreenshot`].
    ShellScreen {
        id: String,
        screen: ShellScreen,
    },
    Error {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        message: String,
    },
}

impl Response {
    /// The request id this response answers, when it answers one — what the
    /// controller's demux routes on, so a new variant that forgets to appear
    /// here would strand its waiter.
    pub fn correlation_id(&self) -> Option<&str> {
        match self {
            Response::HelloOk { .. } => None,
            Response::ToolResult { id, .. }
            | Response::Shells { id, .. }
            | Response::ShellTail { id, .. }
            | Response::Shell { id, .. }
            | Response::ShellScreen { id, .. } => Some(id),
            Response::Error { id, .. } => id.as_deref(),
        }
    }
}

impl Response {
    /// Encode as one NDJSON line (including trailing newline).
    pub fn encode(&self) -> Result<Vec<u8>, String> {
        let mut line = serde_json::to_string(self).map_err(|e| e.to_string())?;
        line.push('\n');
        Ok(line.into_bytes())
    }

    /// Parse one NDJSON line into a response.
    pub fn decode(line: &str) -> Result<Self, String> {
        serde_json::from_str(line.trim()).map_err(|e| format!("parse response {line:?}: {e}"))
    }

    /// Serialize and write this response to `writer`, then flush.
    pub async fn write_to<W>(&self, writer: &mut W) -> Result<(), String>
    where
        W: tokio::io::AsyncWriteExt + Unpin,
    {
        let bytes = self.encode()?;
        writer
            .write_all(&bytes)
            .await
            .map_err(|e| format!("write: {e}"))?;
        writer.flush().await.map_err(|e| format!("flush: {e}"))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use myco_api::{Content, ToolResult, ToolUse};
    use serde_json::json;

    #[test]
    fn request_tool_call_roundtrip() {
        let msg = Request::ToolCall {
            id: "1".into(),
            agent_id: uuid::Uuid::nil(),
            tool_use: ToolUse {
                id: "toolu_1".into(),
                name: "bash".into(),
                input: json!({"command": "echo hi"}),
            },
        };
        let line = String::from_utf8(msg.encode().unwrap()).unwrap();
        assert!(line.contains(r#""type":"tool_call""#));
        let back = Request::decode(&line).unwrap();
        match back {
            Request::ToolCall { id, tool_use, .. } => {
                assert_eq!(id, "1");
                assert_eq!(tool_use.name, "bash");
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    /// The observer messages survive the wire, lock spelling included — and
    /// every id-carrying response names its correlation id, or the demux
    /// would strand its waiter.
    #[test]
    fn shell_observer_messages_roundtrip() {
        let req = Request::ShellTail {
            id: "7".into(),
            shell: "term".into(),
            from: 42,
            max_bytes: 65536,
        };
        let line = String::from_utf8(req.encode().unwrap()).unwrap();
        assert!(line.contains(r#""type":"shell_tail""#), "{line}");
        match Request::decode(&line).unwrap() {
            Request::ShellTail { shell, from, .. } => {
                assert_eq!(shell, "term");
                assert_eq!(from, 42);
            }
            other => panic!("unexpected {other:?}"),
        }

        let resp = Response::Shell {
            id: "7".into(),
            shell: crate::machines::tool_services::ShellOverview {
                id: "term".into(),
                cmdline: "cat".into(),
                running: true,
                exit_code: None,
                lock: ShellLock::User,
                end_offset: 9,
                created_secs: 1,
                idle_secs: 0,
                pty: true,
            },
            previous_lock: Some(ShellLock::Assistant),
        };
        let line = String::from_utf8(resp.encode().unwrap()).unwrap();
        assert!(line.contains(r#""lock":"user""#), "{line}");
        let back = Response::decode(&line).unwrap();
        assert_eq!(back.correlation_id(), Some("7"));
        match back {
            Response::Shell {
                shell,
                previous_lock,
                ..
            } => {
                assert_eq!(shell.lock, ShellLock::User);
                assert!(shell.pty);
                assert_eq!(previous_lock, Some(ShellLock::Assistant));
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn response_tool_result_roundtrip() {
        let msg = Response::ToolResult {
            id: "1".into(),
            result: ToolResult::text("hi").with_id("toolu_1"),
        };
        let line = String::from_utf8(msg.encode().unwrap()).unwrap();
        let back = Response::decode(&line).unwrap();
        match back {
            Response::ToolResult { id, result } => {
                assert_eq!(id, "1");
                assert!(!result.is_error);
                assert!(matches!(&result.content[0], Content::Text { text } if text == "hi"));
            }
            other => panic!("unexpected {other:?}"),
        }
    }
}
