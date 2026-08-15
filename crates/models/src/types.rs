//! The conversation vocabulary the model layer speaks, ported from v2's
//! `myco-types` — the subset that describes a model turn. Plain serde
//! data; nothing here knows about HTTP, providers, or the bus. The chat
//! kind stores its own transcript shape and projects into these at
//! request time.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TurnEndReason {
    EndTurn,
    MaxTokens,
    ToolUse,
    /// Provider-specific / unknown stop reason (owned so histories can
    /// serialize cleanly).
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
    /// Model thinking *summary* (history + live UI).
    ///
    /// Stored in chat history for resume, but **stripped when backends
    /// compose the next API request** (not echoed as CoT). Prefer provider
    /// summary channels over raw reasoning text.
    Thinking {
        text: String,
        /// Opaque provider signature (Anthropic). Not re-sent on
        /// subsequent turns.
        signature: Option<String>,
        /// True for redacted/encrypted thinking placeholders with no
        /// plaintext.
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
    /// Context occupied by the prompt = total input tokens (cached input is
    /// a subset, already included).
    pub fn context_tokens(self) -> u64 {
        self.input_tokens
    }

    /// Fold a later usage report into this one, keeping known fields when
    /// the later report omits them (providers split usage across stream
    /// events).
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
