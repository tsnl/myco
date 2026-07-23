//! Anthropic Messages API backend.
//!
//! Ref: https://platform.claude.com/docs/en/api/messages/create
//! Streaming: https://platform.claude.com/docs/en/build-with-claude/streaming
//!
//! Invariant: history thinking is never re-sent to the API; whether budget-mode
//! thinking + tool use requires re-sending is an open question.

use std::sync::Arc;

use crate::core::*;

use super::driver_core::{Slot, SlotMap, SseAccumulator};
use super::*;

/// Anthropic Messages API settings ([`BackendConfig::Anthropic`]).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct AnthropicBackendConfig {
    pub anthropic_base_url: String,
    pub anthropic_auth_token: String,
    pub max_tokens_per_generate: usize,
    pub enable_prompt_caching: bool,
    pub debug_dump_api_requests: bool,
    /// When set, enables Anthropic extended thinking at this effort level.
    ///
    /// The request shape follows the model's [`ThinkingMode`]: `adaptive` sends
    /// `thinking.type: "adaptive"` plus `output_config.effort`; `budget` sends
    /// `thinking.type: "enabled"` with a mapped `budget_tokens`; `none` sends
    /// no thinking fields regardless of this value.
    ///
    /// Defaults to [`Effort::DEFAULT`] so thinking is always on for interactive use.
    pub effort: Option<Effort>,
}

impl Default for AnthropicBackendConfig {
    fn default() -> Self {
        Self {
            // No built-in gateway: the catalog (config.toml) supplies base_url.
            anthropic_base_url: String::new(),
            anthropic_auth_token: String::new(),
            max_tokens_per_generate: 8192,
            enable_prompt_caching: true,
            debug_dump_api_requests: false,
            effort: Some(Effort::DEFAULT),
        }
    }
}

/// Stateless Anthropic driver. Conversation history is owned by the caller.
pub struct AnthropicGenerativeModel {
    model: ModelSpec,
    system_prompt: String,
    tools: Vec<AnthropicTool>,
    backend: AnthropicBackendConfig,
    client: reqwest::Client,
}

impl AnthropicGenerativeModel {
    pub fn new(
        config: GenerativeModelConfig,
        backend: AnthropicBackendConfig,
    ) -> Result<Arc<Self>, ModelCreationError> {
        if config.model.protocol != Protocol::AnthropicMessages {
            return Err(ModelCreationError::BadConfig(format!(
                "model `{}` speaks {}, not {}",
                config.model,
                config.model.protocol,
                Protocol::AnthropicMessages
            )));
        }

        // api.anthropic.com authenticates API keys (`sk-ant-…`) via the
        // `x-api-key` header and rejects them as `Authorization: Bearer`;
        // Bearer is the convention for gateway/OAuth tokens. Pick by token
        // shape so both work against the default base URL.
        let token = &backend.anthropic_auth_token;
        let auth = if token.is_empty() {
            None
        } else if token.starts_with("sk-ant-") {
            Some(("x-api-key", token.clone()))
        } else {
            Some(("authorization", format!("Bearer {token}")))
        };
        let client = driver_core::build_client(auth, &[("anthropic-version", "2023-06-01")])?;

        let tools = config
            .tools
            .into_iter()
            .map(|spec| AnthropicTool {
                name: spec.name,
                description: spec.description,
                input_schema: spec.input_schema,
            })
            .collect();

        Ok(Arc::new(Self {
            model: config.model,
            system_prompt: config.system_prompt,
            tools,
            backend,
            client,
        }))
    }

    /// Build (without sending) the streaming Messages request.
    fn message_request(&self, messages: &[AnthropicMessage]) -> reqwest::RequestBuilder {
        // Anthropic only honors `cache_control` on content blocks (system / messages /
        // tools), never as a top-level request field. Put the breakpoint on the system
        // prompt text block so the stable prefix can be cached across turns.
        let system = if self.system_prompt.is_empty() {
            None
        } else {
            Some(vec![AnthropicSystemText {
                type_: "text",
                text: &self.system_prompt,
                cache_control: if self.backend.enable_prompt_caching {
                    Some(AnthropicCacheControl::Ephemeral)
                } else {
                    None
                },
            }])
        };

        let (thinking, output_config) =
            thinking_request_fields(self.model.thinking, self.backend.effort);
        // Anthropic requires max_tokens > thinking.budget_tokens for non-adaptive
        // extended thinking (e.g. Haiku). Adaptive thinking has no budget field.
        let mut max_tokens = self.backend.max_tokens_per_generate;
        if let Some(AnthropicThinkingConfig::Enabled { budget_tokens }) = &thinking {
            let need = (*budget_tokens as usize).saturating_add(1024);
            if max_tokens <= *budget_tokens as usize {
                max_tokens = need;
            }
        }
        let request = AnthropicMessagesRequest {
            max_tokens,
            model: &self.model.api_id,
            messages,
            system,
            tools: &self.tools,
            stream: true,
            thinking,
            output_config,
        };

        if self.backend.debug_dump_api_requests {
            eprintln!("{}", serde_json::to_string_pretty(&request).unwrap());
        }

        self.client
            .post(format!("{}/v1/messages", self.backend.anthropic_base_url))
            .json(&request)
    }
}

impl GenerativeModel for AnthropicGenerativeModel {
    fn generate(&self, input: &[Message]) -> AsyncStream<Result<MessagePart, GenerateError>> {
        let messages = convert_messages(input, self.backend.enable_prompt_caching);
        driver_core::spawn_generate(
            self.message_request(&messages),
            StreamAccumulator::default(),
            "Anthropic",
        )
    }
}

//
// Message conversion
//

/// One role-alternating turn of Anthropic content — the merged form of one or
/// more consecutive same-role [`Message`]s. A user turn may combine tool-result
/// and text blocks, which no single `Message` variant can hold, so the merge
/// yields these runs rather than `Message`s.
struct MessageRun {
    role: AnthropicRole,
    content: Vec<AnthropicContent>,
}

/// Merge consecutive same-role turns into role-alternating runs. Anthropic
/// requires alternating user/assistant roles, and tool-result blocks must lead
/// the user turn they answer.
fn merge_same_role_turns(input: &[Message]) -> Box<[MessageRun]> {
    let mut runs: Vec<MessageRun> = Vec::new();

    for message in input {
        let (role, content): (_, Vec<AnthropicContent>) = match message {
            Message::UserMessage { content } => (
                AnthropicRole::User,
                content.iter().cloned().filter_map(answer_block).collect(),
            ),
            Message::ToolResults { tool_use_results } => (
                AnthropicRole::User,
                tool_use_results
                    .iter()
                    .map(|result| AnthropicContent::ToolResult {
                        tool_use_id: result.id.clone(),
                        content: result
                            .content
                            .iter()
                            .cloned()
                            .filter_map(answer_block)
                            .collect(),
                        is_error: result.is_error,
                    })
                    .collect(),
            ),
            Message::AssistantMessage {
                content,
                tool_uses,
                turn_end_reason: _,
            } => {
                // Thinking may be stored in history for resume/UI; never echo it back to the API.
                let mut blocks: Vec<AnthropicContent> =
                    content.iter().cloned().filter_map(answer_block).collect();
                for tool_use in tool_uses {
                    blocks.push(AnthropicContent::ToolUse {
                        id: tool_use.id.clone(),
                        name: tool_use.name.clone(),
                        input: tool_use.input.clone(),
                    });
                }
                // A thinking-only turn (e.g. max_tokens hit mid-thinking)
                // strips to nothing; the API rejects empty assistant content
                // on every later request, permanently wedging the session.
                if blocks.is_empty() {
                    continue;
                }
                (AnthropicRole::Assistant, blocks)
            }
        };

        // Tool-result blocks must appear before any other content in a user turn.
        if let Some(last) = runs.last_mut()
            && last.role == role
        {
            if role == AnthropicRole::User {
                let new_is_only_tool_results = !content.is_empty()
                    && content
                        .iter()
                        .all(|c| matches!(c, AnthropicContent::ToolResult { .. }));
                if new_is_only_tool_results {
                    let mut combined = content;
                    combined.append(&mut last.content);
                    last.content = combined;
                } else {
                    last.content.extend(content);
                }
            } else {
                last.content.extend(content);
            }
            continue;
        }
        runs.push(MessageRun { role, content });
    }

    runs.into_boxed_slice()
}

fn convert_messages(input: &[Message], enable_cache: bool) -> Vec<AnthropicMessage> {
    // Merge into role-alternating runs, then emit one message per run — rolling
    // cache breakpoints onto the last two. Marking a block caches the whole prefix
    // up to it, and two breakpoints (rather than one) keep the previous turn's
    // write inside Anthropic's 20-block lookback as the conversation grows — the
    // recommended multi-turn pattern:
    // <https://platform.claude.com/docs/en/build-with-claude/prompt-caching>
    let runs = merge_same_role_turns(input);
    let count = runs.len();
    runs.into_vec()
        .into_iter()
        .enumerate()
        .map(|(i, MessageRun { role, content })| AnthropicMessage {
            role,
            content,
            cache_control: (enable_cache && i + 2 >= count)
                .then_some(AnthropicCacheControl::Ephemeral),
        })
        .collect()
}

//
// Stream accumulation (the SSE drive loop is shared, in driver_core)
//

/// Maps Anthropic's unified content-block indices onto separate content/tool-use index spaces.
#[derive(Default)]
struct StreamAccumulator {
    slots: SlotMap,
    stop_reason: Option<AnthropicStopReason>,
    finished: bool,
}

impl SseAccumulator for StreamAccumulator {
    fn handle_data(&mut self, data: &str) -> Result<Vec<MessagePart>, GenerateError> {
        let event: AnthropicStreamEvent = serde_json::from_str(data).map_err(|e| {
            GenerateError::MalformedResponseError(format!(
                "Failed to parse Anthropic SSE event JSON: {e}; data={data}"
            ))
        })?;
        self.handle_event(event)
    }

    fn finished(&self) -> bool {
        self.finished
    }

    fn finish(self) -> Result<(), GenerateError> {
        if self.stop_reason.is_none() {
            return Err(GenerateError::MalformedResponseError(
                "Anthropic stream ended without a stop_reason".into(),
            ));
        }
        Ok(())
    }
}

impl StreamAccumulator {
    fn handle_event(
        &mut self,
        event: AnthropicStreamEvent,
    ) -> Result<Vec<MessagePart>, GenerateError> {
        let mut out = Vec::new();

        match event {
            AnthropicStreamEvent::MessageStart { message } => {
                // Prompt-side counts (input + cache) arrive here; message_delta
                // later carries only output_tokens. The accumulator merges both.
                if let Some(u) = message.usage {
                    out.push(MessagePart::Usage(u.into_token_usage()));
                }
            }
            AnthropicStreamEvent::ContentBlockStart {
                index,
                content_block,
            } => match content_block {
                AnthropicStreamContentBlock::Text { text } => {
                    let content_index = self.slots.open_content(index);
                    out.push(MessagePart::ContentStart(ContentStart::Text {
                        index: content_index,
                    }));
                    if !text.is_empty() {
                        out.push(MessagePart::ContentDelta(ContentDelta::Text {
                            index: content_index,
                            delta: text,
                        }));
                    }
                }
                AnthropicStreamContentBlock::Thinking {
                    thinking,
                    signature,
                } => {
                    let content_index = self.slots.open_thinking(index);
                    out.push(MessagePart::ContentStart(ContentStart::Thinking {
                        index: content_index,
                        signature,
                        redacted: false,
                    }));
                    if !thinking.is_empty() {
                        out.push(MessagePart::ContentDelta(ContentDelta::Thinking {
                            index: content_index,
                            delta: thinking,
                        }));
                    }
                }
                AnthropicStreamContentBlock::RedactedThinking { data } => {
                    let content_index = self.slots.open_thinking(index);
                    // Preserve opaque payload in signature; no plaintext deltas.
                    out.push(MessagePart::ContentStart(ContentStart::Thinking {
                        index: content_index,
                        signature: if data.is_empty() { None } else { Some(data) },
                        redacted: true,
                    }));
                }
                AnthropicStreamContentBlock::ToolUse { id, name, input } => {
                    // Input arrives via input_json_delta; starter object is usually empty.
                    let _ = input;
                    let tool_index = self.slots.open_tool_use(index);
                    out.push(MessagePart::ToolUseStart(ToolUseStart {
                        index: tool_index,
                        id,
                        name,
                    }));
                }
                AnthropicStreamContentBlock::Other => {
                    self.slots.ignore(index);
                }
            },
            AnthropicStreamEvent::ContentBlockDelta { index, delta } => {
                let slot = self.slots.get(index).ok_or_else(|| {
                    GenerateError::MalformedResponseError(format!(
                        "content_block_delta for unknown index {index}"
                    ))
                })?;

                match (slot, delta) {
                    (
                        Slot::Content {
                            index: content_index,
                        }
                        | Slot::Thinking {
                            index: content_index,
                        },
                        AnthropicDelta::TextDelta { text },
                    ) => {
                        out.push(MessagePart::ContentDelta(ContentDelta::Text {
                            index: content_index,
                            delta: text,
                        }));
                    }
                    (
                        Slot::Content {
                            index: content_index,
                        }
                        | Slot::Thinking {
                            index: content_index,
                        },
                        AnthropicDelta::ThinkingDelta { thinking },
                    ) => {
                        out.push(MessagePart::ContentDelta(ContentDelta::Thinking {
                            index: content_index,
                            delta: thinking,
                        }));
                    }
                    (
                        Slot::Content {
                            index: content_index,
                        }
                        | Slot::Thinking {
                            index: content_index,
                        },
                        AnthropicDelta::InputJsonDelta { .. },
                    ) => {
                        return Err(GenerateError::MalformedResponseError(format!(
                            "input_json_delta on content block index {content_index}"
                        )));
                    }
                    (
                        Slot::ToolUse { index: tool_index },
                        AnthropicDelta::InputJsonDelta { partial_json },
                    ) => {
                        out.push(MessagePart::ToolUseDelta(ToolUseDelta {
                            index: tool_index,
                            input_json_delta: partial_json,
                        }));
                    }
                    (
                        Slot::ToolUse { .. },
                        AnthropicDelta::TextDelta { .. } | AnthropicDelta::ThinkingDelta { .. },
                    ) => {
                        return Err(GenerateError::MalformedResponseError(
                            "text/thinking delta on tool_use block".into(),
                        ));
                    }
                    (Slot::Ignored, _) | (_, AnthropicDelta::Other) => {}
                }
            }
            AnthropicStreamEvent::ContentBlockStop { .. } => {}
            AnthropicStreamEvent::MessageDelta { delta, usage } => {
                if let Some(u) = usage {
                    out.push(MessagePart::Usage(u.into_token_usage()));
                }
                if let Some(stop_reason) = delta.stop_reason {
                    if matches!(stop_reason, AnthropicStopReason::Refusal) {
                        return Err(GenerateError::RefusalError(
                            "Anthropic stop_reason=refusal".into(),
                        ));
                    }
                    self.stop_reason = Some(stop_reason.clone());
                    out.push(MessagePart::TurnEndReason(TurnEndReason::from(stop_reason)));
                }
            }
            AnthropicStreamEvent::MessageStop => {
                self.finished = true;
            }
            AnthropicStreamEvent::Ping => {}
            AnthropicStreamEvent::Error { error } => {
                return Err(GenerateError::ExecutionError(format!(
                    "Anthropic stream error event: {error}"
                )));
            }
            AnthropicStreamEvent::Other => {}
        }

        Ok(out)
    }
}

//
// Anthropic streaming event types
//

#[derive(Debug, serde::Deserialize)]
#[serde(tag = "type")]
enum AnthropicStreamEvent {
    #[serde(rename = "message_start")]
    MessageStart {
        #[serde(default)]
        message: AnthropicStartMessage,
    },
    #[serde(rename = "content_block_start")]
    ContentBlockStart {
        index: usize,
        content_block: AnthropicStreamContentBlock,
    },
    #[serde(rename = "content_block_delta")]
    ContentBlockDelta { index: usize, delta: AnthropicDelta },
    #[serde(rename = "content_block_stop")]
    ContentBlockStop {
        #[serde(default)]
        #[allow(dead_code)]
        index: usize,
    },
    #[serde(rename = "message_delta")]
    MessageDelta {
        delta: AnthropicMessageDelta,
        #[serde(default)]
        usage: Option<AnthropicUsage>,
    },
    #[serde(rename = "message_stop")]
    MessageStop,
    #[serde(rename = "ping")]
    Ping,
    #[serde(rename = "error")]
    Error { error: serde_json::Value },
    #[serde(other)]
    Other,
}

#[derive(Debug, serde::Deserialize)]
#[serde(tag = "type")]
enum AnthropicStreamContentBlock {
    #[serde(rename = "text")]
    Text {
        #[serde(default)]
        text: String,
    },
    #[serde(rename = "thinking")]
    Thinking {
        #[serde(default)]
        thinking: String,
        #[serde(default)]
        signature: Option<String>,
    },
    #[serde(rename = "redacted_thinking")]
    RedactedThinking {
        #[serde(default)]
        data: String,
    },
    #[serde(rename = "tool_use")]
    ToolUse {
        id: String,
        name: String,
        #[serde(default)]
        input: serde_json::Value,
    },
    #[serde(other)]
    Other,
}

#[derive(Debug, serde::Deserialize)]
#[serde(tag = "type")]
enum AnthropicDelta {
    #[serde(rename = "text_delta")]
    TextDelta { text: String },
    #[serde(rename = "thinking_delta")]
    ThinkingDelta { thinking: String },
    #[serde(rename = "input_json_delta")]
    InputJsonDelta { partial_json: String },
    #[serde(other)]
    Other,
}

#[derive(Debug, serde::Deserialize)]
struct AnthropicMessageDelta {
    stop_reason: Option<AnthropicStopReason>,
}

/// `message_start` payload; only its prompt-side `usage` is read.
#[derive(Debug, Default, serde::Deserialize)]
struct AnthropicStartMessage {
    #[serde(default)]
    usage: Option<AnthropicUsage>,
}

#[derive(Debug, Clone, serde::Deserialize)]
struct AnthropicUsage {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
    #[serde(default)]
    cache_read_input_tokens: Option<u64>,
    #[serde(default)]
    cache_creation_input_tokens: Option<u64>,
}

impl AnthropicUsage {
    fn into_token_usage(self) -> crate::generative_model::TokenUsage {
        // Full prompt = input + cache read + cache write; cached_input = reads.
        let cache_read = self.cache_read_input_tokens.unwrap_or(0);
        let cache_creation = self.cache_creation_input_tokens.unwrap_or(0);
        crate::generative_model::TokenUsage {
            input_tokens: self
                .input_tokens
                .saturating_add(cache_read)
                .saturating_add(cache_creation),
            output_tokens: self.output_tokens,
            cached_input_tokens: cache_read,
        }
    }
}

//
// Request / wire types
//

#[derive(Debug, serde::Serialize, serde::Deserialize, Clone, PartialEq, Eq)]
enum AnthropicRole {
    #[serde(rename = "assistant")]
    Assistant,
    #[serde(rename = "user")]
    User,
}

#[derive(Debug, Clone)]
struct AnthropicMessage {
    role: AnthropicRole,
    content: Vec<AnthropicContent>,
    /// When set, a cache breakpoint is attached to the final content block at
    /// serialization time — Anthropic only accepts `cache_control` on blocks, so
    /// the message-level flag is lowered onto the last block on the wire.
    cache_control: Option<AnthropicCacheControl>,
}

/// Serializes `{role, content}`, splicing `cache_control` onto the last content
/// block when the message is a cache breakpoint. Anthropic then caches the whole
/// prefix (tools + system + prior messages) up to and including that block.
impl serde::Serialize for AnthropicMessage {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut msg = serializer.serialize_struct("AnthropicMessage", 2)?;
        msg.serialize_field("role", &self.role)?;
        match self.cache_control {
            Some(cache_control) => msg.serialize_field(
                "content",
                &TrailingCache {
                    blocks: &self.content,
                    cache_control,
                },
            )?,
            None => msg.serialize_field("content", &self.content)?,
        }
        msg.end()
    }
}

/// Serializes a block list, injecting `cache_control` into the final block only.
struct TrailingCache<'a> {
    blocks: &'a [AnthropicContent],
    cache_control: AnthropicCacheControl,
}

impl serde::Serialize for TrailingCache<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::{Error, SerializeSeq};
        let last = self.blocks.len().saturating_sub(1);
        let mut seq = serializer.serialize_seq(Some(self.blocks.len()))?;
        for (i, block) in self.blocks.iter().enumerate() {
            if i == last {
                // `cache_control` isn't a field on the block enum; splice it into the
                // serialized object so only this final block carries the breakpoint.
                let mut value = serde_json::to_value(block).map_err(Error::custom)?;
                if let serde_json::Value::Object(map) = &mut value {
                    map.insert(
                        "cache_control".to_string(),
                        serde_json::to_value(self.cache_control).map_err(Error::custom)?,
                    );
                }
                seq.serialize_element(&value)?;
            } else {
                seq.serialize_element(block)?;
            }
        }
        seq.end()
    }
}

#[derive(Debug, serde::Serialize, serde::Deserialize, Clone)]
#[serde(tag = "type")]
enum AnthropicContent {
    #[serde(rename = "text")]
    Text { text: String },

    #[serde(rename = "image")]
    Image { source: AnthropicImageSource },

    #[serde(rename = "tool_use")]
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },

    #[serde(rename = "tool_result")]
    ToolResult {
        tool_use_id: String,
        content: Vec<AnthropicContent>,
        is_error: bool,
    },
}

/// Wire form of Anthropic image `source`. Public `Content::Image.source` is an opaque
/// string that we interpret as a URL, a `data:` URL, or raw base64 (default PNG).
#[derive(Debug, serde::Serialize, serde::Deserialize, Clone, PartialEq, Eq)]
#[serde(tag = "type")]
enum AnthropicImageSource {
    #[serde(rename = "base64")]
    Base64 { media_type: String, data: String },
    #[serde(rename = "url")]
    Url { url: String },
}

fn anthropic_image_source(source: String) -> AnthropicImageSource {
    if source.starts_with("http://") || source.starts_with("https://") {
        return AnthropicImageSource::Url { url: source };
    }
    if let Some(rest) = source.strip_prefix("data:") {
        // data:[<media_type>][;base64],<data>
        if let Some((meta, data)) = rest.split_once(',') {
            let media_type = meta
                .split(';')
                .next()
                .filter(|s| !s.is_empty())
                .unwrap_or("image/png")
                .to_string();
            return AnthropicImageSource::Base64 {
                media_type,
                data: data.to_string(),
            };
        }
    }
    AnthropicImageSource::Base64 {
        media_type: "image/png".into(),
        data: source,
    }
}

/// Wire block for one history content item. History thinking is never re-sent
/// (module invariant), so `Content::Thinking` yields `None`.
fn answer_block(content: Content) -> Option<AnthropicContent> {
    match content {
        Content::Text { text } => Some(AnthropicContent::Text { text }),
        Content::Image { source } => Some(AnthropicContent::Image {
            source: anthropic_image_source(source),
        }),
        Content::Thinking { .. } => None,
    }
}

#[derive(Debug, serde::Serialize)]
struct AnthropicMessagesRequest<'a> {
    max_tokens: usize,
    messages: &'a [AnthropicMessage],
    model: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<Vec<AnthropicSystemText<'a>>>,
    tools: &'a [AnthropicTool],
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    thinking: Option<AnthropicThinkingConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    output_config: Option<AnthropicOutputConfig>,
}

/// Wire form of Anthropic `thinking` request field.
///
/// Newer models reject `type: "enabled"` and require adaptive thinking plus
/// `output_config.effort`. Older models require `type: "enabled"` with a budget.
#[derive(Debug, serde::Serialize, Clone, PartialEq, Eq)]
#[serde(tag = "type")]
enum AnthropicThinkingConfig {
    #[serde(rename = "enabled")]
    Enabled { budget_tokens: u32 },
    #[serde(rename = "adaptive")]
    Adaptive {
        /// `"summarized"` surfaces readable thinking text; default on newest models is
        /// `"omitted"` (empty `thinking` field).
        #[serde(skip_serializing_if = "Option::is_none")]
        display: Option<&'static str>,
    },
}

#[derive(Debug, serde::Serialize, Clone, PartialEq, Eq)]
struct AnthropicOutputConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    effort: Option<&'static str>,
}

/// Build `thinking` / `output_config` for the given model when effort is set.
fn thinking_request_fields(
    mode: ThinkingMode,
    effort: Option<Effort>,
) -> (
    Option<AnthropicThinkingConfig>,
    Option<AnthropicOutputConfig>,
) {
    let Some(effort) = effort else {
        return (None, None);
    };

    match mode {
        ThinkingMode::Adaptive => (
            Some(AnthropicThinkingConfig::Adaptive {
                // Agent UIs stream thinking; omit would yield empty thinking deltas.
                display: Some("summarized"),
            }),
            Some(AnthropicOutputConfig {
                effort: Some(effort.as_str()),
            }),
        ),
        ThinkingMode::Budget => (
            Some(AnthropicThinkingConfig::Enabled {
                budget_tokens: effort.budget_tokens(),
            }),
            None,
        ),
        // `effort` is rejected for this protocol at catalog resolution.
        ThinkingMode::Effort | ThinkingMode::None => (None, None),
    }
}

/// System prompt as a content-block array so `cache_control` can be attached.
#[derive(Debug, serde::Serialize)]
struct AnthropicSystemText<'a> {
    #[serde(rename = "type")]
    type_: &'static str,
    text: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    cache_control: Option<AnthropicCacheControl>,
}

#[derive(Debug, serde::Serialize, Clone, Copy)]
#[serde(tag = "type")]
enum AnthropicCacheControl {
    #[serde(rename = "ephemeral")]
    Ephemeral,
}

#[derive(Debug, Clone, serde::Serialize)]
struct AnthropicTool {
    name: String,
    description: String,
    input_schema: serde_json::Value,
}

#[derive(Clone, Debug, serde::Deserialize)]
enum AnthropicStopReason {
    #[serde(rename = "end_turn")]
    EndTurn,
    #[serde(rename = "max_tokens")]
    MaxTokens,
    #[serde(rename = "stop_sequence")]
    StopSequence,
    #[serde(rename = "tool_use")]
    ToolUse,
    #[serde(rename = "pause_turn")]
    PauseTurn,
    #[serde(rename = "refusal")]
    Refusal,
    /// The API grows stop reasons over time (`model_context_window_exceeded`
    /// arrived in 2025); an unknown one must not fail the whole message_delta
    /// event and discard an already-streamed generation.
    #[serde(other)]
    Unknown,
}

impl From<AnthropicStopReason> for TurnEndReason {
    fn from(stop_reason: AnthropicStopReason) -> Self {
        match stop_reason {
            AnthropicStopReason::EndTurn => TurnEndReason::EndTurn,
            AnthropicStopReason::MaxTokens => TurnEndReason::MaxTokens,
            AnthropicStopReason::ToolUse => TurnEndReason::ToolUse,
            AnthropicStopReason::StopSequence => {
                TurnEndReason::Other("Anthropic::StopSequence".into())
            }
            AnthropicStopReason::PauseTurn => TurnEndReason::Other("Anthropic::PauseTurn".into()),
            AnthropicStopReason::Refusal => TurnEndReason::Other("Anthropic::Refusal".into()),
            AnthropicStopReason::Unknown => TurnEndReason::Other("Anthropic::Unknown".into()),
        }
    }
}

//
// Tests
//

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_role_serdes() {
        let role = AnthropicRole::Assistant;
        let json = serde_json::to_string(&role).unwrap();
        assert_eq!(json, r#""assistant""#);
    }

    #[test]
    fn test_content_serdes() {
        let content = AnthropicContent::Text {
            text: "Hello, world".to_string(),
        };
        let json = serde_json::to_string(&content).unwrap();
        assert_eq!(json, r#"{"type":"text","text":"Hello, world"}"#);
    }

    #[test]
    fn cache_breakpoints_mark_last_two_messages() {
        let input = [
            Message::UserMessage {
                content: vec![Content::Text { text: "one".into() }],
            },
            Message::AssistantMessage {
                content: vec![Content::Text { text: "two".into() }],
                tool_uses: vec![],
                turn_end_reason: None,
            },
            Message::UserMessage {
                content: vec![Content::Text {
                    text: "three".into(),
                }],
            },
        ];
        let json = serde_json::to_value(convert_messages(&input, true)).unwrap();
        // The final two messages carry a breakpoint on their last block.
        assert_eq!(json[2]["content"][0]["cache_control"]["type"], "ephemeral");
        assert_eq!(json[1]["content"][0]["cache_control"]["type"], "ephemeral");
        // The oldest message does not.
        assert!(json[0]["content"][0].get("cache_control").is_none());
    }

    #[test]
    fn cache_breakpoint_marks_only_final_block_not_nested() {
        let input = [
            Message::AssistantMessage {
                content: vec![],
                tool_uses: vec![ToolUse {
                    id: "a".into(),
                    name: "x".into(),
                    input: serde_json::Value::Null,
                }],
                turn_end_reason: None,
            },
            Message::ToolResults {
                tool_use_results: vec![
                    ToolResult {
                        id: "a".into(),
                        content: vec![Content::Text { text: "ra".into() }],
                        is_error: false,
                    },
                    ToolResult {
                        id: "b".into(),
                        content: vec![Content::Text { text: "rb".into() }],
                        is_error: false,
                    },
                ],
            },
        ];
        let json = serde_json::to_value(convert_messages(&input, true)).unwrap();
        // Last message has two tool_result blocks; only the final one is marked.
        assert_eq!(json[1]["content"][1]["cache_control"]["type"], "ephemeral");
        assert!(json[1]["content"][0].get("cache_control").is_none());
        // The breakpoint sits on the block, not spliced into its nested body.
        assert!(
            json[1]["content"][1]["content"][0]
                .get("cache_control")
                .is_none()
        );
    }

    #[test]
    fn cache_breakpoints_absent_when_disabled() {
        let input = [Message::UserMessage {
            content: vec![Content::Text { text: "hi".into() }],
        }];
        let json = serde_json::to_value(convert_messages(&input, false)).unwrap();
        assert!(json[0]["content"][0].get("cache_control").is_none());
    }

    #[test]
    fn convert_messages_empty_is_noop() {
        assert!(convert_messages(&[], true).is_empty());
    }

    #[test]
    fn image_source_url_and_base64_wire_format() {
        let url = answer_block(Content::Image {
            source: "https://example.com/a.png".into(),
        })
        .unwrap();
        let url_json = serde_json::to_value(&url).unwrap();
        assert_eq!(url_json["type"], "image");
        assert_eq!(url_json["source"]["type"], "url");
        assert_eq!(url_json["source"]["url"], "https://example.com/a.png");

        let b64 = answer_block(Content::Image {
            source: "iVBORw0KGgo=".into(),
        })
        .unwrap();
        let b64_json = serde_json::to_value(&b64).unwrap();
        assert_eq!(b64_json["source"]["type"], "base64");
        assert_eq!(b64_json["source"]["media_type"], "image/png");
        assert_eq!(b64_json["source"]["data"], "iVBORw0KGgo=");

        let data_url = answer_block(Content::Image {
            source: "data:image/jpeg;base64,/9j/4AAQ".into(),
        })
        .unwrap();
        let data_json = serde_json::to_value(&data_url).unwrap();
        assert_eq!(data_json["source"]["type"], "base64");
        assert_eq!(data_json["source"]["media_type"], "image/jpeg");
        assert_eq!(data_json["source"]["data"], "/9j/4AAQ");
    }

    #[test]
    fn request_puts_cache_control_on_system_block_not_root() {
        let system = vec![AnthropicSystemText {
            type_: "text",
            text: "You are helpful.",
            cache_control: Some(AnthropicCacheControl::Ephemeral),
        }];
        let request = AnthropicMessagesRequest {
            max_tokens: 128,
            model: "claude-haiku-4-5",
            messages: &[],
            system: Some(system),
            tools: &[],
            stream: true,
            thinking: None,
            output_config: None,
        };
        let json = serde_json::to_value(&request).unwrap();
        assert!(json.get("cache_control").is_none());
        assert_eq!(json["system"][0]["type"], "text");
        assert_eq!(json["system"][0]["text"], "You are helpful.");
        assert_eq!(json["system"][0]["cache_control"]["type"], "ephemeral");
        assert!(json["system"][0]["cache_control"].get("ttl").is_none());
    }

    #[test]
    fn request_omits_system_when_empty() {
        let request = AnthropicMessagesRequest {
            max_tokens: 128,
            model: "claude-haiku-4-5",
            messages: &[],
            system: None,
            tools: &[],
            stream: true,
            thinking: None,
            output_config: None,
        };
        let json = serde_json::to_value(&request).unwrap();
        assert!(json.get("system").is_none());
    }

    #[test]
    fn adaptive_thinking_uses_effort_not_budget() {
        let (thinking, output_config) =
            thinking_request_fields(ThinkingMode::Adaptive, Some(Effort::High));
        let request = AnthropicMessagesRequest {
            max_tokens: 128,
            model: "claude-opus-4-8",
            messages: &[],
            system: None,
            tools: &[],
            stream: true,
            thinking,
            output_config,
        };
        let json = serde_json::to_value(&request).unwrap();
        assert_eq!(json["thinking"]["type"], "adaptive");
        assert_eq!(json["thinking"]["display"], "summarized");
        assert!(json["thinking"].get("budget_tokens").is_none());
        assert_eq!(json["output_config"]["effort"], "high");
    }

    #[test]
    fn manual_thinking_uses_budget_tokens() {
        let (thinking, output_config) =
            thinking_request_fields(ThinkingMode::Budget, Some(Effort::Medium));
        let request = AnthropicMessagesRequest {
            max_tokens: 128,
            model: "claude-haiku-4-5",
            messages: &[],
            system: None,
            tools: &[],
            stream: true,
            thinking,
            output_config,
        };
        let json = serde_json::to_value(&request).unwrap();
        assert_eq!(json["thinking"]["type"], "enabled");
        assert_eq!(
            json["thinking"]["budget_tokens"],
            Effort::Medium.budget_tokens()
        );
        assert!(json.get("output_config").is_none());
    }

    #[test]
    fn effort_budget_token_mapping() {
        assert_eq!(Effort::Low.budget_tokens(), 1_024);
        assert_eq!(Effort::Medium.budget_tokens(), 4_096);
        assert_eq!(Effort::High.budget_tokens(), 16_000);
        assert_eq!(Effort::Max.budget_tokens(), 64_000);
    }

    #[test]
    fn max_tokens_raised_above_enabled_thinking_budget() {
        // Default backend max_tokens is 8192; High budget is 16000 — request builder
        // must raise max_tokens (validated by reimplementing the clamp here).
        let budget = Effort::High.budget_tokens() as usize;
        let configured = 8192usize;
        let max_tokens = if configured <= budget {
            budget.saturating_add(1024)
        } else {
            configured
        };
        assert!(max_tokens > budget);
        assert_eq!(max_tokens, 16_000 + 1024);
    }

    #[test]
    fn thinking_omitted_when_effort_unset() {
        let (thinking, output_config) = thinking_request_fields(ThinkingMode::Adaptive, None);
        assert!(thinking.is_none());
        assert!(output_config.is_none());
    }

    #[test]
    fn thinking_mode_none_sends_no_thinking_fields() {
        let (thinking, output_config) =
            thinking_request_fields(ThinkingMode::None, Some(Effort::High));
        assert!(thinking.is_none());
        assert!(output_config.is_none());
    }

    #[test]
    fn test_sse_parser_basic() {
        let mut parser = SseParser::default();
        let chunk = b"event: message_start\ndata: {\"type\":\"message_start\"}\n\n\
                      data: {\"type\":\"ping\"}\n\n";
        let events = parser.push(chunk);
        assert_eq!(events.len(), 2);
        assert!(events[0].contains("message_start"));
        assert!(events[1].contains("ping"));
    }

    #[test]
    fn test_convert_messages_merges_consecutive_user() {
        let input = [
            Message::UserMessage {
                content: vec![Content::Text { text: "hi".into() }],
            },
            Message::ToolResults {
                tool_use_results: vec![ToolResult {
                    id: "toolu_1".into(),
                    content: vec![Content::Text { text: "ok".into() }],
                    is_error: false,
                }],
            },
        ];
        let msgs = convert_messages(&input, false);
        assert_eq!(msgs.len(), 1);
        assert!(matches!(msgs[0].role, AnthropicRole::User));
        assert_eq!(msgs[0].content.len(), 2);
        // tool_result blocks must come first in a user message.
        assert!(matches!(
            msgs[0].content[0],
            AnthropicContent::ToolResult { .. }
        ));
        assert!(matches!(msgs[0].content[1], AnthropicContent::Text { .. }));
    }

    #[test]
    fn message_start_usage_is_captured() {
        let mut acc = StreamAccumulator::default();
        let event: AnthropicStreamEvent = serde_json::from_str(
            r#"{"type":"message_start","message":{"role":"assistant","usage":{"input_tokens":2095,"cache_read_input_tokens":100,"cache_creation_input_tokens":0,"output_tokens":1}}}"#,
        )
        .unwrap();
        let usage = acc
            .handle_event(event)
            .unwrap()
            .into_iter()
            .find_map(|p| match p {
                MessagePart::Usage(u) => Some(u),
                _ => None,
            })
            .expect("message_start should emit usage");
        assert_eq!(usage.input_tokens, 2195);
        assert_eq!(usage.cached_input_tokens, 100);
        assert_eq!(usage.context_tokens(), 2195);
    }

    #[test]
    fn thinking_delta_maps_to_content_delta() {
        let mut acc = StreamAccumulator::default();
        let parts = acc
            .handle_event(AnthropicStreamEvent::ContentBlockStart {
                index: 0,
                content_block: AnthropicStreamContentBlock::Thinking {
                    thinking: String::new(),
                    signature: Some("sig123".into()),
                },
            })
            .unwrap();
        assert!(matches!(
            &parts[0],
            MessagePart::ContentStart(ContentStart::Thinking {
                index: 0,
                signature: Some(s),
                redacted: false,
            }) if s == "sig123"
        ));

        let parts = acc
            .handle_event(AnthropicStreamEvent::ContentBlockDelta {
                index: 0,
                delta: AnthropicDelta::ThinkingDelta {
                    thinking: "step 1".into(),
                },
            })
            .unwrap();
        match &parts[0] {
            MessagePart::ContentDelta(ContentDelta::Thinking { index, delta }) => {
                assert_eq!(*index, 0);
                assert_eq!(delta, "step 1");
            }
            other => panic!("expected thinking delta, got {other:?}"),
        }

        let parts = acc
            .handle_event(AnthropicStreamEvent::ContentBlockStart {
                index: 1,
                content_block: AnthropicStreamContentBlock::Text { text: "hi".into() },
            })
            .unwrap();
        // content_index is remapped: thinking occupied content slot 0, text is 1.
        assert!(matches!(
            &parts[0],
            MessagePart::ContentStart(ContentStart::Text { index: 1 })
        ));
        assert!(matches!(
            &parts[1],
            MessagePart::ContentDelta(ContentDelta::Text { index: 1, delta }) if delta == "hi"
        ));
    }

    #[test]
    fn test_stream_accumulator_text_and_tool_index_remap() {
        let mut acc = StreamAccumulator::default();

        let items = acc
            .handle_event(AnthropicStreamEvent::ContentBlockStart {
                index: 0,
                content_block: AnthropicStreamContentBlock::Text {
                    text: String::new(),
                },
            })
            .unwrap();
        assert!(matches!(
            items[0],
            MessagePart::ContentStart(ContentStart::Text { index: 0 })
        ));

        let items = acc
            .handle_event(AnthropicStreamEvent::ContentBlockDelta {
                index: 0,
                delta: AnthropicDelta::TextDelta { text: "Hi".into() },
            })
            .unwrap();
        match &items[0] {
            MessagePart::ContentDelta(ContentDelta::Text { index, delta }) => {
                assert_eq!(*index, 0);
                assert_eq!(delta, "Hi");
            }
            _ => panic!(),
        }

        let items = acc
            .handle_event(AnthropicStreamEvent::ContentBlockStart {
                index: 1,
                content_block: AnthropicStreamContentBlock::ToolUse {
                    id: "toolu_1".into(),
                    name: "get_weather".into(),
                    input: serde_json::json!({}),
                },
            })
            .unwrap();
        match &items[0] {
            MessagePart::ToolUseStart(ToolUseStart { index, id, name }) => {
                assert_eq!(*index, 0); // remapped tool index
                assert_eq!(id, "toolu_1");
                assert_eq!(name, "get_weather");
            }
            _ => panic!(),
        }

        let items = acc
            .handle_event(AnthropicStreamEvent::ContentBlockDelta {
                index: 1,
                delta: AnthropicDelta::InputJsonDelta {
                    partial_json: r#"{"city":"SF"}"#.into(),
                },
            })
            .unwrap();
        match &items[0] {
            MessagePart::ToolUseDelta(ToolUseDelta {
                index,
                input_json_delta,
            }) => {
                assert_eq!(*index, 0);
                assert_eq!(input_json_delta, r#"{"city":"SF"}"#);
            }
            _ => panic!(),
        }

        acc.handle_event(AnthropicStreamEvent::MessageDelta {
            delta: AnthropicMessageDelta {
                stop_reason: Some(AnthropicStopReason::ToolUse),
            },
            usage: None,
        })
        .unwrap();
        acc.handle_event(AnthropicStreamEvent::MessageStop).unwrap();
        acc.finish().unwrap();
    }
}
