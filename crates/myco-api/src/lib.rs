//! Wire types shared by the myco API server (`myco::server`) and its clients
//! (`myco-gui`). Serde-only: this crate must stay wasm-compatible and free of
//! server-side dependencies.
//!
//! The transcript surface is deliberately **entry-based**: every transcript
//! item has a stable index today and can grow a parent pointer later without
//! breaking the protocol (sessions-as-trees is a planned store change, not a
//! planned protocol change).

use serde::{Deserialize, Serialize};

pub mod mention;
pub mod tool_display;

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
    /// Per-session reasoning-effort override (`"low"`…`"max"`); `None` means
    /// the model's configured effort applies.
    #[serde(default)]
    pub effort: Option<String>,
    /// Context tokens the last turn reported in use (the prompt side).
    #[serde(default)]
    pub context_tokens: Option<u64>,
    /// The model's context window, for the meter's denominator.
    #[serde(default)]
    pub context_window: Option<u64>,
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
    /// Context tokens in use after the most recent turn, when known.
    #[serde(default)]
    pub context_tokens: Option<u64>,
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
    /// Switch the session to another catalog model key. A live session
    /// rebuilds its agent's model between turns; the change applies from the
    /// next turn onward.
    #[serde(default)]
    pub model: Option<String>,
    /// Set (`"low"`…`"max"`) or clear (`""`) the per-session effort override.
    #[serde(default)]
    pub effort: Option<String>,
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

/// Who holds a keyboard — a shell's, or a subagent child's. The lock gates
/// writes only: reading is always open to both sides, whichever way it points.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShellLockMode {
    Assistant,
    User,
}

/// One live bash session, as the shells rail lists it. Shells exist per
/// host — `(host, id)` addresses one — and remote hosts serve the same
/// surface as local over the host protocol.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Shell {
    /// Host the session runs on (`"local"` or a configured remote).
    pub host: String,
    pub id: String,
    /// Display name a person gave it; the id remains the address the agent
    /// (and every endpoint) uses.
    #[serde(default)]
    pub title: Option<String>,
    pub cmdline: String,
    pub running: bool,
    pub exit_code: Option<i32>,
    pub lock: ShellLockMode,
    /// Absolute end of the scrollback; tail from here for only-new bytes.
    pub end_offset: u64,
    /// Running under a pty — render the screen (see `shell_screen`), not the
    /// raw scrollback.
    #[serde(default)]
    pub pty: bool,
}

/// A rendered terminal screen: what a `cols`×`rows` window shows now
/// (`GET /api/sessions/<id>/shells/<host>/<shell>/screen`) — as plain text
/// (`text`) and as styled runs (`runs`) for a client drawing a real
/// terminal.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ShellScreen {
    pub cols: u16,
    pub rows: u16,
    pub cursor_row: u16,
    pub cursor_col: u16,
    pub text: String,
    /// Styled cell runs, row-major; the cursor cell is its own run.
    #[serde(default)]
    pub runs: Vec<ScreenRun>,
    #[serde(default)]
    pub cursor_hidden: bool,
    /// DECCKM: arrow keys should send SS3 (`\x1bOA`…) instead of CSI.
    #[serde(default)]
    pub application_cursor: bool,
}

/// A run of consecutive same-styled cells on one screen row. Colors are
/// concrete `#rrggbb` (indexed colors already resolved server-side); `None`
/// means the terminal default.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScreenRun {
    pub row: u16,
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fg: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bg: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub bold: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub italic: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub underline: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub inverse: bool,
    /// The cursor sits on (the first cell of) this run.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub cursor: bool,
}

/// `POST /api/sessions/<id>/shells/<host>/<shell>/resize` — fit the terminal
/// to the viewer's window (requires the user keyboard lock).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShellResize {
    pub cols: u16,
    pub rows: u16,
}

/// `POST /api/sessions/<id>/shells/<host>` — open a terminal on that host.
/// It is a real bash session owned by the session's agent (fully addressable
/// by its bash tool), starting **user-held** — whoever opened it is typing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateShell {
    /// Session name; `None` picks the first free `term-N`.
    #[serde(default)]
    pub shell: Option<String>,
    /// Child command; `None` runs an interactive bash.
    #[serde(default)]
    pub command: Option<String>,
    /// A real terminal by default — user terminals exist to be typed into.
    #[serde(default = "default_true")]
    pub pty: bool,
    #[serde(default)]
    pub cols: Option<u16>,
    #[serde(default)]
    pub rows: Option<u16>,
}

fn default_true() -> bool {
    true
}

/// `POST /api/sessions/<id>/shells/<host>/<shell>/rename` — set or clear the
/// display name (`""` clears). Organization only: the id is the address.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShellRename {
    pub title: String,
}

/// Client→server frame on a shell's WebSocket
/// (`GET /api/sessions/<id>/shells/<host>/<shell>/ws`, JSON text frames).
///
/// The socket is the *interactive* transport — ordered keystrokes in, screen
/// pushes out, one connection per open terminal. It carries nothing the REST
/// surface doesn't: input and resize obey the same keyboard lock, and a
/// client without the socket (scripts, `myco.py`) loses latency, not
/// capability.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ShellWsInput {
    /// Raw bytes typed or pasted (requires the user keyboard lock).
    Input { data: String },
    /// Fit the terminal (requires the user keyboard lock).
    Resize { cols: u16, rows: u16 },
}

/// Server→client frame on a shell's WebSocket.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ShellWsOutput {
    /// The screen changed; pushed coalesced, never faster than it renders.
    Screen { screen: ShellScreen },
    /// An input/resize frame was refused (lock not held, shell gone). The
    /// socket stays up — watching is always allowed.
    Error { message: String },
}

/// `GET /api/sessions/<id>/shells`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Shells {
    pub shells: Vec<Shell>,
}

/// One non-consuming scrollback read
/// (`GET /api/sessions/<id>/shells/<shell>?from=N`). `from` in the response
/// is where `data` actually starts — ahead of the request when the viewer
/// fell behind the ring, so a client knows bytes were skipped rather than
/// silently missing them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ShellTailChunk {
    pub from: u64,
    pub end: u64,
    /// Scrollback bytes, lossily UTF-8 (a terminal shows what it can).
    pub data: String,
    pub running: bool,
    pub lock: ShellLockMode,
}

/// `POST /api/sessions/<id>/shells/<shell>/input` — a line the user typed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShellInput {
    pub data: String,
}

/// `POST /api/sessions/<id>/shells/<shell>/lock` — take or return the
/// keyboard. Also the body of `/subagents/<child>/lock`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShellLockRequest {
    pub lock: ShellLockMode,
}

/// One live subagent child, as the work rail lists it — the `subagent` tool's
/// hidden sessions, surfaced. A child is a full session (its id is a URL),
/// so this carries only what the rail shows; the child's transcript comes
/// from the ordinary session endpoints.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Subagent {
    pub id: String,
    pub model: String,
    /// A turn is running in the child right now.
    pub busy: bool,
    /// The shell keyboard lock's twin: while a person holds the child, the
    /// parent agent's `subagent` calls to it are refused until it is handed
    /// back.
    pub lock: ShellLockMode,
}

/// `GET /api/sessions/<id>/subagents`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Subagents {
    pub subagents: Vec<Subagent>,
}

/// A successful `POST /api/auth/token` — the OAuth 2.0 access-token response
/// (RFC 6749 §5.1). Field names are the spec's, not ours: a stock OAuth2
/// client must be able to read this.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccessToken {
    pub access_token: String,
    /// Always `"bearer"`; present because the spec requires it.
    pub token_type: String,
    /// Lifetime in seconds from issuance.
    pub expires_in: i64,
    /// Who the token speaks for. An extension to the spec, which permits
    /// additional parameters — it saves the client an immediate `/whoami`.
    pub user: Identity,
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

    /// Live bash sessions across the session's hosts (empty when the session
    /// is not resident — shells live and die with the agent task). A host
    /// that is down or dormant lists nothing rather than erroring.
    async fn shells(&self, id: &str) -> Result<Shells, ApiError>;
    /// Non-consuming scrollback read from absolute offset `from`.
    async fn shell_tail(
        &self,
        id: &str,
        host: &str,
        shell: &str,
        from: u64,
    ) -> Result<ShellTailChunk, ApiError>;
    /// Type into a user-locked shell. What was typed is echoed into the
    /// scrollback and recorded in the transcript as a non-waking message, so
    /// the agent reads it at its next boundary.
    async fn shell_input(
        &self,
        id: &str,
        host: &str,
        shell: &str,
        data: String,
    ) -> Result<Shell, ApiError>;
    /// Take or return the shell's keyboard; transitions are recorded in the
    /// transcript the same way. Idempotent re-takes are silent.
    async fn shell_lock(
        &self,
        id: &str,
        host: &str,
        shell: &str,
        lock: ShellLockMode,
    ) -> Result<Shell, ApiError>;
    /// The shell's rendered terminal screen — what a person (or the GUI)
    /// shows for a pty session instead of raw scrollback bytes.
    async fn shell_screen(
        &self,
        id: &str,
        host: &str,
        shell: &str,
    ) -> Result<ShellScreen, ApiError>;
    /// Resize the shell's terminal to fit the viewer's window. Requires the
    /// user keyboard lock; pty children learn via SIGWINCH.
    async fn shell_resize(
        &self,
        id: &str,
        host: &str,
        shell: &str,
        cols: u16,
        rows: u16,
    ) -> Result<Shell, ApiError>;
    /// Open a terminal on `host` for the user: a real bash session owned by
    /// the session's agent (its bash tool can drive it), starting user-held.
    async fn shell_start(&self, id: &str, host: &str, req: CreateShell) -> Result<Shell, ApiError>;
    /// Set or clear a shell's display name (organization; the id stays the
    /// address).
    async fn shell_rename(
        &self,
        id: &str,
        host: &str,
        shell: &str,
        title: Option<String>,
    ) -> Result<Shell, ApiError>;

    /// Live subagent children of this session (empty when it is not resident
    /// — children surface on the rail while their agent tasks exist).
    async fn subagents(&self, id: &str) -> Result<Subagents, ApiError>;
    /// Take or return a subagent child. Transitions are recorded in the
    /// *parent* transcript as non-waking messages, exactly like a shell's
    /// keyboard; idempotent re-takes are silent.
    async fn subagent_lock(
        &self,
        id: &str,
        child: &str,
        lock: ShellLockMode,
    ) -> Result<Subagent, ApiError>;
    /// Post one message into a user-held subagent child (its agent answers
    /// the caller directly). Refused while the child is agent-held; recorded
    /// in the parent transcript the same way shell keystrokes are.
    async fn subagent_input(
        &self,
        id: &str,
        child: &str,
        text: String,
    ) -> Result<Subagent, ApiError>;
}

/// One SSE event on `/api/sessions/<id>/events` — a live projection of the
/// agent's event stream.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StreamEvent {
    TurnStarted,
    /// Somebody posted. Emitted the moment the message is accepted, before any
    /// agent turn it may trigger — so a room full of people see each other in
    /// real time, and see each other *while* the agent is mid-answer.
    ///
    /// Carries the entry itself, so a client places it in the transcript on its
    /// own terms instead of appending text to whatever was last on screen.
    Message {
        entry: Entry,
        /// False when the message was not addressed to the agent, so no turn
        /// follows this event. Clients use it to leave the composer idle.
        wakes_agent: bool,
    },
    TextDelta {
        text: String,
    },
    ThinkingDelta {
        text: String,
    },
    ToolStarted {
        /// The call's id as history stores it — the identity a client keys
        /// the call's card on, so the running card and the finished one are
        /// the same element rather than a removal and an insertion. Display
        /// and dispatch identity only: requests to providers carry minted
        /// positional ids, never this one.
        #[serde(default)]
        id: String,
        name: String,
        /// The call's arguments, structured.
        ///
        /// Not a pre-rendered string: this used to be `input.to_string()` cut
        /// to 200 bytes, which both risked slicing a UTF-8 character in half
        /// and left JSON that no longer parsed — so the client fell back to
        /// treating it as one long quoted string and rendered it escaped.
        /// Deciding how much of a call to show is the reader's end of the
        /// problem, and it needs the structure to do it.
        input: serde_json::Value,
    },
    /// The matching result for an earlier `ToolStarted` (`result.id` pairs
    /// them). Emitted the moment the tool returns, mid-turn — a client
    /// completes the running card in place instead of leaving it spinning
    /// until the turn ends.
    ToolFinished {
        result: ToolResult,
    },
    /// Exactly one per turn, emitted only after the turn's entries are
    /// persisted — a client that refetches on this event reads a transcript
    /// that already contains the answer.
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolUse {
    pub id: String,
    pub name: String,
    pub input: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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

// ---------------------------------------------------------------------------
// Display policy
//
// How a tool call is *summarized* is a property of the conversation, not of
// any one frontend: the terminal and the web client must agree, or the same
// session reads differently depending on where you opened it.
// ---------------------------------------------------------------------------

/// Longest string kept intact when summarizing a tool call's arguments.
pub const TOOL_DISPLAY_STRING_MAX: usize = 72;

/// Truncate to `max_chars`, marking the cut with an ellipsis.
pub fn truncate_display_string(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let trimmed: String = s.chars().take(max_chars.saturating_sub(1)).collect();
    format!("{trimmed}…")
}

/// Deep-copy JSON, replacing long string values with truncated versions for
/// display. Structure is preserved exactly — only leaf strings shrink, so a
/// summarized call still shows every argument it was given.
pub fn truncate_json_strings(value: &serde_json::Value, max_chars: usize) -> serde_json::Value {
    match value {
        serde_json::Value::String(s) => {
            serde_json::Value::String(truncate_display_string(s, max_chars))
        }
        serde_json::Value::Array(items) => serde_json::Value::Array(
            items
                .iter()
                .map(|v| truncate_json_strings(v, max_chars))
                .collect(),
        ),
        serde_json::Value::Object(map) => {
            let mut out = serde_json::Map::with_capacity(map.len());
            for (k, v) in map {
                out.insert(k.clone(), truncate_json_strings(v, max_chars));
            }
            serde_json::Value::Object(out)
        }
        other => other.clone(),
    }
}

/// Pretty-printed JSON arguments for a tool call. `summarize` applies
/// [`truncate_json_strings`]; the verbose view passes `false` and gets the
/// call exactly as the model made it.
pub fn tool_input_json(input: &serde_json::Value, summarize: bool) -> String {
    let value = if summarize {
        truncate_json_strings(input, TOOL_DISPLAY_STRING_MAX)
    } else {
        input.clone()
    };
    serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string())
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

#[cfg(test)]
mod stream_wire_tests {
    use super::*;

    /// `StreamEvent::Message` is the only event that carries a whole record
    /// rather than a fragment: it is what lets a client place someone else's
    /// message in the transcript instead of appending text to whatever was
    /// last on screen. Its shape is therefore load-bearing.
    #[test]
    fn a_message_event_round_trips_with_its_entry_intact() {
        let entry = Entry::user(
            Author::User {
                id: "grace".into(),
                name: "Grace Hopper".into(),
            },
            vec![Content::Text {
                text: "@ada did you see the build?".into(),
            }],
        );
        let ev = StreamEvent::Message {
            entry: entry.clone(),
            wakes_agent: false,
        };

        let json = serde_json::to_value(&ev).expect("serialize");
        assert_eq!(json["type"], "message", "the SSE tag clients switch on");
        assert_eq!(json["wakes_agent"], false);

        let back: StreamEvent = serde_json::from_value(json).expect("deserialize");
        let StreamEvent::Message { entry: got, .. } = back else {
            panic!("expected a message event");
        };
        // The timestamp is the identity a client dedupes on, so it must
        // survive the wire unchanged.
        assert_eq!(got.at, entry.at);
        assert_eq!(got.text(), "@ada did you see the build?");
        match got.author {
            Author::User { id, name } => {
                assert_eq!(id, "grace");
                assert_eq!(name, "Grace Hopper");
            }
            other => panic!("expected a user author, got {other:?}"),
        }
    }
}

#[cfg(test)]
mod display_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn summarized_arguments_keep_every_key_and_cut_only_long_strings() {
        let input = json!({
            "command": "x".repeat(TOOL_DISPLAY_STRING_MAX + 40),
            "timeout_ms": 5000,
            "nested": { "short": "ok" },
        });
        let summary = tool_input_json(&input, true);
        // Structure survives: a collapsed card still shows what was passed.
        assert!(summary.contains("\"command\""), "{summary}");
        assert!(summary.contains("\"timeout_ms\": 5000"), "{summary}");
        assert!(summary.contains("\"short\": \"ok\""), "{summary}");
        // The long value does not.
        assert!(!summary.contains(&"x".repeat(TOOL_DISPLAY_STRING_MAX + 40)));
        assert!(summary.contains('…'), "{summary}");
        // Pretty-printed, not one line.
        assert!(summary.contains('\n'), "{summary}");

        // Verbose is the call exactly as made.
        let full = tool_input_json(&input, false);
        assert!(full.contains(&"x".repeat(TOOL_DISPLAY_STRING_MAX + 40)));
        assert!(!full.contains('…'));
    }

    #[test]
    fn truncation_counts_characters_not_bytes() {
        let s = "é".repeat(TOOL_DISPLAY_STRING_MAX + 5);
        let cut = truncate_display_string(&s, TOOL_DISPLAY_STRING_MAX);
        assert_eq!(cut.chars().count(), TOOL_DISPLAY_STRING_MAX);
        assert!(cut.ends_with('…'));
        // Short strings are returned untouched.
        assert_eq!(truncate_display_string("ok", 10), "ok");
    }
}
