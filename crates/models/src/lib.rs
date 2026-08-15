//! The model-side adapter, ported from v2 (`main-v2:src/models/`): one
//! trait ([`GenerativeModel`]), a streaming part vocabulary, and the
//! provider drivers that project [`Message`] histories onto real wire
//! protocols (Anthropic Messages, OpenAI Responses, OpenAI Chat
//! Completions). Nothing here knows about instances or the bus — the chat
//! kind (next PR) converts its transcript into [`Message`]s and consumes
//! the stream; this crate ends at the provider's socket.
//!
//! The catalog ([`resolve_catalog`]) turns `[gateways]` / `[models]`
//! tables — now sections of `server.toml` — into usable [`CatalogModel`]s;
//! myco ships no built-in models. Credentials resolve lazily: a configured
//! model without its env var is fine until the moment it is actually used.

use std::{pin::pin, sync::Arc};

use futures::{Stream, StreamExt};

/// A boxed sendable stream — the shape every provider returns.
pub type AsyncStream<T> = std::pin::Pin<Box<dyn Stream<Item = T> + Send>>;

mod anthropic;

/// The scripted test double. Compiled unconditionally (not `cfg(test)`)
/// because downstream crates' tests script turns with it; it is a test
/// double all the same, never a production path.
pub mod test_support;

mod types;
pub use types::{
    Content, TokenUsage, ToolResult, ToolUse, TurnEndReason, answer_content, content_text,
};

mod catalog;
pub use catalog::{
    AuthEntry, CatalogFile, GatewayEntry, ModelEntry, RetryEntry, read_auth_file,
    resolve_catalog,
};

/// Wire ids for every tool call in `input`: `out[i][j]` is the id a driver
/// sends for the `j`-th tool_use of the assistant message at index `i`, and
/// equally for the `j`-th result of the `ToolResults` message answering it.
///
/// A call and its result pair positionally: `tool_use_results[j]` answers
/// `tool_uses[j]` of the immediately preceding assistant message, the order
/// the agent loop writes and compaction preserves (tails start at a user
/// message and never split a pair). Providers, however, require an id on the
/// wire to make that same pairing inside one request, and each has its own
/// dialect ([`mint_tool_id`]) — so drivers mint one per position here. The
/// ids *stored* in history are display and dispatch identity (tool cards,
/// result correlation) and never reach the wire: echoing a provider-minted
/// id is what used to wedge a session the moment it resumed on a different
/// provider whose id grammar rejects it.
///
/// Ids derive from (message index, ordinal), so history growth never changes
/// the ids of earlier messages and the request prefix stays byte-identical
/// across turns for provider prompt caching. A history whose result count
/// disagrees with the preceding assistant's tool calls fails here, before any
/// request is sent: guessing the pairing would corrupt the conversation
/// silently.
pub(crate) fn wire_tool_ids(input: &[Message]) -> Result<Vec<Vec<String>>, GenerateError> {
    let mut out: Vec<Vec<String>> = Vec::with_capacity(input.len());
    for (i, message) in input.iter().enumerate() {
        let ids = match message {
            Message::UserMessage { .. } => Vec::new(),
            Message::AssistantMessage { tool_uses, .. } => {
                (0..tool_uses.len()).map(|j| mint_tool_id(i, j)).collect()
            }
            Message::ToolResults { tool_use_results } => {
                let preceding_uses = match i.checked_sub(1).map(|p| &input[p]) {
                    Some(Message::AssistantMessage { tool_uses, .. }) => tool_uses.len(),
                    _ => 0,
                };
                if preceding_uses != tool_use_results.len() {
                    return Err(GenerateError::ExecutionError(format!(
                        "history is malformed: message {i} carries {} tool results but the \
                         message before it has {preceding_uses} tool calls",
                        tool_use_results.len()
                    )));
                }
                out[i - 1].clone()
            }
        };
        out.push(ids);
    }
    Ok(out)
}

/// One wire tool id: `'t'` + message index + ordinal, four base-36 digits
/// each. Nine alphanumeric characters is the intersection of every dialect's
/// constraints — Anthropic's `^[a-zA-Z0-9_-]+$`, OpenAI's length cap, and
/// Mistral-style OpenAI-compatible backends requiring exactly nine
/// alphanumerics (the binding constraint).
fn mint_tool_id(message_index: usize, ordinal: usize) -> String {
    const CAP: usize = 36 * 36 * 36 * 36;
    // A history long enough to overflow four digits (1.6M messages) exceeds
    // the request size cap long before it gets here.
    assert!(message_index < CAP && ordinal < CAP);
    let mut id = String::with_capacity(9);
    id.push('t');
    for n in [message_index, ordinal] {
        for place in [36 * 36 * 36, 36 * 36, 36, 1] {
            id.push(char::from_digit(((n / place) % 36) as u32, 36).unwrap());
        }
    }
    id
}

pub use anthropic::AnthropicBackendConfig;

mod driver_core;

mod openai_common;
pub use openai_common::OpenAIBackendConfig;

mod openai_completions;
mod openai_responses;

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
)]
pub enum Protocol {
    #[serde(rename = "anthropic-messages")]
    AnthropicMessages,
    #[serde(rename = "openai-responses")]
    OpenAIResponses,
    #[serde(rename = "openai-completions")]
    OpenAICompletions,
}

impl std::fmt::Display for Protocol {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Protocol::AnthropicMessages => f.write_str("anthropic-messages"),
            Protocol::OpenAIResponses => f.write_str("openai-responses"),
            Protocol::OpenAICompletions => f.write_str("openai-completions"),
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
            Protocol::OpenAIResponses | Protocol::OpenAICompletions => ThinkingMode::Effort,
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
            Protocol::OpenAIResponses | Protocol::OpenAICompletions => {
                matches!(self, ThinkingMode::Effort | ThinkingMode::None)
            }
        }
    }
}

/// When and how a driver re-sends a request the provider failed transiently.
/// Resolved per gateway (a model's own `[models.KEY.retry]` table replaces it
/// wholesale); consumed by `driver_core::send_with_retry` before anything has
/// streamed to the consumer.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct RetryPolicy {
    /// Total attempts including the first. `1` disables retry.
    pub max_attempts: u32,
    pub initial_backoff: std::time::Duration,
    /// Ceiling on one wait, applied to a provider's `Retry-After` too, so a
    /// hostile or mistaken header cannot park an unattended run for hours.
    pub max_backoff: std::time::Duration,
    pub backoff_multiplier: f64,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 3,
            initial_backoff: std::time::Duration::from_millis(500),
            max_backoff: std::time::Duration::from_secs(30),
            backoff_multiplier: 2.0,
        }
    }
}

impl RetryPolicy {
    /// Wait before `attempt` (1-based). Attempt 1 is the original send and
    /// never waits; `retry_after` is the provider's ask, honoured when it
    /// exceeds the computed backoff and still capped by [`Self::max_backoff`].
    ///
    /// No jitter: myco is one client per user, so there is no fleet to
    /// de-synchronise, and a deterministic schedule is one less thing to
    /// reason about when reading an overnight log.
    pub fn backoff(
        &self,
        attempt: u32,
        retry_after: Option<std::time::Duration>,
    ) -> std::time::Duration {
        if attempt <= 1 {
            return std::time::Duration::ZERO;
        }
        let steps = attempt.saturating_sub(2);
        let factor = self.backoff_multiplier.max(1.0).powi(steps.min(32) as i32);
        let millis = (self.initial_backoff.as_millis() as f64 * factor).min(u64::MAX as f64);
        let computed = std::time::Duration::from_millis(millis as u64);
        retry_after
            .unwrap_or(std::time::Duration::ZERO)
            .max(computed)
            .min(self.max_backoff)
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
    /// Largest image this model accepts, measured on the base64 payload.
    /// Enforced locally by `view_image` and by REPL `@path` attachments so an
    /// oversized image fails with a clear message instead of a provider 400.
    /// Always concrete: config resolution applies the `max_image_base64_bytes`
    /// entry or its default, and callers downstream take this value.
    pub max_image_base64_bytes: u64,
    /// Queue an auto-compaction when a turn ends with the prompt at or past
    /// this many tokens (resolved from the config fraction `auto_compact_at`).
    pub auto_compact_at_tokens: u64,
    /// Consecutive `max_tokens` truncations one turn resumes through
    /// (config `max_truncated_resumes`; `0` never resumes).
    pub max_truncated_resumes: u32,
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
    /// Set when the auth source did not resolve (env var unset, auth file
    /// unreadable). Reported by [`ModelCatalog::get`] when the model is
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
    /// failing auth source (env var / file).
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

    /// Spec for `key`, ignoring credential state. For settings that must be
    /// read while merely *configuring* a run (the image cap host workers are
    /// spawned with) rather than using the model — [`Self::get`] is still the
    /// gate for that.
    pub fn spec(&self, key: &str) -> Option<&ModelSpec> {
        self.entries.get(key).map(|entry| &entry.spec)
    }

    pub fn keys(&self) -> Vec<&str> {
        self.entries.keys().map(String::as_str).collect()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
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
    /// Responses API (`{base_url}/responses`).
    OpenAIResponses(OpenAIBackendConfig),
    /// Chat Completions API (`{base_url}/chat/completions`).
    OpenAICompletions(OpenAIBackendConfig),
}

impl BackendConfig {
    pub fn protocol(&self) -> Protocol {
        match self {
            BackendConfig::Anthropic(_) => Protocol::AnthropicMessages,
            BackendConfig::OpenAIResponses(_) => Protocol::OpenAIResponses,
            BackendConfig::OpenAICompletions(_) => Protocol::OpenAICompletions,
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
        BackendConfig::OpenAICompletions(backend) => {
            let model = openai_completions::OpenAICompletionsGenerativeModel::new(config, backend)?;
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

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
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
        let mut usage: Option<TokenUsage> = None;

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
                MessagePart::ContentStart(start) => {
                    let (index, block) = start_block(start);
                    ensure_slot(&mut content, index, block);
                }
                MessagePart::ContentDelta(delta) => apply_content_delta(&mut content, delta)?,
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
                    usage = Some(match usage {
                        Some(prev) => prev.merge(u),
                        None => u,
                    });
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

/// The empty [`Content`] block a [`ContentStart`] opens, with its index.
fn start_block(start: ContentStart) -> (usize, Content) {
    match start {
        ContentStart::Text { index } => (
            index,
            Content::Text {
                text: String::new(),
            },
        ),
        ContentStart::Image { index } => (
            index,
            Content::Image {
                source: String::new(),
            },
        ),
        ContentStart::Thinking {
            index,
            signature,
            redacted,
        } => (
            index,
            Content::Thinking {
                text: String::new(),
                signature,
                redacted,
            },
        ),
    }
}

/// Append a [`ContentDelta`] to its opened block; the slot must exist and be
/// the matching kind (redacted thinking swallows its deltas).
fn apply_content_delta(
    content: &mut [Option<Content>],
    delta: ContentDelta,
) -> Result<(), GenerateError> {
    let index = match &delta {
        ContentDelta::Text { index, .. }
        | ContentDelta::Image { index, .. }
        | ContentDelta::Thinking { index, .. } => *index,
    };
    match (content.get_mut(index).and_then(Option::as_mut), delta) {
        (Some(Content::Text { text }), ContentDelta::Text { delta, .. }) => text.push_str(&delta),
        (Some(Content::Image { source }), ContentDelta::Image { delta, .. }) => {
            source.push_str(&delta);
        }
        (Some(Content::Thinking { text, redacted, .. }), ContentDelta::Thinking { delta, .. }) => {
            if !*redacted {
                text.push_str(&delta);
            }
        }
        _ => {
            return Err(GenerateError::MalformedResponseError(format!(
                "Malformed stream: content delta at index {index}: out of bounds or wrong kind"
            )));
        }
    }
    Ok(())
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
    use crate::test_support::{assistant_tool, tool_results, user};

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
    fn token_usage_merge_prefers_known_fields() {
        let start = TokenUsage {
            input_tokens: 2195,
            output_tokens: 1,
            cached_input_tokens: 2000,
        };
        let delta = TokenUsage {
            input_tokens: 0,
            output_tokens: 89,
            cached_input_tokens: 0,
        };
        let merged = start.merge(delta);
        assert_eq!(merged.input_tokens, 2195);
        assert_eq!(merged.output_tokens, 89);
        assert_eq!(merged.cached_input_tokens, 2000);
        assert_eq!(merged.context_tokens(), 2195);
    }

    #[tokio::test]
    async fn accumulate_merges_split_usage() {
        use futures::stream;

        let parts = vec![
            Ok(MessagePart::MessageStart),
            Ok(MessagePart::Usage(TokenUsage {
                input_tokens: 2195,
                output_tokens: 1,
                cached_input_tokens: 2000,
            })),
            Ok(MessagePart::ContentStart(ContentStart::Text { index: 0 })),
            Ok(MessagePart::ContentDelta(ContentDelta::Text {
                index: 0,
                delta: "hi".into(),
            })),
            Ok(MessagePart::Usage(TokenUsage {
                input_tokens: 0,
                output_tokens: 89,
                cached_input_tokens: 0,
            })),
            Ok(MessagePart::TurnEndReason(TurnEndReason::EndTurn)),
        ];
        let output = GenerateOutput::from_stream(stream::iter(parts))
            .await
            .expect("accumulate");
        let usage = output.usage.expect("usage present");
        assert_eq!(usage.input_tokens, 2195);
        assert_eq!(usage.output_tokens, 89);
        assert_eq!(usage.cached_input_tokens, 2000);
        assert_eq!(usage.context_tokens(), 2195);
    }

    fn spec(key: &str, protocol: Protocol) -> ModelSpec {
        ModelSpec {
            key: key.into(),
            api_id: key.into(),
            protocol,
            thinking: ThinkingMode::default_for(protocol),
            context_window_tokens: 1_000_000,
            max_image_base64_bytes: crate::catalog::DEFAULT_MAX_IMAGE_BASE64_BYTES,
            auto_compact_at_tokens: 170_000,
            max_truncated_resumes: 3,
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
        // Both OpenAI dialects take the same effort-shaped thinking.
        assert_eq!(
            ThinkingMode::default_for(Protocol::OpenAICompletions),
            ThinkingMode::Effort
        );
        assert!(ThinkingMode::None.compatible_with(Protocol::OpenAICompletions));
        assert!(!ThinkingMode::Adaptive.compatible_with(Protocol::OpenAICompletions));
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
        assert_eq!(
            serde_json::from_value::<Protocol>(serde_json::json!("openai-completions")).unwrap(),
            Protocol::OpenAICompletions
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
            backend: BackendConfig::OpenAIResponses(OpenAIBackendConfig::default()),
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

    /// The preflight is the whole point of the rewind path: a request that is
    /// too big must be refused *before* upload, and must say so in a way the
    /// top level can act on.
    #[test]
    fn oversized_request_is_refused_before_upload() {
        assert!(check_request_size(MAX_REQUEST_BYTES, "Anthropic").is_ok());

        let err = check_request_size(MAX_REQUEST_BYTES + 1, "Anthropic").unwrap_err();
        assert!(
            matches!(err, GenerateError::RequestTooLargeError(_)),
            "{err:?}"
        );
        assert_eq!(err.recovery(), Recovery::OmitLastMessage);
        assert!(err.to_string().contains("the limit is 30 MiB"), "{err}");
    }

    /// A provider that rejects the size itself lands on the same variant, so
    /// the caller rewinds whether the cap was caught locally or remotely.
    #[test]
    fn provider_size_rejections_map_to_the_same_recovery() {
        let too_large = http_error(
            reqwest::StatusCode::PAYLOAD_TOO_LARGE,
            "HTTP 413: too big".into(),
        );
        assert_eq!(too_large.recovery(), Recovery::OmitLastMessage);

        // Anthropic reports it as a 400 whose body names the error type.
        let named = http_error(
            reqwest::StatusCode::BAD_REQUEST,
            r#"HTTP 400: {"error":{"type":"request_too_large"}}"#.into(),
        );
        assert_eq!(named.recovery(), Recovery::OmitLastMessage);

        // Others name no type and only describe the size in prose. Read as a
        // generic failure this is resent unchanged on every later turn.
        let described = http_error(
            reqwest::StatusCode::BAD_REQUEST,
            r#"HTTP 400: {"error":{"code":400,"message":"The message size (31271377 bytes) \
               exceeds 30.000MB limit.","status":"FAILED_PRECONDITION"}}"#
                .into(),
        );
        assert_eq!(described.recovery(), Recovery::OmitLastMessage);

        let unrelated = http_error(
            reqwest::StatusCode::INTERNAL_SERVER_ERROR,
            "HTTP 500: overloaded".into(),
        );
        assert_eq!(unrelated.recovery(), Recovery::Retry);
    }

    /// The `j`-th id of a ToolResults message equals the `j`-th id of the
    /// assistant message it answers — the pairing providers require on the
    /// wire, made from position alone.
    #[test]
    fn wire_ids_pair_results_with_the_preceding_calls() {
        let input = [
            user("hi"),
            assistant_tool(None, "prov_a", "bash", serde_json::json!({})),
            tool_results(&[("prov_a", "ok")]),
        ];
        let ids = wire_tool_ids(&input).unwrap();
        assert!(ids[0].is_empty());
        assert_eq!(ids[1].len(), 1);
        assert_eq!(ids[1], ids[2]);
        // Nine chars of [a-z0-9], every dialect's intersection.
        assert_eq!(ids[1][0].len(), 9);
        assert!(ids[1][0].chars().all(|c| c.is_ascii_alphanumeric()));
    }

    /// Appending messages must not change earlier ids: the request prefix has
    /// to stay byte-identical across turns or provider prompt caching breaks.
    #[test]
    fn wire_ids_stable_as_history_grows() {
        let mut input = vec![
            user("hi"),
            assistant_tool(None, "a", "bash", serde_json::json!({})),
            tool_results(&[("a", "ok")]),
        ];
        let before = wire_tool_ids(&input).unwrap();
        input.push(user("more"));
        input.push(assistant_tool(None, "b", "bash", serde_json::json!({})));
        input.push(tool_results(&[("b", "ok")]));
        let after = wire_tool_ids(&input).unwrap();
        assert_eq!(before[..], after[..3]);
        assert_ne!(after[1], after[4], "distinct calls must get distinct ids");
    }

    /// Positional pairing must refuse to guess: a results message whose count
    /// disagrees with the preceding assistant's tool calls fails before any
    /// request is sent, instead of silently mispairing.
    #[test]
    fn wire_ids_fail_loud_on_broken_pairing() {
        let orphaned = [user("hi"), tool_results(&[("a", "ok")])];
        assert!(wire_tool_ids(&orphaned).is_err());

        let miscounted = [
            assistant_tool(None, "a", "bash", serde_json::json!({})),
            tool_results(&[("a", "ok"), ("b", "ok")]),
        ];
        assert!(wire_tool_ids(&miscounted).is_err());
    }

    /// Backoff doubles from `initial_backoff`, stops at `max_backoff`, and a
    /// provider's `Retry-After` wins when it asks for longer — still under the
    /// cap, so a mistaken header cannot park an unattended run for hours.
    #[test]
    fn retry_backoff_grows_caps_and_honours_retry_after() {
        use std::time::Duration;
        let policy = RetryPolicy {
            max_attempts: 6,
            initial_backoff: Duration::from_millis(100),
            max_backoff: Duration::from_millis(1000),
            backoff_multiplier: 2.0,
        };

        // Attempt 1 is the original send; it never waits.
        assert_eq!(policy.backoff(1, None), Duration::ZERO);
        assert_eq!(policy.backoff(2, None), Duration::from_millis(100));
        assert_eq!(policy.backoff(3, None), Duration::from_millis(200));
        assert_eq!(policy.backoff(4, None), Duration::from_millis(400));
        // Growth stops at the cap rather than running away.
        assert_eq!(policy.backoff(9, None), Duration::from_millis(1000));

        // A longer Retry-After wins over the computed wait...
        assert_eq!(
            policy.backoff(2, Some(Duration::from_millis(500))),
            Duration::from_millis(500)
        );
        // ...but is still capped.
        assert_eq!(
            policy.backoff(2, Some(Duration::from_secs(3600))),
            Duration::from_millis(1000)
        );
        // A shorter one does not shrink the backoff.
        assert_eq!(
            policy.backoff(3, Some(Duration::from_millis(1))),
            Duration::from_millis(200)
        );
    }

    /// Reading the body is how a size rejection is recognized, so an ordinary
    /// failure must not be mistaken for one: rewinding drops a user message,
    /// which is destructive when the request was never too big.
    #[test]
    fn ordinary_failures_are_not_read_as_size_rejections() {
        for body in [
            r#"HTTP 400: {"error":{"message":"max_tokens exceeds the model's limit"}}"#,
            r#"HTTP 400: {"error":{"message":"messages: unexpected role"}}"#,
            r#"HTTP 401: {"error":{"message":"invalid x-api-key"}}"#,
        ] {
            let err = http_error(reqwest::StatusCode::BAD_REQUEST, body.into());
            assert_eq!(err.recovery(), Recovery::Retry, "{body}");
        }
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

/// What a caller can do about a failed turn.
///
/// Failures that are a property of the *history* cannot be fixed by trying
/// again — every later turn resends that history and fails the same way, which
/// wedges the session. This is the top-level signal for those: it says whether
/// the last user message has to come back out (see
/// [`crate::session::Agent::rewind_last_user_turn`]) before the conversation
/// can continue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Recovery {
    /// Nothing about the history is known to be at fault; resubmitting as-is
    /// may work (provider blip, refusal, malformed stream).
    Retry,
    /// The request is too big for the provider. Retrying unchanged fails
    /// identically — drop the last user message (typically the one carrying an
    /// oversized attachment) and the session can go on.
    OmitLastMessage,
}

/// Ceiling on one serialized API request body, checked before upload.
///
/// Providers cap the whole request (Anthropic: 32 MB), not just each image, and
/// attachments accumulate in history — so a session that was fine for several
/// turns can cross the cap and then fail on *every* subsequent turn. Checking
/// locally, with headroom under the provider's number, turns a confusing 413
/// after a multi-megabyte upload into an immediate [`Recovery::OmitLastMessage`].
pub const MAX_REQUEST_BYTES: usize = 30 * 1024 * 1024;

#[derive(thiserror::Error, Debug)]
pub enum GenerateError {
    #[error("Something went wrong while generating a response: {0}")]
    ExecutionError(String),

    #[error("Generation succeeded, but the model refused to comply: {0}")]
    RefusalError(String),

    #[error("Generation succeeded, but the output was malformed or corrupted: {0}")]
    MalformedResponseError(String),

    /// Request exceeds the provider's size limit — locally detected, or the
    /// provider's own 413. Not retryable without shrinking the history.
    #[error("The request is too large to send: {0}")]
    RequestTooLargeError(String),
}

impl GenerateError {
    pub fn recovery(&self) -> Recovery {
        match self {
            GenerateError::RequestTooLargeError(_) => Recovery::OmitLastMessage,
            GenerateError::ExecutionError(_)
            | GenerateError::RefusalError(_)
            | GenerateError::MalformedResponseError(_) => Recovery::Retry,
        }
    }
}

/// Refuse a composed request body over [`MAX_REQUEST_BYTES`].
///
/// Takes the already-serialized length so the checked size is exactly the size
/// uploaded — no second serialization pass over a multi-megabyte body.
pub(crate) fn check_request_size(len: usize, provider: &str) -> Result<(), GenerateError> {
    if len > MAX_REQUEST_BYTES {
        return Err(GenerateError::RequestTooLargeError(format!(
            "the {provider} request is {:.1} MiB; the limit is {} MiB. \
             Attached images stay in the conversation and add up — drop the last \
             message (or start a new session) to get back under it",
            len as f64 / (1024.0 * 1024.0),
            MAX_REQUEST_BYTES / (1024 * 1024),
        )));
    }
    Ok(())
}

/// Map a provider HTTP status to the right error variant: a size rejection is
/// not a generic failure.
///
/// 413 is unambiguous. Everything else has to be read out of the body, because
/// providers disagree on how they report an oversized request: some name a
/// machine-readable type (`request_too_large`), others return a 400 whose body
/// only describes the size in prose. Getting this wrong is expensive — the
/// identical body is resent on every later turn, so a size failure classified
/// as [`Recovery::Retry`] never triggers the rewind and the session fails
/// forever.
pub(crate) fn http_error(status: reqwest::StatusCode, message: String) -> GenerateError {
    if status == reqwest::StatusCode::PAYLOAD_TOO_LARGE || describes_a_size_rejection(&message) {
        GenerateError::RequestTooLargeError(message)
    } else {
        GenerateError::ExecutionError(message)
    }
}

/// Does this provider error body say the request was too big?
///
/// Matching prose is unavoidable, so it is kept to phrasings only a size
/// rejection produces: "too large" however the provider spells it, or a size
/// that "exceeds" a stated limit.
fn describes_a_size_rejection(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    message.contains("too_large")
        || message.contains("too large")
        || (message.contains("size") && message.contains("exceeds"))
}
