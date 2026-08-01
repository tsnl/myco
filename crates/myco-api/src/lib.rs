//! Wire types shared by the myco API server (`myco::server`) and its clients
//! (`myco-gui`). Serde-only: this crate must stay wasm-compatible and free of
//! server-side dependencies.
//!
//! The transcript surface is deliberately **entry-based**: every transcript
//! item has a stable index today and can grow a parent pointer later without
//! breaking the protocol (sessions-as-trees is a planned store change, not a
//! planned protocol change).

use serde::{Deserialize, Serialize};

/// One session in the browser list.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSummary {
    pub id: String,
    pub title: Option<String>,
    pub model: String,
    pub created_at: String,
    pub updated_at: String,
    pub message_count: usize,
    pub snippet: String,
    /// Live in this server process (an agent task exists for it).
    pub live: bool,
    /// An agent turn is currently running.
    pub busy: bool,
}

/// One rendered transcript entry. Lossy plaintext projection of the
/// underlying message history — enough for a minimal client.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entry {
    /// Stable index into the rendered transcript.
    pub index: usize,
    /// "user" | "assistant" | "thinking" | "tool_use" | "tool_result"
    pub role: String,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionDetail {
    pub summary: SessionSummary,
    pub entries: Vec<Entry>,
}

/// `GET /api/sessions/<id>/poll?since=N` — entries with `index >= since`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Poll {
    pub busy: bool,
    /// Total entries currently renderable; poll again with this as `since`.
    pub total: usize,
    pub entries: Vec<Entry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PostMessage {
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateSession {
    /// Model key from the catalog; `None` = server default.
    #[serde(default)]
    pub model: Option<String>,
    /// Create as a hidden subagent child of this session (nested agents).
    #[serde(default)]
    pub parent_session: Option<String>,
    /// With `parent_session`: seed the child with the parent's saved
    /// conversation (context fork) instead of starting empty.
    #[serde(default)]
    pub fork: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Models {
    pub models: Vec<String>,
    pub default_model: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiError {
    pub error: String,
}
