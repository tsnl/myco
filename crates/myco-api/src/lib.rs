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
    /// Why the most recent turn produced nothing, if it failed; cleared when
    /// the next turn starts.
    #[serde(default)]
    pub last_error: Option<String>,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    NotFound,
    Conflict,
    BadRequest,
    #[default]
    Internal,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiError {
    pub error: String,
    #[serde(default)]
    pub kind: ErrorKind,
}

impl ApiError {
    pub fn new(kind: ErrorKind, error: impl Into<String>) -> Self {
        Self {
            error: error.into(),
            kind,
        }
    }
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.error)
    }
}

impl std::error::Error for ApiError {}

/// A session's live event feed, frontend-agnostic.
pub type EventStream = std::pin::Pin<Box<dyn futures::Stream<Item = StreamEvent> + Send>>;

/// The myco interface: one async contract, two implementations — the server
/// object does the work in-process, the HTTP client speaks to a remote one.
/// Holders of `dyn MycoApi` cannot tell the difference.
#[async_trait::async_trait]
pub trait MycoApi: Send + Sync {
    async fn list_sessions(&self) -> Result<Vec<SessionSummary>, ApiError>;
    async fn create_session(&self, req: CreateSession) -> Result<SessionSummary, ApiError>;
    async fn session_detail(&self, id: &str) -> Result<SessionDetail, ApiError>;
    /// Queue one user turn (input queues while a turn runs).
    async fn post_message(&self, id: &str, req: PostMessage) -> Result<Poll, ApiError>;
    async fn poll(&self, id: &str, since: usize) -> Result<Poll, ApiError>;
    /// Subscribe to the live feed; makes the session resident.
    async fn events(&self, id: &str) -> Result<EventStream, ApiError>;
    async fn cancel(&self, id: &str) -> Result<Poll, ApiError>;
    /// Summarize into a successor session (new id; watch for `Compacted`).
    async fn compact(&self, id: &str) -> Result<Poll, ApiError>;
    /// Retire the live agent task (the session stays on disk, resumable).
    async fn retire(&self, id: &str) -> Result<Poll, ApiError>;
    async fn models(&self) -> Result<Models, ApiError>;
}

/// One SSE event on `/api/sessions/<id>/events` — a live projection of the
/// agent's event stream.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StreamEvent {
    TurnStarted,
    TextDelta {
        text: String,
    },
    ThinkingDelta {
        text: String,
    },
    ToolStarted {
        name: String,
        input: String,
    },
    TurnFinished,
    /// The turn ended without a reply (provider error, cancellation); the
    /// message is also on `Poll::last_error` until the next turn starts.
    TurnFailed {
        message: String,
    },
    /// Compaction replaced this session with a successor — follow the new id.
    Compacted {
        predecessor: String,
        successor: String,
    },
}
