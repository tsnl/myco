//! Google GenAI (Gemini Developer API) backend.
//!
//! The `generateContent` dialect served at `generativelanguage.googleapis.com`:
//! `{base_url}/v1beta/models/{api_id}:streamGenerateContent?alt=sse`.
//!
//! Ref: <https://ai.google.dev/api/generate-content>
//! Thinking: <https://ai.google.dev/gemini-api/docs/thinking>
//! Signatures: <https://ai.google.dev/gemini-api/docs/thought-signatures>
//!
//! Dialect choices worth knowing:
//!
//! - tool schemas go out as `parametersJsonSchema`, which takes the JSON
//!   Schema the harness already produces; the older `parameters` field is an
//!   OpenAPI subset that rejects ordinary keywords.
//! - a function call arrives whole in one part (`functionCall.args` is a JSON
//!   object, never a partial string), so each call is one `ToolUseStart` plus
//!   one `ToolUseDelta` carrying the serialized arguments.
//! - Gemini pairs a `functionResponse` with its call by `name` and position,
//!   not by id, so the minted wire ids are only used to validate the history
//!   shape. A call's `thoughtSignature` is required back on the next request
//!   (Gemini 3 rejects the turn without it); calls that came from another
//!   provider carry the documented validation-skip value instead.
//! - thinking is requested through `generationConfig.thinkingConfig`:
//!   `budget` sends a `thinkingBudget` mapped from [`Effort`], `effort` a
//!   Gemini 3 `thinkingLevel`, `adaptive` the dynamic budget `-1`, and `none`
//!   leaves the model's default and asks for no thought text. Thought text
//!   (`thought: true` parts) becomes [`Content::Thinking`] and, like every
//!   other backend, is never echoed back to the provider.

use std::sync::Arc;

use super::driver_core::SseAccumulator;
use super::*;

/// Gemini Developer API settings ([`BackendConfig::GoogleGenAI`]).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct GoogleGenAIBackendConfig {
    /// Base URL without the API version, e.g.
    /// `https://generativelanguage.googleapis.com`.
    pub base_url: String,
    /// A Google API key (`AIza…`, sent as `x-goog-api-key`) or an OAuth /
    /// gateway token (sent as `Authorization: Bearer`).
    pub auth_token: String,
    pub max_output_tokens: Option<usize>,
    pub debug_dump_api_requests: bool,
    /// Maximum serialized request body, including history, images, tools, and prompts.
    #[serde(default = "default_max_request_bytes")]
    pub max_request_bytes: usize,
    /// Transient-failure retry for this endpoint (`[gateways.NAME.retry]`).
    #[serde(default)]
    pub retry: RetryPolicy,
    /// When set, request thinking at this effort, shaped by the model's
    /// [`ThinkingMode`] (see the module docs). Defaults to [`Effort::DEFAULT`].
    pub effort: Option<Effort>,
}

impl Default for GoogleGenAIBackendConfig {
    fn default() -> Self {
        Self {
            // No built-in gateway: the catalog (config.toml) supplies base_url.
            base_url: String::new(),
            auth_token: String::new(),
            max_output_tokens: Some(8192),
            debug_dump_api_requests: false,
            max_request_bytes: MAX_REQUEST_BYTES,
            retry: RetryPolicy::default(),
            effort: Some(Effort::DEFAULT),
        }
    }
}

/// Stateless Gemini driver. Conversation history is owned by the caller.
pub struct GoogleGenAIGenerativeModel {
    model: ModelSpec,
    system_prompt: String,
    tools: Vec<GeminiFunctionDeclaration>,
    backend: GoogleGenAIBackendConfig,
    client: reqwest::Client,
}

impl GoogleGenAIGenerativeModel {
    pub fn new(
        config: GenerativeModelConfig,
        backend: GoogleGenAIBackendConfig,
    ) -> Result<Arc<Self>, ModelCreationError> {
        // Google API keys are recognisable by prefix and travel in their own
        // header; anything else is an OAuth or gateway token.
        let token = &backend.auth_token;
        let auth = if token.is_empty() {
            None
        } else if token.starts_with("AIza") {
            Some(("x-goog-api-key", token.clone()))
        } else {
            Some(("authorization", format!("Bearer {token}")))
        };
        let client = driver_core::build_client(auth, &[])?;

        let tools = config
            .tools
            .into_iter()
            .map(|spec| GeminiFunctionDeclaration {
                name: spec.name,
                description: spec.description,
                parameters_json_schema: spec.input_schema,
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

    /// Build (without sending) the streaming generateContent request.
    fn generate_request(&self, contents: &[GeminiContent]) -> reqwest::RequestBuilder {
        let system_instruction =
            (!self.system_prompt.is_empty()).then(|| GeminiSystemInstruction {
                parts: vec![GeminiPart::Text {
                    text: self.system_prompt.clone(),
                    thought: None,
                    thought_signature: None,
                }],
            });
        let tools = (!self.tools.is_empty()).then(|| {
            vec![GeminiTool {
                function_declarations: &self.tools,
            }]
        });
        let request = GeminiGenerateRequest {
            system_instruction,
            contents,
            tools,
            generation_config: GeminiGenerationConfig {
                max_output_tokens: self.backend.max_output_tokens,
                thinking_config: thinking_config(self.model.thinking, self.backend.effort),
            },
        };

        let base = self.backend.base_url.trim_end_matches('/');
        let model = model_resource(&self.model.api_id);
        self.client
            .post(format!(
                "{base}/v1beta/{model}:streamGenerateContent?alt=sse"
            ))
            .json(&request)
    }
}

/// The resource path for a wire id: bare ids name a published model; ids that
/// already carry a collection (`models/…`, `tunedModels/…`) pass through.
fn model_resource(api_id: &str) -> String {
    if api_id.contains('/') {
        api_id.to_string()
    } else {
        format!("models/{api_id}")
    }
}

impl GenerativeModel for GoogleGenAIGenerativeModel {
    fn generate(&self, input: &[Message]) -> AsyncStream<GenerationEvent> {
        if let Err(error) = validate_image_sizes(input, self.model.max_image_base64_bytes) {
            return driver_core::error_stream(error);
        }
        let contents = match convert_messages(input) {
            Ok(contents) => contents,
            Err(e) => return driver_core::error_stream(e),
        };
        driver_core::spawn_generate(
            self.generate_request(&contents),
            StreamAccumulator::default(),
            "Google GenAI",
            self.backend.debug_dump_api_requests,
            self.backend.max_request_bytes,
        )
    }
}

//
// Thinking
//

/// `thinkingBudget` for `thinking = "budget"`. [`Effort::budget_tokens`] fits
/// every Gemini 2.5 model except at `max`, which is clamped to the smallest
/// ceiling among them (Flash and Flash-Lite: 24,576) so no model rejects it.
fn thinking_budget(effort: Effort) -> u32 {
    match effort {
        Effort::Max => 24_576,
        other => other.budget_tokens(),
    }
}

/// `thinkingLevel` for `thinking = "effort"` (Gemini 3). `max` has no level
/// of its own.
fn thinking_level(effort: Effort) -> &'static str {
    match effort {
        Effort::Low => "LOW",
        Effort::Medium => "MEDIUM",
        Effort::High | Effort::Max => "HIGH",
    }
}

fn thinking_config(mode: ThinkingMode, effort: Option<Effort>) -> Option<GeminiThinkingConfig> {
    let effort = effort?;
    let config = match mode {
        ThinkingMode::None => return None,
        ThinkingMode::Adaptive => GeminiThinkingConfig {
            include_thoughts: true,
            thinking_budget: Some(-1),
            thinking_level: None,
        },
        ThinkingMode::Budget => GeminiThinkingConfig {
            include_thoughts: true,
            thinking_budget: Some(thinking_budget(effort) as i64),
            thinking_level: None,
        },
        ThinkingMode::Effort => GeminiThinkingConfig {
            include_thoughts: true,
            thinking_budget: None,
            thinking_level: Some(thinking_level(effort)),
        },
    };
    Some(config)
}

//
// Message conversion → `contents`
//

/// Documented placeholder for a function call that has no Gemini signature
/// (made by another provider, or stored before signatures were kept). Gemini 3
/// rejects a signature-less call outright; this value skips that validation.
const SKIP_SIGNATURE_VALIDATION: &str = "skip_thought_signature_validator";

fn convert_messages(input: &[Message]) -> Result<Vec<GeminiContent>, GenerateError> {
    // Gemini pairs results with calls by position, the same contract history
    // keeps; the ids are minted only to fail loud on a broken pairing.
    wire_tool_ids(input)?;
    let mut out: Vec<GeminiContent> = Vec::new();

    for (i, message) in input.iter().enumerate() {
        let (role, parts): (&'static str, Vec<GeminiPart>) = match message {
            Message::UserMessage { content } => ("user", answer_parts(content)),
            Message::ToolResults { tool_use_results } => {
                let Some(Message::AssistantMessage { tool_uses, .. }) =
                    i.checked_sub(1).map(|p| &input[p])
                else {
                    unreachable!("wire_tool_ids validated the pairing");
                };
                let mut parts = Vec::new();
                let mut images = Vec::new();
                for (result, call) in tool_use_results.iter().zip(tool_uses) {
                    let text = text_of(&result.content);
                    let result_images = images_of(&result.content);
                    let mut response = serde_json::Map::new();
                    let key = if result.is_error { "error" } else { "output" };
                    let value = if text.is_empty() && !result_images.is_empty() {
                        "[image attached below]".to_string()
                    } else {
                        text
                    };
                    response.insert(key.into(), serde_json::Value::String(value));
                    parts.push(GeminiPart::FunctionResponse {
                        function_response: GeminiFunctionResponse {
                            name: call.name.clone(),
                            response: serde_json::Value::Object(response),
                        },
                    });
                    images.extend(result_images);
                }
                // A functionResponse carries only JSON, so a result's images
                // ride as sibling parts of the same user turn.
                parts.extend(images.into_iter().map(image_part));
                ("user", parts)
            }
            Message::AssistantMessage {
                content,
                tool_uses,
                turn_end_reason: _,
            } => {
                // Thinking is kept for the session, never echoed back.
                let mut parts: Vec<GeminiPart> = content
                    .iter()
                    .filter_map(|c| match c {
                        Content::Text { text } if !text.is_empty() => Some(GeminiPart::Text {
                            text: text.clone(),
                            thought: None,
                            thought_signature: None,
                        }),
                        _ => None,
                    })
                    .collect();
                for tool_use in tool_uses {
                    parts.push(GeminiPart::FunctionCall {
                        function_call: GeminiFunctionCall {
                            name: tool_use.name.clone(),
                            args: tool_use.input.clone(),
                        },
                        thought_signature: Some(
                            tool_use
                                .thought_signature
                                .clone()
                                .unwrap_or_else(|| SKIP_SIGNATURE_VALIDATION.to_string()),
                        ),
                    });
                }
                // A thinking-only turn strips to nothing, and a model turn
                // without parts is rejected on every later request.
                if parts.is_empty() {
                    continue;
                }
                ("model", parts)
            }
        };
        if parts.is_empty() {
            continue;
        }
        // Gemini expects user and model turns to alternate; consecutive
        // same-role messages (a tool round followed by a user nudge) merge
        // into one turn, in order.
        if let Some(last) = out.last_mut()
            && last.role == role
        {
            last.parts.extend(parts);
            continue;
        }
        out.push(GeminiContent { role, parts });
    }

    Ok(out)
}

/// `Text` / `System` / `Image` parts of a user message, in order. Thinking
/// never appears in user content.
fn answer_parts(content: &[Content]) -> Vec<GeminiPart> {
    content
        .iter()
        .filter_map(|c| match c {
            Content::Text { text } | Content::System { text, .. } => Some(GeminiPart::Text {
                text: text.clone(),
                thought: None,
                thought_signature: None,
            }),
            Content::Image { source } => Some(image_part(source)),
            Content::Thinking { .. } => None,
        })
        .collect()
}

/// Text blocks joined by newlines; images and thinking are not text.
fn text_of(content: &[Content]) -> String {
    content
        .iter()
        .filter_map(|c| match c {
            Content::Text { text } | Content::System { text, .. } => Some(text.as_str()),
            Content::Image { .. } | Content::Thinking { .. } => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn images_of(content: &[Content]) -> Vec<&str> {
    content
        .iter()
        .filter_map(|c| match c {
            Content::Image { source } => Some(source.as_str()),
            _ => None,
        })
        .collect()
}

/// Remote URLs become `fileData` (the provider fetches and validates them);
/// `data:` URLs and raw base64 (assumed PNG) become `inlineData`.
fn image_part(source: &str) -> GeminiPart {
    if source.starts_with("http://") || source.starts_with("https://") {
        return GeminiPart::FileData {
            file_data: GeminiFileData {
                file_uri: source.to_string(),
            },
        };
    }
    let (mime_type, data) = match source
        .strip_prefix("data:")
        .and_then(|rest| rest.split_once(";base64,"))
    {
        Some((mime, data)) => (mime.to_string(), data.to_string()),
        None => ("image/png".to_string(), source.to_string()),
    };
    GeminiPart::InlineData {
        inline_data: GeminiBlob { mime_type, data },
    }
}

//
// Stream decoding
//

/// Which content block the most recent text part extended.
#[derive(Clone, Copy, PartialEq, Eq)]
enum OpenBlock {
    Text(usize),
    Thinking(usize),
}

#[derive(Default)]
struct StreamAccumulator {
    content_count: usize,
    tool_use_count: usize,
    /// Consecutive parts of one kind share a block; any other part closes it.
    open: Option<OpenBlock>,
    saw_tool_call: bool,
    stop_reason: Option<TurnEndReason>,
}

impl StreamAccumulator {
    fn handle_chunk(
        &mut self,
        chunk: GeminiStreamChunk,
    ) -> Result<Vec<MessagePart>, GenerationFailure> {
        if let Some(error) = chunk.error {
            return Err(stream_failure(error));
        }
        if let Some(reason) = chunk
            .prompt_feedback
            .and_then(|feedback| feedback.block_reason)
        {
            return Err(GenerateError::RefusalError(format!(
                "Google GenAI blocked the prompt: {reason}"
            ))
            .into());
        }

        let mut out = Vec::new();
        for candidate in chunk.candidates.into_iter().take(1) {
            for part in candidate.content.parts {
                out.extend(self.handle_part(part)?);
            }
            if let Some(finish_reason) = candidate.finish_reason {
                let reason = match finish_reason.as_str() {
                    "STOP" if self.saw_tool_call => TurnEndReason::ToolUse,
                    "STOP" => TurnEndReason::EndTurn,
                    "MAX_TOKENS" => TurnEndReason::MaxTokens,
                    "SAFETY"
                    | "RECITATION"
                    | "BLOCKLIST"
                    | "PROHIBITED_CONTENT"
                    | "SPII"
                    | "IMAGE_SAFETY"
                    | "IMAGE_PROHIBITED_CONTENT"
                    | "IMAGE_RECITATION" => {
                        return Err(GenerateError::RefusalError(format!(
                            "Google GenAI stopped for {finish_reason}{}",
                            candidate
                                .finish_message
                                .map(|m| format!(": {m}"))
                                .unwrap_or_default()
                        ))
                        .into());
                    }
                    // The model emitted a call it could not express as JSON;
                    // a fresh attempt is the only remedy.
                    "MALFORMED_FUNCTION_CALL" => {
                        return Err(GenerateError::MalformedResponseError(format!(
                            "Google GenAI stopped for MALFORMED_FUNCTION_CALL{}",
                            candidate
                                .finish_message
                                .map(|m| format!(": {m}"))
                                .unwrap_or_default()
                        ))
                        .into());
                    }
                    other => TurnEndReason::Other(format!("GoogleGenAI::{other}")),
                };
                self.stop_reason = Some(reason.clone());
                out.push(MessagePart::TurnEndReason(reason));
            }
        }

        // Every chunk carries cumulative counts; the accumulator keeps the latest.
        if let Some(usage) = chunk.usage_metadata {
            out.push(MessagePart::Usage(usage.into_token_usage()));
        }

        Ok(out)
    }

    fn handle_part(&mut self, part: GeminiPartIn) -> Result<Vec<MessagePart>, GenerateError> {
        let mut out = Vec::new();
        if let Some(call) = part.function_call {
            let name = call.name.unwrap_or_default();
            if name.is_empty() {
                return Err(GenerateError::MalformedResponseError(
                    "Google GenAI: function call without a name".into(),
                ));
            }
            self.open = None;
            let index = self.tool_use_count;
            self.tool_use_count += 1;
            self.saw_tool_call = true;
            out.push(MessagePart::ToolUseStart(ToolUseStart {
                index,
                name,
                thought_signature: part.thought_signature.filter(|s| !s.is_empty()),
            }));
            if let Some(args) = call.args {
                out.push(MessagePart::ToolUseDelta(ToolUseDelta {
                    index,
                    input_json_delta: args.to_string(),
                }));
            }
            return Ok(out);
        }

        let Some(text) = part.text else {
            // Media or server-side tool parts: nothing myco presents.
            self.open = None;
            return Ok(out);
        };
        if text.is_empty() {
            return Ok(out);
        }
        if part.thought.unwrap_or(false) {
            let index = match self.open {
                Some(OpenBlock::Thinking(index)) => index,
                _ => {
                    let index = self.open_block(OpenBlock::Thinking);
                    out.push(MessagePart::ContentStart(ContentStart::Thinking {
                        index,
                        signature: None,
                        redacted: false,
                    }));
                    index
                }
            };
            out.push(MessagePart::ContentDelta(ContentDelta::Thinking {
                index,
                delta: text,
            }));
        } else {
            let index = match self.open {
                Some(OpenBlock::Text(index)) => index,
                _ => {
                    let index = self.open_block(OpenBlock::Text);
                    out.push(MessagePart::ContentStart(ContentStart::Text { index }));
                    index
                }
            };
            out.push(MessagePart::ContentDelta(ContentDelta::Text {
                index,
                delta: text,
            }));
        }
        Ok(out)
    }

    fn open_block(&mut self, kind: fn(usize) -> OpenBlock) -> usize {
        let index = self.content_count;
        self.content_count += 1;
        self.open = Some(kind(index));
        index
    }
}

/// A 200 whose body carries a `google.rpc.Status`. Only the statuses Google
/// documents as transient permit replay; prose and unknown statuses never do.
fn stream_failure(error: serde_json::Value) -> GenerationFailure {
    let retryable = matches!(
        error.get("status").and_then(serde_json::Value::as_str),
        Some("UNAVAILABLE" | "RESOURCE_EXHAUSTED" | "INTERNAL" | "DEADLINE_EXCEEDED")
    );
    GenerationFailure {
        cause: GenerateError::ExecutionError(format!("Google GenAI stream error: {error}")),
        retryable,
        retry_after: None,
    }
}

impl SseAccumulator for StreamAccumulator {
    fn handle_data(&mut self, data: &str) -> Result<Vec<MessagePart>, GenerationFailure> {
        let chunk: GeminiStreamChunk = serde_json::from_str(data).map_err(|e| {
            GenerateError::MalformedResponseError(format!(
                "Failed to parse Google GenAI SSE event JSON: {e}; data={data}"
            ))
        })?;
        self.handle_chunk(chunk)
    }

    /// No terminal event: the finish reason and final usage share the last
    /// chunk, and the server closes the stream.
    fn finished(&self) -> bool {
        false
    }

    fn finish(self) -> Result<(), GenerateError> {
        // Arguments arrive as parsed JSON, so there is nothing left to validate.
        driver_core::validate_finish(
            "Google GenAI",
            self.stop_reason.is_some(),
            std::iter::empty(),
        )
    }
}

//
// Wire types
//

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct GeminiGenerateRequest<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    system_instruction: Option<GeminiSystemInstruction>,
    contents: &'a [GeminiContent],
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<GeminiTool<'a>>>,
    generation_config: GeminiGenerationConfig,
}

#[derive(Debug, serde::Serialize)]
struct GeminiSystemInstruction {
    parts: Vec<GeminiPart>,
}

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct GeminiTool<'a> {
    function_declarations: &'a [GeminiFunctionDeclaration],
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct GeminiFunctionDeclaration {
    name: String,
    description: String,
    parameters_json_schema: serde_json::Value,
}

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct GeminiGenerationConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    max_output_tokens: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    thinking_config: Option<GeminiThinkingConfig>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct GeminiThinkingConfig {
    include_thoughts: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    thinking_budget: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    thinking_level: Option<&'static str>,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
struct GeminiContent {
    role: &'static str,
    parts: Vec<GeminiPart>,
}

/// Exactly one payload per part, so the variants serialize flat.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(untagged)]
enum GeminiPart {
    #[serde(rename_all = "camelCase")]
    Text {
        text: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        thought: Option<bool>,
        #[serde(skip_serializing_if = "Option::is_none")]
        thought_signature: Option<String>,
    },
    #[serde(rename_all = "camelCase")]
    InlineData { inline_data: GeminiBlob },
    #[serde(rename_all = "camelCase")]
    FileData { file_data: GeminiFileData },
    #[serde(rename_all = "camelCase")]
    FunctionCall {
        function_call: GeminiFunctionCall,
        #[serde(skip_serializing_if = "Option::is_none")]
        thought_signature: Option<String>,
    },
    #[serde(rename_all = "camelCase")]
    FunctionResponse {
        function_response: GeminiFunctionResponse,
    },
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct GeminiBlob {
    mime_type: String,
    data: String,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct GeminiFileData {
    file_uri: String,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
struct GeminiFunctionCall {
    name: String,
    args: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
struct GeminiFunctionResponse {
    name: String,
    response: serde_json::Value,
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct GeminiStreamChunk {
    #[serde(default)]
    candidates: Vec<GeminiCandidate>,
    #[serde(default)]
    prompt_feedback: Option<GeminiPromptFeedback>,
    #[serde(default)]
    usage_metadata: Option<GeminiUsage>,
    /// A failure reported inside a 200 stream (a `google.rpc.Status`).
    #[serde(default)]
    error: Option<serde_json::Value>,
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct GeminiPromptFeedback {
    #[serde(default)]
    block_reason: Option<String>,
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct GeminiCandidate {
    #[serde(default)]
    content: GeminiContentIn,
    #[serde(default)]
    finish_reason: Option<String>,
    #[serde(default)]
    finish_message: Option<String>,
}

#[derive(Debug, Default, serde::Deserialize)]
struct GeminiContentIn {
    #[serde(default)]
    parts: Vec<GeminiPartIn>,
}

/// One streamed part. Fields are optional because a part carries exactly one
/// payload; parts myco does not present (`inlineData`, `executableCode`, …)
/// decode to neither text nor a call.
#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct GeminiPartIn {
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    thought: Option<bool>,
    #[serde(default)]
    thought_signature: Option<String>,
    #[serde(default)]
    function_call: Option<GeminiFunctionCallIn>,
}

#[derive(Debug, serde::Deserialize)]
struct GeminiFunctionCallIn {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    args: Option<serde_json::Value>,
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct GeminiUsage {
    #[serde(default)]
    prompt_token_count: u64,
    #[serde(default)]
    candidates_token_count: u64,
    /// Reported separately from `candidatesTokenCount`.
    #[serde(default)]
    thoughts_token_count: u64,
    #[serde(default)]
    cached_content_token_count: u64,
}

impl GeminiUsage {
    fn into_token_usage(self) -> TokenUsage {
        TokenUsage {
            input_tokens: self.prompt_token_count,
            output_tokens: self.candidates_token_count + self.thoughts_token_count,
            cached_input_tokens: self.cached_content_token_count,
        }
    }
}

//
// Tests
//

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{
        assistant_tool, expect_text_delta, expect_thinking_delta, expect_tool_args_delta,
        expect_tool_start, expect_turn_end, tool_results, user,
    };

    fn chunk(json: serde_json::Value) -> GeminiStreamChunk {
        serde_json::from_value(json).expect("chunk")
    }

    fn wire(input: &[Message]) -> serde_json::Value {
        serde_json::to_value(convert_messages(input).unwrap()).unwrap()
    }

    #[test]
    fn system_parts_reach_the_provider_in_text_and_image_messages_without_metadata() {
        for with_image in [false, true] {
            let mut content = vec![Content::System {
                kind: "private_kind".into(),
                text: "runtime notice".into(),
                data: serde_json::json!({"private_metadata":true}),
            }];
            if with_image {
                content.push(Content::Image {
                    source: "data:image/png;base64,AAAA".into(),
                });
            }
            content.push(Content::Text {
                text: "human request".into(),
            });
            let json = wire(&[Message::UserMessage { content }]).to_string();
            assert!(json.contains("runtime notice"), "{json}");
            assert!(json.contains("human request"), "{json}");
            assert!(!json.contains("private_kind"), "{json}");
            assert!(!json.contains("private_metadata"), "{json}");
        }
    }

    #[test]
    fn roles_are_user_and_model_and_text_is_a_part() {
        let json = wire(&[user("hi"), crate::test_support::assistant("hello")]);
        assert_eq!(json[0]["role"], "user");
        assert_eq!(json[0]["parts"][0]["text"], "hi");
        assert_eq!(json[1]["role"], "model");
        assert_eq!(json[1]["parts"][0]["text"], "hello");
    }

    #[test]
    fn function_call_and_response_pair_by_name_and_position() {
        let input = [
            assistant_tool(
                Some("checking"),
                "bash",
                serde_json::json!({"command": "echo hi"}),
            ),
            tool_results(&["hi\n"]),
        ];
        let json = wire(&input);
        assert_eq!(json[0]["role"], "model");
        assert_eq!(json[0]["parts"][0]["text"], "checking");
        assert_eq!(json[0]["parts"][1]["functionCall"]["name"], "bash");
        assert_eq!(
            json[0]["parts"][1]["functionCall"]["args"],
            serde_json::json!({"command": "echo hi"})
        );
        assert_eq!(json[1]["role"], "user");
        assert_eq!(json[1]["parts"][0]["functionResponse"]["name"], "bash");
        assert_eq!(
            json[1]["parts"][0]["functionResponse"]["response"]["output"],
            "hi\n"
        );
    }

    #[test]
    fn error_results_use_the_error_key() {
        let input = [
            assistant_tool(None, "bash", serde_json::json!({})),
            Message::ToolResults {
                tool_use_results: vec![ToolResult::err("boom")],
            },
        ];
        let response = &wire(&input)[1]["parts"][0]["functionResponse"]["response"];
        assert_eq!(response["error"], "boom");
        assert!(response.get("output").is_none(), "{response}");
    }

    #[test]
    fn thought_signature_round_trips_and_foreign_calls_skip_validation() {
        let mut acc = StreamAccumulator::default();
        let items = acc
            .handle_chunk(chunk(serde_json::json!({
                "candidates": [{"content": {"role": "model", "parts": [
                    {"functionCall": {"name": "bash", "args": {"command": "date"}},
                     "thoughtSignature": "sig-abc"}
                ]}, "finishReason": "STOP"}]
            })))
            .unwrap();
        match &items[0] {
            MessagePart::ToolUseStart(ToolUseStart {
                thought_signature, ..
            }) => assert_eq!(thought_signature.as_deref(), Some("sig-abc")),
            other => panic!("expected a tool start, got {other:?}"),
        }
        expect_tool_args_delta(&items[1], 0, r#"{"command":"date"}"#);
        expect_turn_end(&items[2], TurnEndReason::ToolUse);

        let signed = Message::AssistantMessage {
            content: vec![],
            tool_uses: vec![ToolUse {
                name: "bash".into(),
                input: serde_json::json!({"command": "date"}),
                thought_signature: Some("sig-abc".into()),
            }],
            turn_end_reason: Some(TurnEndReason::ToolUse),
        };
        let json = wire(&[signed, tool_results(&["ok"])]);
        assert_eq!(json[0]["parts"][0]["thoughtSignature"], "sig-abc");

        // A call from another provider gets the documented skip value instead
        // of being rejected on every later turn.
        let json = wire(&[assistant_tool(None, "bash", serde_json::json!({}))]);
        assert_eq!(
            json[0]["parts"][0]["thoughtSignature"],
            SKIP_SIGNATURE_VALIDATION
        );
    }

    #[test]
    fn thinking_only_assistant_message_is_dropped() {
        let input = [Message::AssistantMessage {
            content: vec![Content::Thinking {
                text: "hmm".into(),
                signature: None,
                redacted: false,
            }],
            tool_uses: vec![],
            turn_end_reason: Some(TurnEndReason::EndTurn),
        }];
        assert!(convert_messages(&input).unwrap().is_empty());
    }

    #[test]
    fn consecutive_same_role_messages_merge_into_one_turn() {
        let input = [
            assistant_tool(None, "bash", serde_json::json!({})),
            tool_results(&["ok"]),
            user("now also do this"),
        ];
        let json = wire(&input);
        assert_eq!(json.as_array().unwrap().len(), 2);
        assert_eq!(json[1]["role"], "user");
        assert!(json[1]["parts"][0].get("functionResponse").is_some());
        assert_eq!(json[1]["parts"][1]["text"], "now also do this");
    }

    #[test]
    fn tool_result_images_follow_the_function_responses_in_the_same_turn() {
        let input = [
            Message::AssistantMessage {
                content: vec![],
                tool_uses: vec![
                    ToolUse {
                        name: "view_image".into(),
                        input: serde_json::json!({}),
                        thought_signature: None,
                    },
                    ToolUse {
                        name: "bash".into(),
                        input: serde_json::json!({}),
                        thought_signature: None,
                    },
                ],
                turn_end_reason: Some(TurnEndReason::ToolUse),
            },
            Message::ToolResults {
                tool_use_results: vec![
                    ToolResult::ok(vec![Content::Image {
                        source: "data:image/jpeg;base64,AAAA".into(),
                    }]),
                    ToolResult::text("ok"),
                ],
            },
        ];
        let parts = &wire(&input)[1]["parts"];
        assert_eq!(parts.as_array().unwrap().len(), 3);
        assert_eq!(parts[0]["functionResponse"]["name"], "view_image");
        assert_eq!(
            parts[0]["functionResponse"]["response"]["output"],
            "[image attached below]"
        );
        assert_eq!(parts[1]["functionResponse"]["name"], "bash");
        assert_eq!(parts[2]["inlineData"]["mimeType"], "image/jpeg");
        assert_eq!(parts[2]["inlineData"]["data"], "AAAA");
    }

    #[test]
    fn image_sources_pick_inline_or_file_data() {
        let input = [Message::UserMessage {
            content: vec![
                Content::Image {
                    source: "iVBOR".into(),
                },
                Content::Image {
                    source: "https://x.test/a.png".into(),
                },
                Content::Text {
                    text: "what are these?".into(),
                },
            ],
        }];
        let parts = &wire(&input)[0]["parts"];
        assert_eq!(parts[0]["inlineData"]["mimeType"], "image/png");
        assert_eq!(parts[0]["inlineData"]["data"], "iVBOR");
        assert_eq!(parts[1]["fileData"]["fileUri"], "https://x.test/a.png");
        assert_eq!(parts[2]["text"], "what are these?");
    }

    #[test]
    fn model_resource_prefixes_bare_ids_only() {
        assert_eq!(model_resource("gemini-2.5-pro"), "models/gemini-2.5-pro");
        assert_eq!(model_resource("tunedModels/x"), "tunedModels/x");
    }

    #[test]
    fn thinking_modes_shape_thinking_config() {
        assert_eq!(
            thinking_config(ThinkingMode::None, Some(Effort::High)),
            None
        );
        assert_eq!(thinking_config(ThinkingMode::Budget, None), None);
        assert_eq!(
            thinking_config(ThinkingMode::Adaptive, Some(Effort::Low)),
            Some(GeminiThinkingConfig {
                include_thoughts: true,
                thinking_budget: Some(-1),
                thinking_level: None,
            })
        );
        assert_eq!(
            thinking_config(ThinkingMode::Budget, Some(Effort::High)),
            Some(GeminiThinkingConfig {
                include_thoughts: true,
                thinking_budget: Some(16_000),
                thinking_level: None,
            })
        );
        // `max` stays within every Gemini 2.5 model's ceiling.
        assert_eq!(thinking_budget(Effort::Max), 24_576);
        assert_eq!(
            thinking_config(ThinkingMode::Effort, Some(Effort::Max)),
            Some(GeminiThinkingConfig {
                include_thoughts: true,
                thinking_budget: None,
                thinking_level: Some("HIGH"),
            })
        );
        assert_eq!(thinking_level(Effort::Medium), "MEDIUM");
        let json =
            serde_json::to_value(thinking_config(ThinkingMode::Effort, Some(Effort::Low)).unwrap())
                .unwrap();
        assert_eq!(
            json,
            serde_json::json!({"includeThoughts": true, "thinkingLevel": "LOW"})
        );
    }

    #[test]
    fn stream_thoughts_then_text_then_usage() {
        let mut acc = StreamAccumulator::default();

        let items = acc
            .handle_chunk(chunk(serde_json::json!({
                "candidates": [{"content": {"parts": [{"text": "step one", "thought": true}]}}],
                "usageMetadata": {"promptTokenCount": 12}
            })))
            .unwrap();
        assert!(matches!(
            items[0],
            MessagePart::ContentStart(ContentStart::Thinking { index: 0, .. })
        ));
        expect_thinking_delta(&items[1], 0, "step one");
        assert!(matches!(items[2], MessagePart::Usage(_)));

        // A second thought part extends the open block.
        let items = acc
            .handle_chunk(chunk(serde_json::json!({
                "candidates": [{"content": {"parts": [{"text": " two", "thought": true}]}}]
            })))
            .unwrap();
        expect_thinking_delta(&items[0], 0, " two");
        assert_eq!(items.len(), 1);

        let items = acc
            .handle_chunk(chunk(serde_json::json!({
                "candidates": [{"content": {"parts": [{"text": "Paris"}]}}]
            })))
            .unwrap();
        assert!(matches!(
            items[0],
            MessagePart::ContentStart(ContentStart::Text { index: 1 })
        ));
        expect_text_delta(&items[1], 1, "Paris");

        let items = acc
            .handle_chunk(chunk(serde_json::json!({
                "candidates": [{"content": {"parts": [{"text": "!"}]}, "finishReason": "STOP"}],
                "usageMetadata": {
                    "promptTokenCount": 12,
                    "candidatesTokenCount": 3,
                    "thoughtsTokenCount": 40,
                    "cachedContentTokenCount": 10
                }
            })))
            .unwrap();
        expect_text_delta(&items[0], 1, "!");
        expect_turn_end(&items[1], TurnEndReason::EndTurn);
        match items[2] {
            MessagePart::Usage(usage) => {
                assert_eq!(usage.input_tokens, 12);
                assert_eq!(usage.output_tokens, 43);
                assert_eq!(usage.cached_input_tokens, 10);
            }
            ref other => panic!("expected usage, got {other:?}"),
        }
        acc.finish().unwrap();
    }

    #[test]
    fn parallel_function_calls_get_distinct_indices_and_close_the_text_block() {
        let mut acc = StreamAccumulator::default();
        let items = acc
            .handle_chunk(chunk(serde_json::json!({
                "candidates": [{"content": {"parts": [
                    {"text": "Running both."},
                    {"functionCall": {"name": "bash", "args": {"command": "ls"}}},
                    {"functionCall": {"name": "manual"}},
                    {"text": "Done."}
                ]}, "finishReason": "STOP"}]
            })))
            .unwrap();
        assert!(matches!(
            items[0],
            MessagePart::ContentStart(ContentStart::Text { index: 0 })
        ));
        expect_text_delta(&items[1], 0, "Running both.");
        expect_tool_start(&items[2], 0, "bash");
        expect_tool_args_delta(&items[3], 0, r#"{"command":"ls"}"#);
        expect_tool_start(&items[4], 1, "manual");
        // No args: no delta, and the accumulator reads it as `{}`.
        assert!(matches!(
            items[5],
            MessagePart::ContentStart(ContentStart::Text { index: 1 })
        ));
        expect_text_delta(&items[6], 1, "Done.");
        expect_turn_end(&items[7], TurnEndReason::ToolUse);
        acc.finish().unwrap();
    }

    #[test]
    fn finish_reasons_map_to_turn_ends_and_refusals() {
        let mut acc = StreamAccumulator::default();
        let items = acc
            .handle_chunk(chunk(serde_json::json!({
                "candidates": [{"content": {"parts": []}, "finishReason": "MAX_TOKENS"}]
            })))
            .unwrap();
        expect_turn_end(&items[0], TurnEndReason::MaxTokens);

        let mut acc = StreamAccumulator::default();
        let err = acc
            .handle_chunk(chunk(serde_json::json!({
                "candidates": [{"finishReason": "SAFETY"}]
            })))
            .unwrap_err();
        assert!(
            matches!(err.cause, GenerateError::RefusalError(_)),
            "{err:?}"
        );

        let mut acc = StreamAccumulator::default();
        let err = acc
            .handle_chunk(chunk(serde_json::json!({
                "candidates": [{"finishReason": "MALFORMED_FUNCTION_CALL",
                                "finishMessage": "could not parse"}]
            })))
            .unwrap_err();
        match err.cause {
            GenerateError::MalformedResponseError(msg) => {
                assert!(msg.contains("could not parse"), "{msg}")
            }
            other => panic!("expected MalformedResponseError, got {other:?}"),
        }

        let mut acc = StreamAccumulator::default();
        let items = acc
            .handle_chunk(chunk(serde_json::json!({
                "candidates": [{"finishReason": "UNEXPECTED_TOOL_CALL"}]
            })))
            .unwrap();
        expect_turn_end(
            &items[0],
            TurnEndReason::Other("GoogleGenAI::UNEXPECTED_TOOL_CALL".into()),
        );
    }

    #[test]
    fn blocked_prompt_is_a_refusal() {
        let mut acc = StreamAccumulator::default();
        let err = acc
            .handle_chunk(chunk(serde_json::json!({
                "promptFeedback": {"blockReason": "PROHIBITED_CONTENT"}
            })))
            .unwrap_err();
        match err.cause {
            GenerateError::RefusalError(msg) => {
                assert!(msg.contains("PROHIBITED_CONTENT"), "{msg}")
            }
            other => panic!("expected RefusalError, got {other:?}"),
        }
    }

    #[test]
    fn stream_errors_retry_only_on_transient_statuses() {
        for (status, retryable) in [
            ("UNAVAILABLE", true),
            ("RESOURCE_EXHAUSTED", true),
            ("INVALID_ARGUMENT", false),
            ("PERMISSION_DENIED", false),
        ] {
            let mut acc = StreamAccumulator::default();
            let err = acc
                .handle_chunk(chunk(serde_json::json!({
                    "error": {"code": 503, "message": "try later", "status": status}
                })))
                .unwrap_err();
            assert_eq!(err.retryable, retryable, "{status}");
            assert!(err.cause.to_string().contains(status), "{err:?}");
            assert!(err.cause.to_string().contains("try later"), "{err:?}");
        }
    }

    #[test]
    fn function_call_without_name_is_malformed() {
        let mut acc = StreamAccumulator::default();
        let err = acc
            .handle_chunk(chunk(serde_json::json!({
                "candidates": [{"content": {"parts": [{"functionCall": {"args": {}}}]}}]
            })))
            .unwrap_err();
        assert!(
            matches!(err.cause, GenerateError::MalformedResponseError(_)),
            "{err:?}"
        );
    }

    #[test]
    fn stream_without_finish_reason_is_malformed() {
        let mut acc = StreamAccumulator::default();
        acc.handle_chunk(chunk(serde_json::json!({
            "candidates": [{"content": {"parts": [{"text": "truncated"}]}}]
        })))
        .unwrap();
        let err = acc.finish().unwrap_err();
        assert!(matches!(err, GenerateError::MalformedResponseError(_)));
    }

    #[test]
    fn unpresented_parts_are_skipped() {
        let mut acc = StreamAccumulator::default();
        let items = acc
            .handle_chunk(chunk(serde_json::json!({
                "candidates": [{"content": {"parts": [
                    {"inlineData": {"mimeType": "image/png", "data": "AAAA"}},
                    {"text": ""}
                ]}, "finishReason": "STOP"}]
            })))
            .unwrap();
        expect_turn_end(&items[0], TurnEndReason::EndTurn);
        assert_eq!(items.len(), 1);
    }
}
