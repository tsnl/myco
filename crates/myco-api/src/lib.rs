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
    /// Filed away: still readable, but out of the default listing.
    #[serde(default)]
    pub archived: bool,
    /// Live in this server process (an agent task exists for it).
    pub live: bool,
    /// An agent turn is currently running.
    pub busy: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionDetail {
    pub summary: SessionSummary,
    pub entries: Vec<Entry>,
}

/// `GET /api/sessions/<id>/poll?since=N` — entries from offset `N`.
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

/// `PATCH /sessions/<id>`: the session-metadata fields a client may set.
/// Absent fields are left alone.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UpdateSession {
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub archived: Option<bool>,
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

/// Who the caller is, as the server sees them. Every implementation of
/// [`MycoApi`] is bound to exactly one identity — the in-process server to
/// the roster's local user, an HTTP client to whoever its token belongs to —
/// so this is a property of the handle, not a parameter on each call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Identity {
    pub id: String,
    pub name: String,
}

impl From<Identity> for Author {
    fn from(i: Identity) -> Author {
        Author::User {
            id: i.id,
            name: i.name,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    NotFound,
    Conflict,
    BadRequest,
    /// No credential, or one the server does not recognize.
    Unauthorized,
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
    /// Sessions in the store, newest first. Archived sessions are excluded
    /// unless `include_archived`.
    async fn list_sessions(&self, include_archived: bool) -> Result<Vec<SessionSummary>, ApiError>;
    async fn create_session(&self, req: CreateSession) -> Result<SessionSummary, ApiError>;
    async fn session_detail(&self, id: &str) -> Result<SessionDetail, ApiError>;
    /// Set session metadata (title, archived). Absent fields are unchanged.
    async fn update_session(
        &self,
        id: &str,
        req: UpdateSession,
    ) -> Result<SessionSummary, ApiError>;
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
    /// The identity this handle acts as. Anything it writes is attributed
    /// here, so a client can show the user who they are posting as.
    async fn whoami(&self) -> Result<Identity, ApiError>;
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

// ---------------------------------------------------------------------------
// Conversation vocabulary
//
// These types are the durable shape of a conversation, so they live here
// rather than in the model layer: `myco-session` stores them, the wire
// carries them, and `myco-models` projects them onto whatever a provider
// wants. Plain serde data — nothing here knows about HTTP or providers.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TurnEndReason {
    EndTurn,
    MaxTokens,
    ToolUse,
    /// Provider-specific / unknown stop reason (owned so sessions can serialize cleanly).
    Other(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolUse {
    pub id: String,
    pub name: String,
    pub input: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResult {
    pub id: String,
    pub content: Vec<Content>,
    pub is_error: bool,
}

impl ToolResult {
    pub fn ok(content: Vec<Content>) -> Self {
        Self {
            id: String::new(),
            content,
            is_error: false,
        }
    }

    pub fn text(text: impl Into<String>) -> Self {
        Self {
            id: String::new(),
            content: vec![Content::Text { text: text.into() }],
            is_error: false,
        }
    }

    pub fn err(text: impl Into<String>) -> Self {
        Self {
            id: String::new(),
            content: vec![Content::Text { text: text.into() }],
            is_error: true,
        }
    }

    pub fn with_id(mut self, id: impl Into<String>) -> Self {
        self.id = id.into();
        self
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Content {
    Text {
        text: String,
    },
    Image {
        source: String,
    },
    /// Model thinking *summary* (session history + live UI).
    ///
    /// Stored in agent/session history for resume, but **stripped when backends
    /// compose the next API request** (not echoed as CoT). Prefer provider
    /// summary channels over raw reasoning text.
    Thinking {
        text: String,
        /// Opaque provider signature (Anthropic). Not re-sent on subsequent turns.
        signature: Option<String>,
        /// True for redacted/encrypted thinking placeholders with no plaintext.
        redacted: bool,
    },
}

/// Clone only answer blocks (`Text` / `Image`), dropping thinking.
pub fn answer_content(content: &[Content]) -> Vec<Content> {
    content
        .iter()
        .filter(|c| matches!(c, Content::Text { .. } | Content::Image { .. }))
        .cloned()
        .collect()
}

/// Token counts for one generate call. `cached_input_tokens` is a subset of
/// `input_tokens`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenUsage {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default)]
    pub cached_input_tokens: u64,
}

impl TokenUsage {
    /// Context occupied by the prompt = total input tokens (cached input is a
    /// subset, already included).
    pub fn context_tokens(self) -> u64 {
        self.input_tokens
    }

    /// Fold a later usage report into this one, keeping known fields when the
    /// later report omits them (providers split usage across stream events).
    pub fn merge(self, next: TokenUsage) -> TokenUsage {
        fn pick(prev: u64, next: u64) -> u64 {
            if next != 0 { next } else { prev }
        }
        TokenUsage {
            input_tokens: pick(self.input_tokens, next.input_tokens),
            output_tokens: pick(self.output_tokens, next.output_tokens),
            cached_input_tokens: pick(self.cached_input_tokens, next.cached_input_tokens),
        }
    }
}

/// Who produced an entry. Attribution is intrinsic to the record, not
/// decoration: a shared session is unreadable without it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Author {
    /// A person. `name` is denormalized so a transcript stays readable even
    /// if the user roster changes underneath it.
    User { id: String, name: String },
    /// The model, under the catalog key it ran as.
    Agent { model: String },
    /// The runtime itself: session stamps, compaction notes.
    System,
}

impl Author {
    pub fn name(&self) -> &str {
        match self {
            Author::User { name, .. } => name,
            Author::Agent { model } => model,
            Author::System => "system",
        }
    }
}

/// One record in a session: who, when, and what.
///
/// An entry maps one-to-one onto a provider message, so projection in either
/// direction is lossless — but the session owns it, and the model layer only
/// borrows it at request time.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entry {
    pub author: Author,
    pub at: chrono::DateTime<chrono::Utc>,
    pub body: EntryBody,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EntryBody {
    /// Prose (and images) from a person.
    User { content: Vec<Content> },
    /// One assistant turn: prose, thinking, and any tool calls it made.
    Agent {
        content: Vec<Content>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        tool_uses: Vec<ToolUse>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        turn_end: Option<TurnEndReason>,
    },
    /// Results for the tool calls in the preceding agent entry.
    ToolResults { results: Vec<ToolResult> },
}

impl Entry {
    pub fn user(author: Author, content: Vec<Content>) -> Self {
        Self {
            author,
            at: chrono::Utc::now(),
            body: EntryBody::User { content },
        }
    }

    pub fn agent(
        model: impl Into<String>,
        content: Vec<Content>,
        tool_uses: Vec<ToolUse>,
        turn_end: Option<TurnEndReason>,
    ) -> Self {
        Self {
            author: Author::Agent {
                model: model.into(),
            },
            at: chrono::Utc::now(),
            body: EntryBody::Agent {
                content,
                tool_uses,
                turn_end,
            },
        }
    }

    pub fn tool_results(model: impl Into<String>, results: Vec<ToolResult>) -> Self {
        Self {
            author: Author::Agent {
                model: model.into(),
            },
            at: chrono::Utc::now(),
            body: EntryBody::ToolResults { results },
        }
    }

    /// Plain text of this entry, for snippets and search.
    pub fn text(&self) -> String {
        match &self.body {
            EntryBody::User { content } | EntryBody::Agent { content, .. } => content_text(content),
            EntryBody::ToolResults { results } => results
                .iter()
                .map(|r| content_text(&r.content))
                .collect::<Vec<_>>()
                .join("\n"),
        }
    }
}

/// Concatenated text of a content run, images noted rather than inlined.
pub fn content_text(content: &[Content]) -> String {
    content
        .iter()
        .map(|c| match c {
            Content::Text { text } => text.clone(),
            Content::Thinking { text, .. } => text.clone(),
            Content::Image { .. } => "[image]".to_string(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}
