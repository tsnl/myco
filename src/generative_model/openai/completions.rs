//! Chat Completions dialect (`{base_url}/chat/completions`): OpenAI plus the
//! servers that emulate it and never shipped the newer Responses API
//! (llama.cpp, Ollama, vLLM, LM Studio, DeepSeek, Groq, Together, …).
//!
//! Ref: https://platform.openai.com/docs/api-reference/chat/create
//! Streaming: https://platform.openai.com/docs/api-reference/chat-streaming
//!
//! Two dialect choices worth knowing:
//!
//! - the output cap is sent as `max_completion_tokens`. `max_tokens` is
//!   deprecated and *rejected* by reasoning models, whereas older servers
//!   merely ignore the newer name and fall back to their own default — a soft
//!   failure beats a 400.
//! - Chat Completions has no reasoning *summary* channel, so the de-facto
//!   `reasoning_content` / `reasoning` deltas are what becomes
//!   [`Content::Thinking`]. Like every other backend, thinking is kept for the
//!   session and never echoed back to the provider.
//!
//! A `tool` message may only carry text here, so images in a tool result (e.g.
//! `view_image`) ride in a user message after the round's tool replies.

use super::*;

/// Build one `POST /chat/completions` body.
pub(super) fn request_body(driver: &OpenAIGenerativeModel, input: &[Message]) -> serde_json::Value {
    let tools: Vec<ChatTool> = driver
        .tools
        .iter()
        .map(|spec| ChatTool {
            type_: "function",
            function: ChatToolFunction {
                name: spec.name.clone(),
                description: spec.description.clone(),
                parameters: spec.input_schema.clone(),
            },
        })
        .collect();
    let request = ChatCompletionsRequest {
        model: &driver.model.api_id,
        messages: convert_messages(&driver.system_prompt, input),
        tools: (!tools.is_empty()).then_some(tools),
        max_completion_tokens: driver.backend.max_output_tokens,
        reasoning_effort: reasoning_effort(driver),
        stream: true,
        // Usage is omitted from streamed responses unless asked for.
        stream_options: ChatStreamOptions {
            include_usage: true,
        },
    };
    serde_json::to_value(&request).expect("chat completions request serializes")
}

//
// Message conversion → Chat Completions `messages` list
//

fn convert_messages(system_prompt: &str, input: &[Message]) -> Vec<ChatMessage> {
    let mut out = Vec::new();

    if !system_prompt.is_empty() {
        out.push(ChatMessage {
            role: "system",
            content: Some(ChatContent::Text(system_prompt.to_string())),
            tool_calls: None,
            tool_call_id: None,
        });
    }

    for message in input {
        match message {
            Message::UserMessage { content } => {
                out.push(ChatMessage {
                    role: "user",
                    content: Some(user_content(content)),
                    tool_calls: None,
                    tool_call_id: None,
                });
            }
            Message::ToolResults { tool_use_results } => {
                // Every `tool` reply must follow its assistant turn without an
                // intervening message, so images from the whole round are
                // gathered into one user message after them.
                let mut images = Vec::new();
                for result in tool_use_results {
                    let mut text = tool_result_text(result);
                    let result_images = images_of(&result.content);
                    if text.is_empty() && !result_images.is_empty() {
                        text = "[image attached below]".into();
                    }
                    images.extend(result_images);
                    out.push(ChatMessage {
                        role: "tool",
                        content: Some(ChatContent::Text(text)),
                        tool_calls: None,
                        tool_call_id: Some(result.id.clone()),
                    });
                }
                if !images.is_empty() {
                    out.push(ChatMessage {
                        role: "user",
                        content: Some(ChatContent::Parts(
                            images
                                .into_iter()
                                .map(|source| ChatContentPart::ImageUrl {
                                    image_url: ChatImageUrl {
                                        url: image_url(source),
                                    },
                                })
                                .collect(),
                        )),
                        tool_calls: None,
                        tool_call_id: None,
                    });
                }
            }
            Message::AssistantMessage {
                content,
                tool_uses,
                turn_end_reason: _,
            } => {
                // Test the *rendered* text: a thinking-only turn has non-empty
                // `content` that renders to "", and a message with neither text
                // nor tool calls is rejected by providers on every later request.
                let text = text_of(content);
                let tool_calls: Vec<ChatToolCall> = tool_uses
                    .iter()
                    .map(|tool_use| ChatToolCall {
                        id: tool_use.id.clone(),
                        type_: "function",
                        function: ChatToolCallFunction {
                            name: tool_use.name.clone(),
                            arguments: tool_use.input.to_string(),
                        },
                    })
                    .collect();
                if text.is_empty() && tool_calls.is_empty() {
                    continue;
                }
                out.push(ChatMessage {
                    role: "assistant",
                    content: (!text.is_empty()).then_some(ChatContent::Text(text)),
                    tool_calls: (!tool_calls.is_empty()).then_some(tool_calls),
                    tool_call_id: None,
                });
            }
        }
    }

    out
}

/// User message content: plain string when text-only, `text` / `image_url`
/// parts when images are attached.
fn user_content(content: &[Content]) -> ChatContent {
    if !content.iter().any(|c| matches!(c, Content::Image { .. })) {
        return ChatContent::Text(text_of(content));
    }
    ChatContent::Parts(
        content
            .iter()
            .filter_map(|c| match c {
                Content::Text { text } => Some(ChatContentPart::Text { text: text.clone() }),
                Content::Image { source } => Some(ChatContentPart::ImageUrl {
                    image_url: ChatImageUrl {
                        url: image_url(source),
                    },
                }),
                Content::Thinking { .. } => None,
            })
            .collect(),
    )
}

//
// Stream decoding
//

/// Folds the single `delta` channel into myco's indexed content / tool-use slots.
#[derive(Default)]
pub(super) struct Decoder {
    /// Content-block indices handed out to the thinking and text blocks, in
    /// arrival order (Chat Completions streams at most one of each).
    next_content_index: usize,
    thinking_index: Option<usize>,
    text_index: Option<usize>,
    /// Wire `tool_calls[].index` → our tool-use slot.
    tool_calls: Vec<Option<ToolCallSlot>>,
    saw_tool_call: bool,
    stop_reason: Option<TurnEndReason>,
}

struct ToolCallSlot {
    index: usize,
    arguments: String,
}

impl Decoder {
    pub(super) fn handle(&mut self, data: &str) -> Result<Vec<MessagePart>, GenerateError> {
        let chunk: ChatCompletionChunk = serde_json::from_str(data).map_err(|e| {
            GenerateError::MalformedResponseError(format!(
                "Failed to parse OpenAI Chat Completions SSE event JSON: {e}; data={data}"
            ))
        })?;
        self.handle_chunk(chunk)
    }

    fn take_content_index(&mut self) -> usize {
        let index = self.next_content_index;
        self.next_content_index += 1;
        index
    }

    fn handle_chunk(
        &mut self,
        chunk: ChatCompletionChunk,
    ) -> Result<Vec<MessagePart>, GenerateError> {
        if let Some(error) = chunk.error {
            return Err(GenerateError::ExecutionError(format!(
                "OpenAI Chat Completions stream error: {error}"
            )));
        }

        let mut out = Vec::new();

        for choice in chunk.choices {
            let delta = choice.delta;

            // No summary channel in this dialect: `reasoning_content`
            // (DeepSeek / vLLM / llama.cpp) or `reasoning` (OpenRouter) is the
            // only thinking a user can see.
            if let Some(text) = delta
                .reasoning_content
                .or(delta.reasoning)
                .filter(|t| !t.is_empty())
            {
                let index = match self.thinking_index {
                    Some(index) => index,
                    None => {
                        let index = self.take_content_index();
                        self.thinking_index = Some(index);
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
            }

            if let Some(text) = delta.content.filter(|t| !t.is_empty()) {
                let index = match self.text_index {
                    Some(index) => index,
                    None => {
                        let index = self.take_content_index();
                        self.text_index = Some(index);
                        out.push(MessagePart::ContentStart(ContentStart::Text { index }));
                        index
                    }
                };
                out.push(MessagePart::ContentDelta(ContentDelta::Text {
                    index,
                    delta: text,
                }));
            }

            for (position, call) in delta.tool_calls.into_iter().enumerate() {
                out.extend(self.handle_tool_call_delta(position, call)?);
            }

            if let Some(finish_reason) = choice.finish_reason {
                let reason = match finish_reason.as_str() {
                    "tool_calls" | "function_call" => TurnEndReason::ToolUse,
                    "length" => TurnEndReason::MaxTokens,
                    "content_filter" => {
                        return Err(GenerateError::RefusalError(
                            "OpenAI Chat Completions stopped for content_filter".into(),
                        ));
                    }
                    // Some servers report "stop" even when they emitted tool calls.
                    "stop" if self.saw_tool_call => TurnEndReason::ToolUse,
                    "stop" => TurnEndReason::EndTurn,
                    other => TurnEndReason::Other(format!("OpenAICompletions::{other}")),
                };
                self.stop_reason = Some(reason.clone());
                out.push(MessagePart::TurnEndReason(reason));
            }
        }

        // With `stream_options.include_usage` the totals arrive in a final
        // choice-less chunk, after the finish_reason.
        if let Some(usage) = chunk.usage {
            out.push(MessagePart::Usage(usage.into_token_usage()));
        }

        Ok(out)
    }

    fn handle_tool_call_delta(
        &mut self,
        position: usize,
        call: ChatToolCallDelta,
    ) -> Result<Vec<MessagePart>, GenerateError> {
        // `index` identifies the call across chunks; servers that omit it never
        // interleave, so array position is an equivalent key.
        let wire_index = call.index.unwrap_or(position);
        while self.tool_calls.len() <= wire_index {
            self.tool_calls.push(None);
        }

        let mut out = Vec::new();
        if self.tool_calls[wire_index].is_none() {
            let id = call.id.unwrap_or_default();
            let name = call
                .function
                .as_ref()
                .and_then(|f| f.name.clone())
                .unwrap_or_default();
            if id.is_empty() || name.is_empty() {
                // Without both we would produce a tool result the provider
                // cannot match back to its call. Fail loud instead.
                return Err(GenerateError::MalformedResponseError(format!(
                    "OpenAI Chat Completions: tool call at index {wire_index} started \
                     without an id and name (id={id:?}, name={name:?})"
                )));
            }
            let index = self.tool_calls.iter().filter(|s| s.is_some()).count();
            self.tool_calls[wire_index] = Some(ToolCallSlot {
                index,
                arguments: String::new(),
            });
            self.saw_tool_call = true;
            out.push(MessagePart::ToolUseStart(ToolUseStart { index, id, name }));
        }

        let arguments = call.function.and_then(|f| f.arguments).unwrap_or_default();
        if !arguments.is_empty() {
            let slot = self.tool_calls[wire_index].as_mut().expect("slot opened");
            slot.arguments.push_str(&arguments);
            out.push(MessagePart::ToolUseDelta(ToolUseDelta {
                index: slot.index,
                input_json_delta: arguments,
            }));
        }

        Ok(out)
    }

    pub(super) fn finish(self) -> Result<(), GenerateError> {
        if self.stop_reason.is_none() {
            return Err(GenerateError::MalformedResponseError(
                "OpenAI Chat Completions stream ended without a finish_reason".into(),
            ));
        }

        for (i, slot) in self.tool_calls.into_iter().enumerate() {
            if let Some(slot) = slot {
                let json = if slot.arguments.is_empty() {
                    "{}"
                } else {
                    slot.arguments.as_str()
                };
                if let Err(e) = serde_json::from_str::<serde_json::Value>(json) {
                    return Err(GenerateError::MalformedResponseError(format!(
                        "Malformed stream: tool call arguments at index {i} invalid: {e}"
                    )));
                }
            }
        }

        Ok(())
    }
}

//
// Wire types
//

#[derive(Debug, serde::Serialize)]
struct ChatCompletionsRequest<'a> {
    model: &'a str,
    messages: Vec<ChatMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<ChatTool>>,
    /// The current output cap field; `max_tokens` is deprecated and rejected
    /// by reasoning models.
    #[serde(skip_serializing_if = "Option::is_none")]
    max_completion_tokens: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_effort: Option<&'static str>,
    stream: bool,
    stream_options: ChatStreamOptions,
}

#[derive(Debug, serde::Serialize)]
struct ChatStreamOptions {
    include_usage: bool,
}

#[derive(Debug, Clone, serde::Serialize)]
struct ChatTool {
    #[serde(rename = "type")]
    type_: &'static str,
    function: ChatToolFunction,
}

#[derive(Debug, Clone, serde::Serialize)]
struct ChatToolFunction {
    name: String,
    description: String,
    parameters: serde_json::Value,
}

#[derive(Debug, Clone, serde::Serialize, PartialEq, Eq)]
struct ChatMessage {
    role: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<ChatContent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<ChatToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
}

/// Message `content`: the plain-string form for text-only messages, or the
/// part-list form when a user message carries images.
#[derive(Debug, Clone, serde::Serialize, PartialEq, Eq)]
#[serde(untagged)]
enum ChatContent {
    Text(String),
    Parts(Vec<ChatContentPart>),
}

#[derive(Debug, Clone, serde::Serialize, PartialEq, Eq)]
#[serde(tag = "type")]
enum ChatContentPart {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "image_url")]
    ImageUrl { image_url: ChatImageUrl },
}

#[derive(Debug, Clone, serde::Serialize, PartialEq, Eq)]
struct ChatImageUrl {
    url: String,
}

#[derive(Debug, Clone, serde::Serialize, PartialEq, Eq)]
struct ChatToolCall {
    id: String,
    #[serde(rename = "type")]
    type_: &'static str,
    function: ChatToolCallFunction,
}

#[derive(Debug, Clone, serde::Serialize, PartialEq, Eq)]
struct ChatToolCallFunction {
    name: String,
    arguments: String,
}

#[derive(Debug, serde::Deserialize)]
struct ChatCompletionChunk {
    #[serde(default)]
    choices: Vec<ChatChoice>,
    #[serde(default)]
    usage: Option<ChatUsage>,
    /// Some gateways report mid-stream failures as a 200 carrying this.
    #[serde(default)]
    error: Option<serde_json::Value>,
}

#[derive(Debug, serde::Deserialize)]
struct ChatChoice {
    #[serde(default)]
    delta: ChatDelta,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Debug, Default, serde::Deserialize)]
struct ChatDelta {
    #[serde(default)]
    content: Option<String>,
    /// Reasoning text (DeepSeek, vLLM, llama.cpp, …).
    #[serde(default)]
    reasoning_content: Option<String>,
    /// Reasoning text under OpenRouter's spelling.
    #[serde(default)]
    reasoning: Option<String>,
    #[serde(default)]
    tool_calls: Vec<ChatToolCallDelta>,
}

#[derive(Debug, serde::Deserialize)]
struct ChatToolCallDelta {
    #[serde(default)]
    index: Option<usize>,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    function: Option<ChatToolCallFunctionDelta>,
}

#[derive(Debug, serde::Deserialize)]
struct ChatToolCallFunctionDelta {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

#[derive(Debug, Clone, serde::Deserialize)]
struct ChatUsage {
    #[serde(default)]
    prompt_tokens: u64,
    #[serde(default)]
    completion_tokens: u64,
    #[serde(default)]
    prompt_tokens_details: Option<ChatPromptTokensDetails>,
}

#[derive(Debug, Clone, serde::Deserialize)]
struct ChatPromptTokensDetails {
    #[serde(default)]
    cached_tokens: Option<u64>,
}

impl ChatUsage {
    fn into_token_usage(self) -> TokenUsage {
        // prompt_tokens is already the full prompt; cached_tokens is a subset.
        TokenUsage {
            input_tokens: self.prompt_tokens,
            output_tokens: self.completion_tokens,
            cached_input_tokens: self
                .prompt_tokens_details
                .and_then(|d| d.cached_tokens)
                .unwrap_or(0),
            cached_output_tokens: 0,
        }
    }
}

//
// Tests
//

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk(json: serde_json::Value) -> ChatCompletionChunk {
        serde_json::from_value(json).expect("chunk")
    }

    #[test]
    fn usage_reports_cached_input_as_subset() {
        let usage = ChatUsage {
            prompt_tokens: 10_000,
            completion_tokens: 200,
            prompt_tokens_details: Some(ChatPromptTokensDetails {
                cached_tokens: Some(8_000),
            }),
        }
        .into_token_usage();
        assert_eq!(usage.input_tokens, 10_000);
        assert_eq!(usage.output_tokens, 200);
        assert_eq!(usage.cached_input_tokens, 8_000);
        assert_eq!(usage.context_tokens(), 10_000);
    }

    #[test]
    fn system_prompt_leads_the_message_list() {
        let messages = convert_messages(
            "be helpful",
            &[Message::UserMessage {
                content: vec![Content::Text { text: "hi".into() }],
            }],
        );
        let json = serde_json::to_value(&messages).unwrap();
        assert_eq!(json[0]["role"], "system");
        assert_eq!(json[0]["content"], "be helpful");
        assert_eq!(json[1]["role"], "user");
        assert_eq!(json[1]["content"], "hi");
    }

    #[test]
    fn convert_tool_use_and_tool_result() {
        let input = [
            Message::AssistantMessage {
                content: vec![Content::Text {
                    text: "checking".into(),
                }],
                tool_uses: vec![ToolUse {
                    id: "call_1".into(),
                    name: "bash".into(),
                    input: serde_json::json!({"command": "echo hi"}),
                }],
                turn_end_reason: Some(TurnEndReason::ToolUse),
            },
            Message::ToolResults {
                tool_use_results: vec![ToolResult {
                    id: "call_1".into(),
                    content: vec![Content::Text {
                        text: "hi\n".into(),
                    }],
                    is_error: false,
                }],
            },
        ];
        let json = serde_json::to_value(convert_messages("", &input)).unwrap();
        assert_eq!(json[0]["role"], "assistant");
        assert_eq!(json[0]["content"], "checking");
        assert_eq!(json[0]["tool_calls"][0]["id"], "call_1");
        assert_eq!(json[0]["tool_calls"][0]["type"], "function");
        assert_eq!(json[0]["tool_calls"][0]["function"]["name"], "bash");
        assert_eq!(
            json[0]["tool_calls"][0]["function"]["arguments"],
            r#"{"command":"echo hi"}"#
        );
        assert_eq!(json[1]["role"], "tool");
        assert_eq!(json[1]["tool_call_id"], "call_1");
        assert_eq!(json[1]["content"], "hi\n");
    }

    #[test]
    fn tool_only_assistant_message_omits_content() {
        let input = [Message::AssistantMessage {
            content: vec![Content::Thinking {
                text: "hmm".into(),
                signature: None,
                redacted: false,
            }],
            tool_uses: vec![ToolUse {
                id: "call_1".into(),
                name: "bash".into(),
                input: serde_json::json!({}),
            }],
            turn_end_reason: Some(TurnEndReason::ToolUse),
        }];
        let json = serde_json::to_value(convert_messages("", &input)).unwrap();
        assert!(json[0].get("content").is_none(), "{json}");
        assert_eq!(json[0]["tool_calls"][0]["id"], "call_1");
    }

    #[test]
    fn empty_assistant_message_is_dropped() {
        let input = [Message::AssistantMessage {
            content: vec![Content::Thinking {
                text: "hmm".into(),
                signature: None,
                redacted: false,
            }],
            tool_uses: vec![],
            turn_end_reason: Some(TurnEndReason::EndTurn),
        }];
        assert!(convert_messages("", &input).is_empty());
    }

    #[test]
    fn tool_result_images_follow_as_a_user_message() {
        let input = [Message::ToolResults {
            tool_use_results: vec![
                ToolResult {
                    id: "call_1".into(),
                    content: vec![Content::Image {
                        source: "data:image/png;base64,AAAA".into(),
                    }],
                    is_error: false,
                },
                ToolResult {
                    id: "call_2".into(),
                    content: vec![Content::Text { text: "ok".into() }],
                    is_error: false,
                },
            ],
        }];
        let json = serde_json::to_value(convert_messages("", &input)).unwrap();
        // Both tool replies stay adjacent to their call, images after them.
        assert_eq!(json[0]["role"], "tool");
        assert_eq!(json[0]["tool_call_id"], "call_1");
        assert_eq!(json[0]["content"], "[image attached below]");
        assert_eq!(json[1]["role"], "tool");
        assert_eq!(json[1]["content"], "ok");
        assert_eq!(json[2]["role"], "user");
        assert_eq!(json[2]["content"][0]["type"], "image_url");
        assert_eq!(
            json[2]["content"][0]["image_url"]["url"],
            "data:image/png;base64,AAAA"
        );
        assert_eq!(json[2]["content"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn user_image_becomes_image_url_part() {
        let input = [Message::UserMessage {
            content: vec![
                Content::Image {
                    source: "iVBOR".into(),
                },
                Content::Text {
                    text: "what is this?".into(),
                },
            ],
        }];
        let json = serde_json::to_value(convert_messages("", &input)).unwrap();
        assert_eq!(json[0]["content"][0]["type"], "image_url");
        assert_eq!(
            json[0]["content"][0]["image_url"]["url"],
            "data:image/png;base64,iVBOR"
        );
        assert_eq!(json[0]["content"][1]["type"], "text");
        assert_eq!(json[0]["content"][1]["text"], "what is this?");
    }

    #[test]
    fn request_asks_for_usage_and_the_modern_token_cap() {
        let driver = super::super::tests::driver(Protocol::OpenAICompletions);
        let json = request_body(&driver, &[]);
        assert_eq!(json["model"], "test-model");
        assert_eq!(json["stream"], true);
        assert_eq!(json["max_completion_tokens"], 128);
        assert!(json.get("max_tokens").is_none(), "{json}");
        assert_eq!(json["reasoning_effort"], "high");
        assert_eq!(json["stream_options"]["include_usage"], true);
    }

    #[test]
    fn stream_accumulator_reasoning_then_text() {
        let mut acc = Decoder::default();

        let items = acc
            .handle_chunk(chunk(serde_json::json!({
                "choices": [{"delta": {"reasoning_content": "step one"}}]
            })))
            .unwrap();
        assert!(matches!(
            items[0],
            MessagePart::ContentStart(ContentStart::Thinking { index: 0, .. })
        ));
        assert!(matches!(
            &items[1],
            MessagePart::ContentDelta(ContentDelta::Thinking { index: 0, delta }) if delta == "step one"
        ));

        let items = acc
            .handle_chunk(chunk(serde_json::json!({
                "choices": [{"delta": {"content": "Paris"}}]
            })))
            .unwrap();
        assert!(matches!(
            items[0],
            MessagePart::ContentStart(ContentStart::Text { index: 1 })
        ));
        assert!(matches!(
            &items[1],
            MessagePart::ContentDelta(ContentDelta::Text { index: 1, delta }) if delta == "Paris"
        ));

        // Second text delta reuses the open block.
        let items = acc
            .handle_chunk(chunk(serde_json::json!({
                "choices": [{"delta": {"content": "!"}, "finish_reason": "stop"}],
                "usage": {"prompt_tokens": 12, "completion_tokens": 3}
            })))
            .unwrap();
        assert!(matches!(
            items[0],
            MessagePart::ContentDelta(ContentDelta::Text { index: 1, .. })
        ));
        assert!(matches!(
            items[1],
            MessagePart::TurnEndReason(TurnEndReason::EndTurn)
        ));
        match items[2] {
            MessagePart::Usage(usage) => assert_eq!(usage.input_tokens, 12),
            ref other => panic!("expected usage, got {other:?}"),
        }
        acc.finish().unwrap();
    }

    #[test]
    fn stream_accumulator_openrouter_reasoning_spelling() {
        let mut acc = Decoder::default();
        let items = acc
            .handle_chunk(chunk(serde_json::json!({
                "choices": [{"delta": {"reasoning": "thinking hard"}}]
            })))
            .unwrap();
        assert!(matches!(
            &items[1],
            MessagePart::ContentDelta(ContentDelta::Thinking { delta, .. }) if delta == "thinking hard"
        ));
    }

    #[test]
    fn stream_accumulator_streams_tool_call_across_chunks() {
        let mut acc = Decoder::default();

        let items = acc
            .handle_chunk(chunk(serde_json::json!({
                "choices": [{"delta": {"tool_calls": [{
                    "index": 0,
                    "id": "call_1",
                    "type": "function",
                    "function": {"name": "get_weather", "arguments": ""}
                }]}}]
            })))
            .unwrap();
        match &items[0] {
            MessagePart::ToolUseStart(ToolUseStart { index, id, name }) => {
                assert_eq!(*index, 0);
                assert_eq!(id, "call_1");
                assert_eq!(name, "get_weather");
            }
            other => panic!("expected tool use start, got {other:?}"),
        }
        assert_eq!(items.len(), 1, "empty arguments must not emit a delta");

        for fragment in [r#"{"city""#, r#":"SF"}"#] {
            let items = acc
                .handle_chunk(chunk(serde_json::json!({
                    "choices": [{"delta": {"tool_calls": [{
                        "index": 0,
                        "function": {"arguments": fragment}
                    }]}}]
                })))
                .unwrap();
            assert!(matches!(
                &items[0],
                MessagePart::ToolUseDelta(ToolUseDelta { index: 0, input_json_delta })
                    if input_json_delta == fragment
            ));
        }

        let items = acc
            .handle_chunk(chunk(serde_json::json!({
                "choices": [{"delta": {}, "finish_reason": "tool_calls"}]
            })))
            .unwrap();
        assert!(matches!(
            items[0],
            MessagePart::TurnEndReason(TurnEndReason::ToolUse)
        ));
        acc.finish().unwrap();
    }

    #[test]
    fn parallel_tool_calls_get_distinct_indices() {
        let mut acc = Decoder::default();
        let items = acc
            .handle_chunk(chunk(serde_json::json!({
                "choices": [{"delta": {"tool_calls": [
                    {"index": 0, "id": "call_1", "function": {"name": "bash", "arguments": "{}"}},
                    {"index": 1, "id": "call_2", "function": {"name": "manual", "arguments": "{}"}}
                ]}}]
            })))
            .unwrap();
        let starts: Vec<_> = items
            .iter()
            .filter_map(|p| match p {
                MessagePart::ToolUseStart(start) => Some((start.index, start.id.as_str())),
                _ => None,
            })
            .collect();
        assert_eq!(starts, [(0, "call_1"), (1, "call_2")]);
    }

    #[test]
    fn stop_after_tool_calls_is_tool_use() {
        let mut acc = Decoder::default();
        acc.handle_chunk(chunk(serde_json::json!({
            "choices": [{"delta": {"tool_calls": [
                {"index": 0, "id": "call_1", "function": {"name": "bash", "arguments": "{}"}}
            ]}}]
        })))
        .unwrap();
        let items = acc
            .handle_chunk(chunk(serde_json::json!({
                "choices": [{"delta": {}, "finish_reason": "stop"}]
            })))
            .unwrap();
        assert!(matches!(
            items[0],
            MessagePart::TurnEndReason(TurnEndReason::ToolUse)
        ));
    }

    #[test]
    fn finish_reason_length_is_max_tokens_and_filter_is_refusal() {
        let mut acc = Decoder::default();
        let items = acc
            .handle_chunk(chunk(serde_json::json!({
                "choices": [{"delta": {}, "finish_reason": "length"}]
            })))
            .unwrap();
        assert!(matches!(
            items[0],
            MessagePart::TurnEndReason(TurnEndReason::MaxTokens)
        ));

        let mut acc = Decoder::default();
        let err = acc
            .handle_chunk(chunk(serde_json::json!({
                "choices": [{"delta": {}, "finish_reason": "content_filter"}]
            })))
            .unwrap_err();
        assert!(matches!(err, GenerateError::RefusalError(_)), "{err:?}");
    }

    #[test]
    fn tool_call_without_id_or_name_is_malformed() {
        let mut acc = Decoder::default();
        let err = acc
            .handle_chunk(chunk(serde_json::json!({
                "choices": [{"delta": {"tool_calls": [
                    {"index": 0, "function": {"arguments": "{\"x\":1}"}}
                ]}}]
            })))
            .unwrap_err();
        match err {
            GenerateError::MalformedResponseError(msg) => {
                assert!(msg.contains("without an id and name"), "{msg}");
            }
            other => panic!("expected MalformedResponseError, got {other:?}"),
        }
    }

    #[test]
    fn stream_error_payload_is_an_execution_error() {
        let mut acc = Decoder::default();
        let err = acc
            .handle_chunk(chunk(serde_json::json!({
                "error": {"message": "upstream is down", "code": 502}
            })))
            .unwrap_err();
        match err {
            GenerateError::ExecutionError(msg) => {
                assert!(msg.contains("upstream is down"), "{msg}")
            }
            other => panic!("expected ExecutionError, got {other:?}"),
        }
    }

    #[test]
    fn stream_without_finish_reason_is_malformed() {
        let mut acc = Decoder::default();
        acc.handle_chunk(chunk(serde_json::json!({
            "choices": [{"delta": {"content": "truncated"}}]
        })))
        .unwrap();
        let err = acc.finish().unwrap_err();
        assert!(matches!(err, GenerateError::MalformedResponseError(_)));
    }

    #[test]
    fn invalid_tool_arguments_fail_at_finish() {
        let mut acc = Decoder::default();
        acc.handle_chunk(chunk(serde_json::json!({
            "choices": [{"delta": {"tool_calls": [
                {"index": 0, "id": "call_1", "function": {"name": "bash", "arguments": "{\"a\":"}}
            ]}, "finish_reason": "tool_calls"}]
        })))
        .unwrap();
        let err = acc.finish().unwrap_err();
        match err {
            GenerateError::MalformedResponseError(msg) => {
                assert!(msg.contains("arguments at index 0"), "{msg}");
            }
            other => panic!("expected MalformedResponseError, got {other:?}"),
        }
    }
}
