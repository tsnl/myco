//! The shared vocabulary: the types every layer of myco speaks — durable
//! conversation records, tool calls and results, terminal screens, the
//! keyboard lock. Serde-only and wasm-safe: the session store persists
//! these, the host protocol and the HTTP wire carry them, the model layer
//! projects them onto providers, and the browser deserializes them.
//!
//! Nothing here knows about HTTP, providers, or hosts — `myco-api` re-
//! exports this whole crate and adds the API surface on top.

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Conversation records
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

// ---------------------------------------------------------------------------
// Interactive surfaces: the keyboard lock and the terminal screen
// ---------------------------------------------------------------------------

/// Who holds a keyboard — a shell's, or a subagent child's.
///
/// The lock gates *writes* only — both sides always read. A surface starts
/// held by whoever it exists for (agent-started shells serve the agent; a
/// terminal the user opened is theirs), and while one side holds it the
/// other's writes fail politely, in words that say exactly that — because
/// two writers interleaving keystrokes into one stdin is worse than either
/// waiting. Every handoff is recorded in the transcript.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShellLock {
    Assistant,
    User,
}

/// A rendered terminal screen: what a `cols`×`rows` window shows now — as
/// plain text (`text`, for the agent's `screenshot` action and simple
/// clients) and as styled runs (`runs`, for a client drawing a real
/// terminal). One definition serves the host protocol and the HTTP wire.
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
