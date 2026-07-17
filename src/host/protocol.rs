//! NDJSON protocol between [`crate::host::HostController`] and [`crate::host::HostWorker`].
//!
//! Direction is controller-initiated:
//! - [`Request`]  — controller → worker
//! - [`Response`] — worker → controller

use crate::generative_model::{ToolResult, ToolSpec, ToolUse};

/// Controller → worker message.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Request {
    /// First message after connect; worker replies with [`Response::HelloOk`].
    Hello {},
    /// Execute one tool use. Worker responds with [`Response::ToolResult`].
    ToolCall {
        /// Correlation id (unique per in-flight call on this pipe).
        id: String,
        /// Agent that owns this call (session ownership on the host).
        agent_id: uuid::Uuid,
        tool_use: ToolUse,
    },
    /// Reap agent-owned host state (bash sessions, …).
    AgentFinished { agent_id: uuid::Uuid },
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
        name: String,
        version: String,
        tools: Vec<ToolSpec>,
    },
    ToolResult {
        id: String,
        result: ToolResult,
    },
    AgentFinishedOk {
        agent_id: uuid::Uuid,
    },
    Error {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        message: String,
    },
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
    use crate::generative_model::{Content, ToolResult, ToolUse};
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
