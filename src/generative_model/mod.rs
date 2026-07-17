use std::{pin::pin, sync::Arc};

use futures::{Stream, StreamExt};

use crate::core::*;

mod anthropic;
pub use anthropic::AnthropicBackendConfig;

mod openai_responses;
pub use openai_responses::OpenAIResponsesBackendConfig;

mod sse_parser;
use sse_parser::SseParser;

pub trait GenerativeModel: Send + Sync {
    fn generate(&self, input: &[Message]) -> AsyncStream<Result<MessagePart, GenerateError>>;
}

/// Supported generative models. Wire IDs are provided by [`Model::api_id`].
///
/// Serde / JSON Schema use the same canonical wire strings as [`Model::api_id`].
/// Extra aliases are accepted on deserialize only (for CLI / human input).
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    serde::Serialize,
    serde::Deserialize,
    schemars::JsonSchema,
)]
pub enum Model {
    // Anthropic Messages API
    #[serde(rename = "claude-fable-5")]
    ClaudeFable5,
    #[serde(
        rename = "claude-opus-4-8",
        alias = "claude-opus-4.8",
        alias = "claude-opus-4.8[1m]"
    )]
    ClaudeOpus48,
    #[serde(
        rename = "claude-sonnet-4-6",
        alias = "claude-sonnet-5",
        alias = "claude-sonnet-4.5"
    )]
    ClaudeSonnet5,
    #[serde(rename = "claude-haiku-4-5", alias = "claude-haiku-4.5")]
    ClaudeHaiku45,
    // OpenAI Responses API (xAI / Grok gateways)
    #[serde(
        rename = "grok-4.5-build",
        alias = "grok-4.5",
        alias = "grok-4.5-build[1m]"
    )]
    Grok45Build,
}

/// Which API protocol a model is served over.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BackendKind {
    AnthropicMessages,
    OpenAIResponses,
}

impl std::fmt::Display for BackendKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BackendKind::AnthropicMessages => f.write_str("Anthropic Messages"),
            BackendKind::OpenAIResponses => f.write_str("OpenAI Responses"),
        }
    }
}

/// Reasoning / extended-thinking effort level sent to providers.
///
/// Anthropic adaptive models map this to `output_config.effort`; Haiku-style models
/// map it onto a `thinking.budget_tokens` value. OpenAI/xAI gateways receive it as
/// `reasoning.effort`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Effort {
    Low,
    Medium,
    High,
    Max,
}

impl Effort {
    /// Wire string used by Anthropic `output_config.effort` and OpenAI `reasoning.effort`.
    pub fn as_str(self) -> &'static str {
        match self {
            Effort::Low => "low",
            Effort::Medium => "medium",
            Effort::High => "high",
            Effort::Max => "max",
        }
    }

    /// Approximate Anthropic extended-thinking token budget for non-adaptive models.
    pub fn budget_tokens(self) -> u32 {
        match self {
            Effort::Low => 1_024,
            Effort::Medium => 4_096,
            Effort::High => 16_000,
            Effort::Max => 64_000,
        }
    }

    /// Sensible default for interactive agent sessions.
    pub const DEFAULT: Effort = Effort::High;
}

impl std::fmt::Display for Effort {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for Effort {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "low" | "l" => Ok(Effort::Low),
            "medium" | "med" | "m" => Ok(Effort::Medium),
            "high" | "h" => Ok(Effort::High),
            "max" | "x" => Ok(Effort::Max),
            other => Err(format!(
                "unknown effort {other:?}; expected low|medium|high|max"
            )),
        }
    }
}

/// Optional provider-specific backend settings.
///
/// When omitted from [`GenerativeModelConfig`], a default is chosen from the model via
/// [`BackendConfig::default_for_model`].
#[derive(Debug, Clone)]
pub enum BackendConfig {
    Anthropic(AnthropicBackendConfig),
    OpenAIResponses(OpenAIResponsesBackendConfig),
}

impl BackendConfig {
    pub fn kind(&self) -> BackendKind {
        match self {
            BackendConfig::Anthropic(_) => BackendKind::AnthropicMessages,
            BackendConfig::OpenAIResponses(_) => BackendKind::OpenAIResponses,
        }
    }

    /// Environment-based defaults for the protocol that serves `model`.
    pub fn default_for_model(model: Model) -> Self {
        match model.backend_kind() {
            BackendKind::AnthropicMessages => {
                BackendConfig::Anthropic(AnthropicBackendConfig::default_from_env())
            }
            BackendKind::OpenAIResponses => {
                BackendConfig::OpenAIResponses(OpenAIResponsesBackendConfig::default_from_env())
            }
        }
    }
}

impl Model {
    /// Model identifier sent to the provider API.
    pub fn api_id(self) -> &'static str {
        match self {
            Model::ClaudeFable5 => "claude-fable-5",
            Model::ClaudeOpus48 => "claude-opus-4-8",
            Model::ClaudeSonnet5 => "claude-sonnet-4-6",
            Model::ClaudeHaiku45 => "claude-haiku-4-5",
            Model::Grok45Build => "grok-4.5-build",
        }
    }

    /// Protocol / backend that serves this model by default.
    pub fn backend_kind(self) -> BackendKind {
        match self {
            Model::ClaudeFable5
            | Model::ClaudeOpus48
            | Model::ClaudeSonnet5
            | Model::ClaudeHaiku45 => BackendKind::AnthropicMessages,
            Model::Grok45Build => BackendKind::OpenAIResponses,
        }
    }

    /// Whether Anthropic thinking uses `thinking.type: "adaptive"` (+ effort)
    /// rather than manual `thinking.type: "enabled"` with `budget_tokens`.
    ///
    /// Opus 4.8 / Fable 5 reject `enabled` with HTTP 400. Sonnet 4.6 still accepts
    /// budgets but they are deprecated. Haiku 4.5 requires the manual form.
    pub fn uses_adaptive_thinking(self) -> bool {
        match self {
            Model::ClaudeFable5 | Model::ClaudeOpus48 | Model::ClaudeSonnet5 => true,
            Model::ClaudeHaiku45 | Model::Grok45Build => false,
        }
    }

    /// Conservative context window (input+output) for UX and auto-compact heuristics.
    pub fn context_window_tokens(self) -> u64 {
        match self {
            // Wire ids today are standard windows; [1m] aliases can raise later.
            Model::ClaudeFable5
            | Model::ClaudeOpus48
            | Model::ClaudeSonnet5
            | Model::ClaudeHaiku45
            | Model::Grok45Build => 200_000,
        }
    }
}

impl std::fmt::Display for Model {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.api_id())
    }
}

impl std::str::FromStr for Model {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "claude-fable-5" => Ok(Model::ClaudeFable5),
            "claude-opus-4-8" | "claude-opus-4.8" | "claude-opus-4.8[1m]" => {
                Ok(Model::ClaudeOpus48)
            }
            "claude-sonnet-4-6" | "claude-sonnet-5" | "claude-sonnet-4.5" => {
                Ok(Model::ClaudeSonnet5)
            }
            "claude-haiku-4-5" | "claude-haiku-4.5" => Ok(Model::ClaudeHaiku45),
            "grok-4.5-build" | "grok-4.5" | "grok-4.5-build[1m]" => Ok(Model::Grok45Build),
            other => Err(format!("Unknown model: {other:?}")),
        }
    }
}

pub struct GenerativeModelConfig {
    pub model: Model,
    pub tools: Vec<ToolSpec>,
    pub system_prompt: String,
    /// When `None`, a default is selected from [`Model::backend_kind`].
    pub backend_config: Option<BackendConfig>,
}

pub fn new(config: GenerativeModelConfig) -> Result<Arc<dyn GenerativeModel>, ModelCreationError> {
    let backend = config
        .backend_config
        .clone()
        .unwrap_or_else(|| BackendConfig::default_for_model(config.model));

    match backend {
        BackendConfig::Anthropic(backend) => {
            let model = anthropic::AnthropicGenerativeModel::new(config, backend)?;
            Ok(model as Arc<dyn GenerativeModel>)
        }
        BackendConfig::OpenAIResponses(backend) => {
            let model = openai_responses::OpenAIResponsesGenerativeModel::new(config, backend)?;
            Ok(model as Arc<dyn GenerativeModel>)
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum Message {
    UserMessage {
        content: Vec<Content>,
    },
    ToolResults {
        tool_use_results: Vec<ToolResult>,
    },
    AssistantMessage {
        content: Vec<Content>,
        tool_uses: Vec<ToolUse>,
        turn_end_reason: Option<TurnEndReason>,
    },
}
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum TurnEndReason {
    EndTurn,
    MaxTokens,
    ToolUse,
    /// Provider-specific / unknown stop reason (owned so sessions can serialize cleanly).
    Other(String),
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ToolUse {
    pub id: String,
    pub name: String,
    pub input: serde_json::Value,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
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

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
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

impl Content {
    /// Final-answer content (excludes thinking).
    pub fn is_answer(&self) -> bool {
        matches!(self, Content::Text { .. } | Content::Image { .. })
    }
}

/// Clone only answer blocks (`Text` / `Image`), dropping thinking.
pub fn answer_content(content: &[Content]) -> Vec<Content> {
    content.iter().filter(|c| c.is_answer()).cloned().collect()
}

#[derive(Debug, Clone)]
pub enum MessagePart {
    MessageStart,
    ContentStart(ContentStart),
    ContentDelta(ContentDelta),
    ToolUseStart(ToolUseStart),
    ToolUseDelta(ToolUseDelta),
    TurnEndReason(TurnEndReason),
    /// Provider token usage for this generate call (may appear mid-stream or at end).
    Usage(TokenUsage),
}

/// Token counts reported by a provider for one generate call.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TokenUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_creation_tokens: Option<u64>,
}

impl TokenUsage {
    /// Best estimate of context occupied by the prompt (input + cache reads when present).
    pub fn context_tokens(self) -> u64 {
        self.input_tokens
            .saturating_add(self.cache_read_tokens.unwrap_or(0))
            .saturating_add(self.cache_creation_tokens.unwrap_or(0))
    }
}

#[derive(Debug, Clone)]
pub enum ContentStart {
    Text {
        index: usize,
    },
    Image {
        index: usize,
    },
    Thinking {
        index: usize,
        signature: Option<String>,
        redacted: bool,
    },
}

#[derive(Debug, Clone)]
pub enum ContentDelta {
    Text { index: usize, delta: String },
    Image { index: usize, delta: String },
    Thinking { index: usize, delta: String },
}

#[derive(Debug, Clone)]
pub struct ToolUseStart {
    pub index: usize,
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone)]
pub struct ToolUseDelta {
    pub index: usize,
    pub input_json_delta: String,
}

//
// GenerateOutput: accumulate a stream of MessageParts into a finished assistant turn
//

#[derive(Debug, Clone)]
pub struct GenerateOutput {
    pub content: Vec<Content>,
    pub tool_uses: Vec<ToolUse>,
    pub turn_end_reason: TurnEndReason,
    /// Last usage observed on the stream, if the provider reported any.
    pub usage: Option<TokenUsage>,
}

impl GenerateOutput {
    pub async fn from_stream(
        stream: impl Stream<Item = Result<MessagePart, GenerateError>>,
    ) -> Result<Self, GenerateError> {
        Self::from_stream_with_hook(stream, |_| {}).await
    }

    /// Accumulate a generation stream, invoking `on_part` for each successfully parsed part
    /// (including the initial `MessageStart`).
    pub async fn from_stream_with_hook(
        stream: impl Stream<Item = Result<MessagePart, GenerateError>>,
        mut on_part: impl FnMut(&MessagePart),
    ) -> Result<Self, GenerateError> {
        struct IncompleteToolUse {
            id: String,
            name: String,
            input_json: String,
        }

        impl TryInto<ToolUse> for IncompleteToolUse {
            type Error = GenerateError;

            fn try_into(self) -> Result<ToolUse, Self::Error> {
                let input = if self.input_json.is_empty() {
                    serde_json::json!({})
                } else {
                    serde_json::from_str(&self.input_json).map_err(|e| {
                        GenerateError::MalformedResponseError(format!(
                            "Malformed stream: tool use input JSON is invalid: {e}"
                        ))
                    })?
                };
                Ok(ToolUse {
                    id: self.id,
                    name: self.name,
                    input,
                })
            }
        }

        let mut content: Vec<Option<Content>> = Vec::new();
        let mut tool_uses: Vec<Option<IncompleteToolUse>> = Vec::new();
        let mut turn_end_reason = None;
        let mut usage = None;

        let mut stream = pin!(stream);

        let Some(try_item) = stream.next().await else {
            return Err(GenerateError::MalformedResponseError(
                concat!(
                    "Malformed stream: empty stream. ",
                    "Did you accidentally drain the stream already?"
                )
                .into(),
            ));
        };
        let first = try_item?;
        let MessagePart::MessageStart = &first else {
            return Err(GenerateError::MalformedResponseError(
                concat!(
                    "Malformed stream: first item is not MessageStart. ",
                    "Did you accidentally drain the stream already?"
                )
                .into(),
            ));
        };
        on_part(&first);

        while let Some(item) = stream.next().await {
            let item = item?;
            on_part(&item);
            match item {
                MessagePart::MessageStart => {
                    return Err(GenerateError::MalformedResponseError(
                        "Malformed stream: unexpected MessageStart".into(),
                    ));
                }
                MessagePart::ContentStart(ContentStart::Text { index }) => {
                    ensure_slot(
                        &mut content,
                        index,
                        Content::Text {
                            text: String::new(),
                        },
                    );
                }
                MessagePart::ContentStart(ContentStart::Image { index }) => {
                    ensure_slot(
                        &mut content,
                        index,
                        Content::Image {
                            source: String::new(),
                        },
                    );
                }
                MessagePart::ContentStart(ContentStart::Thinking {
                    index,
                    signature,
                    redacted,
                }) => {
                    ensure_slot(
                        &mut content,
                        index,
                        Content::Thinking {
                            text: String::new(),
                            signature,
                            redacted,
                        },
                    );
                }
                MessagePart::ContentDelta(ContentDelta::Text { index, delta }) => {
                    let Some(Some(Content::Text { text })) = content.get_mut(index) else {
                        return Err(GenerateError::MalformedResponseError(format!(
                            "Malformed stream: text delta index {index} is out of bounds \
                             or points to non-text content"
                        )));
                    };
                    text.push_str(&delta);
                }
                MessagePart::ContentDelta(ContentDelta::Image { index, delta }) => {
                    let Some(Some(Content::Image { source })) = content.get_mut(index) else {
                        return Err(GenerateError::MalformedResponseError(format!(
                            "Malformed stream: image delta index {index} is out of bounds \
                             or points to non-image content"
                        )));
                    };
                    source.push_str(&delta);
                }
                MessagePart::ContentDelta(ContentDelta::Thinking { index, delta }) => {
                    let Some(Some(Content::Thinking { text, redacted, .. })) =
                        content.get_mut(index)
                    else {
                        return Err(GenerateError::MalformedResponseError(format!(
                            "Malformed stream: thinking delta index {index} is out of bounds \
                             or points to non-thinking content"
                        )));
                    };
                    if !*redacted {
                        text.push_str(&delta);
                    }
                }
                MessagePart::ToolUseStart(ToolUseStart { index, id, name }) => {
                    ensure_slot(
                        &mut tool_uses,
                        index,
                        IncompleteToolUse {
                            id,
                            name,
                            input_json: String::new(),
                        },
                    );
                }
                MessagePart::ToolUseDelta(ToolUseDelta {
                    index,
                    input_json_delta,
                }) => {
                    let Some(Some(tool_use)) = tool_uses.get_mut(index) else {
                        return Err(GenerateError::MalformedResponseError(format!(
                            "Malformed stream: tool use delta index {index} is out of bounds"
                        )));
                    };
                    tool_use.input_json.push_str(&input_json_delta);
                }
                MessagePart::TurnEndReason(reason) => {
                    turn_end_reason = Some(reason);
                }
                MessagePart::Usage(u) => {
                    usage = Some(u);
                }
            }
        }

        let content = content
            .into_iter()
            .enumerate()
            .map(|(i, slot)| {
                slot.ok_or_else(|| {
                    GenerateError::MalformedResponseError(format!(
                        "Malformed stream: missing content block at index {i}"
                    ))
                })
            })
            .collect::<Result<Vec<_>, _>>()?;

        let tool_uses = tool_uses
            .into_iter()
            .enumerate()
            .map(|(i, slot)| {
                let incomplete = slot.ok_or_else(|| {
                    GenerateError::MalformedResponseError(format!(
                        "Malformed stream: missing tool use at index {i}"
                    ))
                })?;
                incomplete.try_into()
            })
            .collect::<Result<Vec<ToolUse>, GenerateError>>()?;

        let turn_end_reason = turn_end_reason.ok_or_else(|| {
            GenerateError::MalformedResponseError(
                "Malformed stream: no turn end reason provided".into(),
            )
        })?;

        Ok(GenerateOutput {
            content,
            tool_uses,
            turn_end_reason,
            usage,
        })
    }
}

fn ensure_slot<T>(slots: &mut Vec<Option<T>>, index: usize, value: T) {
    while slots.len() <= index {
        slots.push(None);
    }
    slots[index] = Some(value);
}

#[derive(thiserror::Error, Debug)]
pub enum ModelCreationError {
    #[error("Invalid configuration parameters supplied: {0}")]
    BadConfig(String),

    #[error("Uncategorized error occurred: {0}")]
    Uncategorized(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn accumulate_thinking_then_text() {
        use futures::stream;

        let parts = vec![
            Ok(MessagePart::MessageStart),
            Ok(MessagePart::ContentStart(ContentStart::Thinking {
                index: 0,
                signature: Some("sig".into()),
                redacted: false,
            })),
            Ok(MessagePart::ContentDelta(ContentDelta::Thinking {
                index: 0,
                delta: "reason".into(),
            })),
            Ok(MessagePart::ContentStart(ContentStart::Text { index: 1 })),
            Ok(MessagePart::ContentDelta(ContentDelta::Text {
                index: 1,
                delta: "answer".into(),
            })),
            Ok(MessagePart::TurnEndReason(TurnEndReason::EndTurn)),
        ];
        let output = GenerateOutput::from_stream(stream::iter(parts))
            .await
            .expect("accumulate");
        assert_eq!(output.content.len(), 2);
        match &output.content[0] {
            Content::Thinking {
                text,
                signature,
                redacted,
            } => {
                assert_eq!(text, "reason");
                assert_eq!(signature.as_deref(), Some("sig"));
                assert!(!*redacted);
            }
            other => panic!("expected thinking, got {other:?}"),
        }
        match &output.content[1] {
            Content::Text { text } => assert_eq!(text, "answer"),
            other => panic!("expected text, got {other:?}"),
        }
        assert_eq!(answer_content(&output.content).len(), 1);
    }

    #[test]
    fn default_backend_matches_model_kind() {
        assert_eq!(
            BackendConfig::default_for_model(Model::ClaudeHaiku45).kind(),
            BackendKind::AnthropicMessages
        );
        assert_eq!(
            BackendConfig::default_for_model(Model::Grok45Build).kind(),
            BackendKind::OpenAIResponses
        );
    }

    #[test]
    fn adaptive_thinking_model_matrix() {
        assert!(Model::ClaudeFable5.uses_adaptive_thinking());
        assert!(Model::ClaudeOpus48.uses_adaptive_thinking());
        assert!(Model::ClaudeSonnet5.uses_adaptive_thinking());
        assert!(!Model::ClaudeHaiku45.uses_adaptive_thinking());
        assert!(!Model::Grok45Build.uses_adaptive_thinking());
    }

    #[test]
    fn anthropic_backend_rejects_grok_model() {
        let result = anthropic::AnthropicGenerativeModel::new(
            GenerativeModelConfig {
                model: Model::Grok45Build,
                tools: vec![],
                system_prompt: String::new(),
                backend_config: None,
            },
            AnthropicBackendConfig {
                anthropic_auth_token: "dummy".into(),
                ..Default::default()
            },
        );
        let err = match result {
            Ok(_) => panic!("expected model/backend mismatch"),
            Err(e) => e,
        };
        assert!(
            err.to_string().contains("not supported"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn openai_responses_backend_rejects_claude_model() {
        let result = openai_responses::OpenAIResponsesGenerativeModel::new(
            GenerativeModelConfig {
                model: Model::ClaudeHaiku45,
                tools: vec![],
                system_prompt: String::new(),
                backend_config: None,
            },
            OpenAIResponsesBackendConfig {
                auth_token: "dummy".into(),
                ..Default::default()
            },
        );
        let err = match result {
            Ok(_) => panic!("expected model/backend mismatch"),
            Err(e) => e,
        };
        assert!(
            err.to_string().contains("not supported"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn new_rejects_mismatched_explicit_backend() {
        let result = new(GenerativeModelConfig {
            model: Model::Grok45Build,
            tools: vec![],
            system_prompt: String::new(),
            backend_config: Some(BackendConfig::Anthropic(AnthropicBackendConfig {
                anthropic_auth_token: "dummy".into(),
                ..Default::default()
            })),
        });
        let err = match result {
            Ok(_) => panic!("expected mismatch"),
            Err(e) => e,
        };
        assert!(err.to_string().contains("not supported"), "{err}");
    }

    #[test]
    fn message_types_serde_roundtrip() {
        let messages = vec![
            Message::UserMessage {
                content: vec![
                    Content::Text { text: "hi".into() },
                    Content::Image {
                        source: "data".into(),
                    },
                ],
            },
            Message::AssistantMessage {
                content: vec![Content::Text { text: "ok".into() }],
                tool_uses: vec![ToolUse {
                    id: "t1".into(),
                    name: "bash".into(),
                    input: serde_json::json!({"command": "true"}),
                }],
                turn_end_reason: Some(TurnEndReason::ToolUse),
            },
            Message::ToolResults {
                tool_use_results: vec![ToolResult {
                    id: "t1".into(),
                    content: vec![Content::Text {
                        text: "done".into(),
                    }],
                    is_error: false,
                }],
            },
            Message::AssistantMessage {
                content: vec![],
                tool_uses: vec![],
                turn_end_reason: Some(TurnEndReason::Other("Anthropic::PauseTurn".into())),
            },
        ];
        let json = serde_json::to_string(&messages).expect("serialize");
        let back: Vec<Message> = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(
            serde_json::to_value(&back).unwrap(),
            serde_json::to_value(&messages).unwrap()
        );
    }
}

#[derive(thiserror::Error, Debug)]
pub enum GenerateError {
    #[error("Something went wrong while generating a response: {0}")]
    ExecutionError(String),

    #[error("Generation succeeded, but the model refused to comply: {0}")]
    RefusalError(String),

    #[error("Generation succeeded, but the output was malformed or corrupted: {0}")]
    MalformedResponseError(String),
}
