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

/// Wire protocol a model is served over.
///
/// Serde strings are the config.toml `protocol` values.
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
pub enum Protocol {
    #[serde(rename = "anthropic-messages")]
    AnthropicMessages,
    #[serde(rename = "openai-responses")]
    OpenAIResponses,
}

impl std::fmt::Display for Protocol {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Protocol::AnthropicMessages => f.write_str("anthropic-messages"),
            Protocol::OpenAIResponses => f.write_str("openai-responses"),
        }
    }
}

/// How thinking/reasoning is requested for a model.
///
/// Serde strings are the config.toml `thinking` values. Compatibility is
/// per-protocol (validated at catalog resolution): Anthropic Messages takes
/// `adaptive` | `budget` | `none`; OpenAI Responses takes `effort` | `none`.
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
#[serde(rename_all = "lowercase")]
pub enum ThinkingMode {
    /// Anthropic `thinking.type: "adaptive"` + `output_config.effort`
    /// (frontier models; older models reject it).
    Adaptive,
    /// Anthropic `thinking.type: "enabled"` + a `budget_tokens` mapped from
    /// [`Effort`] (e.g. Haiku 4.5).
    Budget,
    /// OpenAI-style `reasoning.effort`.
    Effort,
    /// Do not request thinking.
    None,
}

impl std::fmt::Display for ThinkingMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            ThinkingMode::Adaptive => "adaptive",
            ThinkingMode::Budget => "budget",
            ThinkingMode::Effort => "effort",
            ThinkingMode::None => "none",
        })
    }
}

impl ThinkingMode {
    /// Default mode when a catalog entry does not set `thinking`.
    pub fn default_for(protocol: Protocol) -> Self {
        match protocol {
            Protocol::AnthropicMessages => ThinkingMode::Adaptive,
            Protocol::OpenAIResponses => ThinkingMode::Effort,
        }
    }

    /// Whether this mode is servable over `protocol`.
    pub fn compatible_with(self, protocol: Protocol) -> bool {
        match protocol {
            Protocol::AnthropicMessages => {
                matches!(
                    self,
                    ThinkingMode::Adaptive | ThinkingMode::Budget | ThinkingMode::None
                )
            }
            Protocol::OpenAIResponses => {
                matches!(self, ThinkingMode::Effort | ThinkingMode::None)
            }
        }
    }
}

/// A resolved model: everything the protocol drivers need, minus credentials
/// (those live in [`BackendConfig`]). Built by `crate::config` from the
/// `[models]` / `[gateways]` catalog in config.toml — myco ships no built-in
/// models.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelSpec {
    /// Catalog key: what the user types after `--model` and what sessions
    /// record. Distinct from `api_id` so one wire model can appear under
    /// several keys (e.g. routed via different gateways).
    pub key: String,
    /// Wire id sent to the provider (the request `model` field).
    pub api_id: String,
    pub protocol: Protocol,
    pub thinking: ThinkingMode,
    /// Context window for UX (`USER n/m`) and auto-compact heuristics.
    pub context_window_tokens: u64,
}

impl std::fmt::Display for ModelSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.key)
    }
}

/// One usable catalog entry: spec plus the backend (gateway + credentials)
/// that serves it.
#[derive(Debug, Clone)]
pub struct CatalogModel {
    pub spec: ModelSpec,
    pub backend: BackendConfig,
    /// Set when the auth mechanism did not resolve (env var unset, tokens.toml
    /// key missing). Reported by [`ModelCatalog::get`] when the model is
    /// actually used — configuring a model without its credential is fine
    /// until then.
    pub auth_error: Option<String>,
}

/// Key → model catalog resolved from config.toml. Empty when the user has not
/// configured any models.
#[derive(Debug, Clone, Default)]
pub struct ModelCatalog {
    entries: std::collections::BTreeMap<String, CatalogModel>,
}

impl ModelCatalog {
    pub fn new(entries: std::collections::BTreeMap<String, CatalogModel>) -> Self {
        Self { entries }
    }

    /// Look up a usable model. Errors are user-actionable: unknown keys list
    /// the configured catalog; entries with unresolved credentials report the
    /// missing env var / tokens.toml key.
    pub fn get(&self, key: &str) -> Result<&CatalogModel, String> {
        let Some(entry) = self.entries.get(key) else {
            if self.entries.is_empty() {
                return Err(format!(
                    "unknown model {key:?}: no models configured — define [models] \
                     (and [gateways]) in config.toml"
                ));
            }
            return Err(format!(
                "unknown model {key:?}; configured models: [{}]",
                self.keys().join(", ")
            ));
        };
        if let Some(err) = &entry.auth_error {
            return Err(err.clone());
        }
        Ok(entry)
    }

    /// Key exists (regardless of whether its credential resolved).
    pub fn contains(&self, key: &str) -> bool {
        self.entries.contains_key(key)
    }

    pub fn keys(&self) -> Vec<&str> {
        self.entries.keys().map(String::as_str).collect()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
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

/// Provider backend settings: gateway base URL, credential, per-request knobs.
#[derive(Debug, Clone)]
pub enum BackendConfig {
    Anthropic(AnthropicBackendConfig),
    OpenAIResponses(OpenAIResponsesBackendConfig),
}

impl BackendConfig {
    pub fn protocol(&self) -> Protocol {
        match self {
            BackendConfig::Anthropic(_) => Protocol::AnthropicMessages,
            BackendConfig::OpenAIResponses(_) => Protocol::OpenAIResponses,
        }
    }
}

pub struct GenerativeModelConfig {
    pub model: ModelSpec,
    pub tools: Vec<ToolSpec>,
    pub system_prompt: String,
    pub backend_config: BackendConfig,
}

pub fn new(config: GenerativeModelConfig) -> Result<Arc<dyn GenerativeModel>, ModelCreationError> {
    if config.backend_config.protocol() != config.model.protocol {
        return Err(ModelCreationError::BadConfig(format!(
            "model `{}` speaks {} but the backend config is for {}",
            config.model,
            config.model.protocol,
            config.backend_config.protocol()
        )));
    }
    match config.backend_config.clone() {
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

    fn spec(key: &str, protocol: Protocol) -> ModelSpec {
        ModelSpec {
            key: key.into(),
            api_id: key.into(),
            protocol,
            thinking: ThinkingMode::default_for(protocol),
            context_window_tokens: 1_000_000,
        }
    }

    #[test]
    fn thinking_defaults_and_protocol_compatibility() {
        assert_eq!(
            ThinkingMode::default_for(Protocol::AnthropicMessages),
            ThinkingMode::Adaptive
        );
        assert_eq!(
            ThinkingMode::default_for(Protocol::OpenAIResponses),
            ThinkingMode::Effort
        );
        assert!(ThinkingMode::Budget.compatible_with(Protocol::AnthropicMessages));
        assert!(ThinkingMode::None.compatible_with(Protocol::AnthropicMessages));
        assert!(!ThinkingMode::Effort.compatible_with(Protocol::AnthropicMessages));
        assert!(ThinkingMode::None.compatible_with(Protocol::OpenAIResponses));
        assert!(!ThinkingMode::Adaptive.compatible_with(Protocol::OpenAIResponses));
        assert!(!ThinkingMode::Budget.compatible_with(Protocol::OpenAIResponses));
    }

    #[test]
    fn protocol_serde_uses_config_strings() {
        assert_eq!(
            serde_json::to_value(Protocol::AnthropicMessages).unwrap(),
            serde_json::json!("anthropic-messages")
        );
        assert_eq!(
            serde_json::from_value::<Protocol>(serde_json::json!("openai-responses")).unwrap(),
            Protocol::OpenAIResponses
        );
    }

    #[test]
    fn empty_catalog_get_says_no_models_configured() {
        let catalog = ModelCatalog::default();
        assert!(catalog.is_empty());
        let err = catalog.get("kimi-k3").unwrap_err();
        assert!(err.contains("no models configured"), "{err}");
        assert!(err.contains("[models]"), "{err}");
    }

    #[test]
    fn catalog_get_unknown_key_lists_configured_models() {
        let entry = CatalogModel {
            spec: spec("opus", Protocol::AnthropicMessages),
            backend: BackendConfig::Anthropic(AnthropicBackendConfig::default()),
            auth_error: None,
        };
        let catalog = ModelCatalog::new([("opus".to_string(), entry)].into());
        let err = catalog.get("opsu").unwrap_err();
        assert!(err.contains("unknown model \"opsu\""), "{err}");
        assert!(err.contains("[opus]"), "{err}");
        assert!(catalog.get("opus").is_ok());
    }

    #[test]
    fn catalog_get_reports_deferred_auth_error() {
        let entry = CatalogModel {
            spec: spec("kimi", Protocol::OpenAIResponses),
            backend: BackendConfig::OpenAIResponses(OpenAIResponsesBackendConfig::default()),
            auth_error: Some("model `kimi`: auth env:OPENROUTER_API_KEY is unset".into()),
        };
        let catalog = ModelCatalog::new([("kimi".to_string(), entry)].into());
        let err = catalog.get("kimi").unwrap_err();
        assert!(err.contains("OPENROUTER_API_KEY"), "{err}");
    }

    #[test]
    fn new_rejects_protocol_mismatch() {
        let result = new(GenerativeModelConfig {
            model: spec("grok", Protocol::OpenAIResponses),
            tools: vec![],
            system_prompt: String::new(),
            backend_config: BackendConfig::Anthropic(AnthropicBackendConfig {
                anthropic_auth_token: "dummy".into(),
                ..Default::default()
            }),
        });
        let err = match result {
            Ok(_) => panic!("expected mismatch"),
            Err(e) => e,
        };
        assert!(err.to_string().contains("speaks openai-responses"), "{err}");
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
