//! OpenAI Responses API backend (also used by xAI / Grok and OpenRouter gateways).
//!
//! Ref: https://platform.openai.com/docs/api-reference/responses
//! Streaming: https://platform.openai.com/docs/guides/streaming-responses?api-mode=responses
//! xAI: https://docs.x.ai/docs/guides/function-calling
//! OpenRouter: https://openrouter.ai/docs/api/api-reference/responses/create-responses

use std::sync::Arc;

use crate::core::*;

use super::driver_core::{Slot, SlotMap, SseAccumulator};
use super::openai_common::{
    OpenAIBackendConfig, OpenAIUsage, image_url, images_of, reasoning_effort, text_of,
    tool_result_text, user_content_parts,
};
use super::*;

/// Stateless OpenAI Responses driver. Conversation history is owned by the caller.
pub struct OpenAIResponsesGenerativeModel {
    model: ModelSpec,
    system_prompt: String,
    tools: Vec<ResponsesTool>,
    backend: OpenAIBackendConfig,
    client: reqwest::Client,
}

impl OpenAIResponsesGenerativeModel {
    pub fn new(
        config: GenerativeModelConfig,
        backend: OpenAIBackendConfig,
    ) -> Result<Arc<Self>, ModelCreationError> {
        let auth = (!backend.auth_token.is_empty())
            .then(|| ("authorization", format!("Bearer {}", backend.auth_token)));
        let client = driver_core::build_client(auth, &[])?;

        let tools = config
            .tools
            .into_iter()
            .map(|spec| ResponsesTool {
                type_: "function".into(),
                name: spec.name,
                description: spec.description,
                parameters: spec.input_schema,
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

    /// Build (without sending) the streaming Responses request.
    fn response_request(&self, input: &[ResponsesInputItem]) -> reqwest::RequestBuilder {
        let reasoning =
            reasoning_effort(&self.model, &self.backend).map(|effort| ResponsesReasoningConfig {
                effort: Some(effort),
            });
        let request = ResponsesCreateRequest {
            model: &self.model.api_id,
            input,
            instructions: if self.system_prompt.is_empty() {
                None
            } else {
                Some(self.system_prompt.as_str())
            },
            tools: if self.tools.is_empty() {
                None
            } else {
                Some(&self.tools)
            },
            max_output_tokens: self.backend.max_output_tokens,
            // Stateless client: full history is resent every turn. Do not retain
            // conversations server-side (OpenAI default is store=true).
            store: false,
            stream: true,
            reasoning,
        };

        let base = self.backend.base_url.trim_end_matches('/');
        self.client.post(format!("{base}/responses")).json(&request)
    }
}

impl GenerativeModel for OpenAIResponsesGenerativeModel {
    fn generate(&self, input: &[Message]) -> AsyncStream<Result<MessagePart, GenerateError>> {
        let input_items = match convert_messages(input) {
            Ok(items) => items,
            Err(e) => return driver_core::error_stream(e),
        };
        driver_core::spawn_generate(
            self.response_request(&input_items),
            StreamAccumulator::default(),
            "OpenAI Responses",
            self.backend.debug_dump_api_requests,
        )
    }
}

//
// Message conversion → Responses `input` list
//

fn convert_messages(input: &[Message]) -> Result<Vec<ResponsesInputItem>, GenerateError> {
    // Minted per-position ids go on the wire, never stored provider ids
    // (see [`wire_tool_ids`]).
    let wire_ids = wire_tool_ids(input)?;
    let mut out = Vec::new();

    for (i, message) in input.iter().enumerate() {
        match message {
            Message::UserMessage { content } => {
                out.push(ResponsesInputItem::Message {
                    role: "user".into(),
                    content: user_content_to_input(content),
                });
            }
            Message::ToolResults { tool_use_results } => {
                for (j, result) in tool_use_results.iter().enumerate() {
                    out.push(ResponsesInputItem::FunctionCallOutput {
                        type_: "function_call_output",
                        call_id: wire_ids[i][j].clone(),
                        output: tool_result_to_output(result),
                    });
                }
            }
            Message::AssistantMessage {
                content,
                tool_uses,
                turn_end_reason: _,
            } => {
                // Test the *rendered* text: a thinking-only turn has non-empty
                // `content` that renders to "", and an empty assistant item is
                // rejected by providers on every later request.
                let text = text_of(content);
                if !text.is_empty() {
                    out.push(ResponsesInputItem::Message {
                        role: "assistant".into(),
                        content: ResponsesMessageContent::Text(text),
                    });
                }
                for (j, tool_use) in tool_uses.iter().enumerate() {
                    out.push(ResponsesInputItem::FunctionCall {
                        type_: "function_call",
                        call_id: wire_ids[i][j].clone(),
                        name: tool_use.name.clone(),
                        arguments: tool_use.input.to_string(),
                    });
                }
            }
        }
    }

    Ok(out)
}

/// User message content: plain string when text-only, `input_text` /
/// `input_image` parts when images are attached.
fn user_content_to_input(content: &[Content]) -> ResponsesMessageContent {
    let parts = user_content_parts(
        content,
        |text| ResponsesContentPart::InputText { text: text.into() },
        |source| ResponsesContentPart::InputImage {
            image_url: image_url(source),
        },
    );
    match parts {
        Some(parts) => ResponsesMessageContent::Parts(parts),
        None => ResponsesMessageContent::Text(text_of(content)),
    }
}

/// `function_call_output.output`: the plain-string form for text-only
/// results, or `input_text` / `input_image` parts when a tool result carries
/// images (e.g. `view_image`). Text-only stays a string for compatibility
/// with OpenAI-compatible gateways that predate content parts in function
/// outputs.
fn tool_result_to_output(result: &ToolResult) -> ResponsesFunctionOutput {
    let text = tool_result_text(result);
    let images = images_of(&result.content);
    if images.is_empty() {
        return ResponsesFunctionOutput::Text(text);
    }

    let mut parts = Vec::new();
    if !text.is_empty() {
        parts.push(ResponsesContentPart::InputText { text });
    }
    parts.extend(
        images
            .into_iter()
            .map(|source| ResponsesContentPart::InputImage {
                image_url: image_url(source),
            }),
    );
    ResponsesFunctionOutput::Parts(parts)
}

//
// Stream accumulation (the SSE drive loop is shared, in driver_core)
//

/// Maps Responses `output_index` slots onto separate content / tool-use index spaces.
#[derive(Default)]
struct StreamAccumulator {
    slots: SlotMap,
    /// Streamed function-call arguments per `output_index`, so a
    /// `function_call_arguments.done` with final arguments can be preferred
    /// when no deltas were streamed.
    tool_input_json: Vec<Option<String>>,
    saw_tool_call: bool,
    stop_reason: Option<TurnEndReason>,
    finished: bool,
}

impl SseAccumulator for StreamAccumulator {
    fn handle_data(&mut self, data: &str) -> Result<Vec<MessagePart>, GenerateError> {
        let event: ResponsesStreamEvent = serde_json::from_str(data).map_err(|e| {
            GenerateError::MalformedResponseError(format!(
                "Failed to parse OpenAI Responses SSE event JSON: {e}; data={data}"
            ))
        })?;
        self.handle_event(event)
    }

    fn finished(&self) -> bool {
        self.finished
    }

    fn finish(self) -> Result<(), GenerateError> {
        driver_core::validate_finish(
            "OpenAI Responses",
            self.stop_reason.is_some(),
            self.tool_input_json
                .iter()
                .enumerate()
                .filter_map(|(i, json)| json.as_deref().map(|j| (i, j))),
        )
    }
}

impl StreamAccumulator {
    fn set_tool_json(&mut self, output_index: usize, json: String) {
        while self.tool_input_json.len() <= output_index {
            self.tool_input_json.push(None);
        }
        self.tool_input_json[output_index] = Some(json);
    }

    fn handle_event(
        &mut self,
        event: ResponsesStreamEvent,
    ) -> Result<Vec<MessagePart>, GenerateError> {
        let mut out = Vec::new();

        match event {
            ResponsesStreamEvent::ResponseCreated | ResponsesStreamEvent::ResponseInProgress => {}
            ResponsesStreamEvent::ResponseOutputItemAdded { output_index, item } => match item {
                ResponsesOutputItem::Message => {
                    let content_index = self.slots.open_content(output_index);
                    out.push(MessagePart::ContentStart(ContentStart::Text {
                        index: content_index,
                    }));
                }
                ResponsesOutputItem::Reasoning => {
                    let content_index = self.slots.open_thinking(output_index);
                    out.push(MessagePart::ContentStart(ContentStart::Thinking {
                        index: content_index,
                        signature: None,
                        redacted: false,
                    }));
                }
                ResponsesOutputItem::FunctionCall {
                    call_id,
                    name,
                    arguments,
                } => {
                    // An id-less or nameless call is poison: it would flow into
                    // persisted history and 400 on every later request (resume
                    // included). Fail loud like the arguments-without-item path.
                    let (Some(call_id), Some(name)) = (call_id, name) else {
                        return Err(GenerateError::MalformedResponseError(format!(
                            "OpenAI Responses: function_call output_item.added for \
                             output_index={output_index} is missing call_id or name"
                        )));
                    };
                    let tool_index = self.slots.open_tool_use(output_index);
                    self.set_tool_json(output_index, arguments.unwrap_or_default());
                    self.saw_tool_call = true;
                    out.push(MessagePart::ToolUseStart(ToolUseStart {
                        index: tool_index,
                        id: call_id,
                        name,
                    }));
                }
                ResponsesOutputItem::Other => {
                    self.slots.ignore(output_index);
                }
            },
            ResponsesStreamEvent::ResponseOutputTextDelta {
                output_index,
                delta,
            } => {
                // Lazy-open a content slot if the gateway skips output_item.added for text.
                if self.slots.get(output_index).is_none() {
                    let content_index = self.slots.open_content(output_index);
                    out.push(MessagePart::ContentStart(ContentStart::Text {
                        index: content_index,
                    }));
                }
                if let Some(Slot::Content { index }) = self.slots.get(output_index) {
                    out.push(MessagePart::ContentDelta(ContentDelta::Text {
                        index,
                        delta,
                    }));
                }
            }
            // Summary-only thinking API: surface reasoning *summaries*, ignore raw
            // chain-of-thought streams (e.g. Grok `reasoning_text` can be very long).
            ResponsesStreamEvent::ResponseReasoningSummaryTextDelta {
                output_index,
                delta,
            } => {
                // Lazy-open a thinking slot if the gateway streams deltas without item.added.
                if matches!(self.slots.get(output_index), None | Some(Slot::Ignored)) {
                    let content_index = self.slots.open_thinking(output_index);
                    out.push(MessagePart::ContentStart(ContentStart::Thinking {
                        index: content_index,
                        signature: None,
                        redacted: false,
                    }));
                }
                match self.slots.get(output_index) {
                    Some(Slot::Thinking { index }) if !delta.is_empty() => {
                        out.push(MessagePart::ContentDelta(ContentDelta::Thinking {
                            index,
                            delta,
                        }));
                    }
                    // If this index was already opened as normal text, don't corrupt it.
                    _ => {}
                }
            }
            // Raw reasoning traces are intentionally dropped (summary-only public API).
            ResponsesStreamEvent::ResponseReasoningTextDelta => {}
            ResponsesStreamEvent::ResponseFunctionCallArgumentsDelta {
                output_index,
                delta,
            } => {
                match self.slots.get(output_index) {
                    Some(Slot::ToolUse { index }) => {
                        if let Some(Some(acc)) = self.tool_input_json.get_mut(output_index) {
                            acc.push_str(&delta);
                        }
                        out.push(MessagePart::ToolUseDelta(ToolUseDelta {
                            index,
                            input_json_delta: delta,
                        }));
                    }
                    _ => {
                        // Without a prior output_item.added we have no call_id/name, so
                        // inventing an empty ToolUseStart would produce undeliverable
                        // tool results. Fail loud instead.
                        return Err(GenerateError::MalformedResponseError(format!(
                            "OpenAI Responses: function_call_arguments.delta for \
                             output_index={output_index} arrived without a prior \
                             output_item.added function_call (id/name unknown)"
                        )));
                    }
                }
            }
            ResponsesStreamEvent::ResponseFunctionCallArgumentsDone {
                output_index,
                arguments,
            } => {
                if let Some(arguments) = arguments {
                    // Prefer the final assembled arguments when the gateway provides them.
                    if let Some(Slot::ToolUse { index }) = self.slots.get(output_index) {
                        // Emit a full replace only if we never streamed deltas.
                        let prior = self
                            .tool_input_json
                            .get(output_index)
                            .and_then(|j| j.as_ref())
                            .map(|s| s.as_str())
                            .unwrap_or("");
                        if prior.is_empty() {
                            out.push(MessagePart::ToolUseDelta(ToolUseDelta {
                                index,
                                input_json_delta: arguments.clone(),
                            }));
                        }
                    }
                    self.set_tool_json(output_index, arguments);
                }
            }
            ResponsesStreamEvent::ResponseCompleted { response } => {
                let reason = match response.status.as_deref() {
                    Some("failed") => {
                        return Err(GenerateError::ExecutionError(format!(
                            "OpenAI Responses failed: {:?}",
                            response.error
                        )));
                    }
                    Some("incomplete") => {
                        let incomplete = response
                            .incomplete_details
                            .as_ref()
                            .and_then(|d| d.reason.as_deref());
                        if incomplete == Some("max_output_tokens")
                            || incomplete == Some("max_tokens")
                        {
                            TurnEndReason::MaxTokens
                        } else if incomplete == Some("content_filter") {
                            return Err(GenerateError::RefusalError(
                                "OpenAI Responses stopped for content_filter".into(),
                            ));
                        } else if self.saw_tool_call {
                            TurnEndReason::ToolUse
                        } else {
                            TurnEndReason::Other("OpenAIResponses::incomplete".into())
                        }
                    }
                    _ => {
                        if self.saw_tool_call {
                            TurnEndReason::ToolUse
                        } else {
                            TurnEndReason::EndTurn
                        }
                    }
                };
                self.stop_reason = Some(reason.clone());
                if let Some(u) = response.usage {
                    out.push(MessagePart::Usage(u.into_token_usage()));
                }
                out.push(MessagePart::TurnEndReason(reason));
                self.finished = true;
            }
            ResponsesStreamEvent::ResponseFailed { response } => {
                return Err(GenerateError::ExecutionError(format!(
                    "OpenAI Responses failed: {:?}",
                    response.error
                )));
            }
            ResponsesStreamEvent::Error { error, message } => {
                return Err(GenerateError::ExecutionError(format!(
                    "OpenAI Responses stream error: {error:?} {message:?}"
                )));
            }
            ResponsesStreamEvent::Other => {}
        }

        Ok(out)
    }
}

//
// Wire types
//

#[derive(Debug, serde::Serialize)]
struct ResponsesCreateRequest<'a> {
    model: &'a str,
    input: &'a [ResponsesInputItem],
    #[serde(skip_serializing_if = "Option::is_none")]
    instructions: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<&'a [ResponsesTool]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_output_tokens: Option<usize>,
    /// When false, the API does not persist the response server-side.
    store: bool,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning: Option<ResponsesReasoningConfig>,
}

#[derive(Debug, serde::Serialize)]
struct ResponsesReasoningConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    effort: Option<&'static str>,
}

#[derive(Debug, Clone, serde::Serialize)]
struct ResponsesTool {
    #[serde(rename = "type")]
    type_: String,
    name: String,
    description: String,
    parameters: serde_json::Value,
}

#[derive(Debug, Clone, serde::Serialize, PartialEq, Eq)]
#[serde(untagged)]
enum ResponsesInputItem {
    Message {
        role: String,
        content: ResponsesMessageContent,
    },
    FunctionCall {
        #[serde(rename = "type")]
        type_: &'static str,
        call_id: String,
        name: String,
        arguments: String,
    },
    FunctionCallOutput {
        #[serde(rename = "type")]
        type_: &'static str,
        call_id: String,
        output: ResponsesFunctionOutput,
    },
}

/// Wire form of `function_call_output.output`: a string, or a content-part
/// list when the result includes images (see [`tool_result_to_output`]).
#[derive(Debug, Clone, serde::Serialize, PartialEq, Eq)]
#[serde(untagged)]
enum ResponsesFunctionOutput {
    Text(String),
    Parts(Vec<ResponsesContentPart>),
}

/// Message `content`: the plain-string form for text-only messages, or the
/// part-list form when a user message carries images.
#[derive(Debug, Clone, serde::Serialize, PartialEq, Eq)]
#[serde(untagged)]
enum ResponsesMessageContent {
    Text(String),
    Parts(Vec<ResponsesContentPart>),
}

#[derive(Debug, Clone, serde::Serialize, PartialEq, Eq)]
#[serde(tag = "type")]
enum ResponsesContentPart {
    #[serde(rename = "input_text")]
    InputText { text: String },
    #[serde(rename = "input_image")]
    InputImage { image_url: String },
}

#[derive(Debug, serde::Deserialize)]
#[serde(tag = "type")]
enum ResponsesStreamEvent {
    #[serde(rename = "response.created")]
    ResponseCreated,
    #[serde(rename = "response.in_progress")]
    ResponseInProgress,
    #[serde(rename = "response.output_item.added")]
    ResponseOutputItemAdded {
        #[serde(default)]
        output_index: usize,
        item: ResponsesOutputItem,
    },
    #[serde(rename = "response.output_text.delta")]
    ResponseOutputTextDelta {
        #[serde(default)]
        output_index: usize,
        delta: String,
    },
    /// OpenAI reasoning summary stream (when reasoning is enabled).
    #[serde(rename = "response.reasoning_summary_text.delta")]
    ResponseReasoningSummaryTextDelta {
        #[serde(default)]
        output_index: usize,
        delta: String,
    },
    /// Some gateways stream raw reasoning text under this event name.
    /// Intentionally ignored (summary-only public thinking API).
    #[serde(rename = "response.reasoning_text.delta")]
    ResponseReasoningTextDelta,
    #[serde(rename = "response.function_call_arguments.delta")]
    ResponseFunctionCallArgumentsDelta {
        #[serde(default)]
        output_index: usize,
        delta: String,
    },
    #[serde(rename = "response.function_call_arguments.done")]
    ResponseFunctionCallArgumentsDone {
        #[serde(default)]
        output_index: usize,
        #[serde(default)]
        arguments: Option<String>,
    },
    #[serde(rename = "response.completed")]
    ResponseCompleted { response: ResponsesCompletedBody },
    #[serde(rename = "response.failed")]
    ResponseFailed { response: ResponsesCompletedBody },
    #[serde(rename = "error")]
    Error {
        #[serde(default)]
        error: Option<serde_json::Value>,
        #[serde(default)]
        message: Option<String>,
    },
    #[serde(other)]
    Other,
}

#[derive(Debug, serde::Deserialize)]
#[serde(tag = "type")]
enum ResponsesOutputItem {
    #[serde(rename = "message")]
    Message,
    #[serde(rename = "reasoning")]
    Reasoning,
    #[serde(rename = "function_call")]
    FunctionCall {
        #[serde(default)]
        call_id: Option<String>,
        #[serde(default)]
        name: Option<String>,
        #[serde(default)]
        arguments: Option<String>,
    },
    #[serde(other)]
    Other,
}

#[derive(Debug, serde::Deserialize)]
struct ResponsesCompletedBody {
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    incomplete_details: Option<ResponsesIncompleteDetails>,
    #[serde(default)]
    error: Option<serde_json::Value>,
    #[serde(default)]
    usage: Option<OpenAIUsage>,
}

#[derive(Debug, serde::Deserialize)]
struct ResponsesIncompleteDetails {
    #[serde(default)]
    reason: Option<String>,
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

    #[test]
    fn convert_user_and_tool_results() {
        let input = [
            user("hi"),
            assistant_tool(
                None,
                "call_1",
                "bash",
                serde_json::json!({"command": "echo hi"}),
            ),
            tool_results(&[("call_1", "hi\n")]),
        ];
        let items = convert_messages(&input).unwrap();
        assert_eq!(items.len(), 3);
        assert!(matches!(
            &items[0],
            ResponsesInputItem::Message { role, content }
                if role == "user"
                    && *content == ResponsesMessageContent::Text("hi".into())
        ));
        // The wire carries the minted positional id, not the stored one; the
        // output names the same call.
        let call_id = match &items[1] {
            ResponsesInputItem::FunctionCall { call_id, name, .. } if name == "bash" => {
                call_id.clone()
            }
            other => panic!("expected function_call, got {other:?}"),
        };
        assert_ne!(call_id, "call_1");
        assert!(matches!(
            &items[2],
            ResponsesInputItem::FunctionCallOutput {
                call_id: out_id,
                output,
                ..
            } if *out_id == call_id && *output == ResponsesFunctionOutput::Text("hi\n".into())
        ));
        // Text-only output serializes as a plain string, not a parts array.
        let json = serde_json::to_value(&items).unwrap();
        assert_eq!(json[2]["output"], "hi\n");
    }

    #[test]
    fn tool_result_image_becomes_output_parts() {
        let input = [
            assistant_tool(None, "call_1", "view_image", serde_json::json!({})),
            Message::ToolResults {
                tool_use_results: vec![ToolResult {
                    id: "call_1".into(),
                    content: vec![
                        Content::Text { text: "ok".into() },
                        Content::Image {
                            source: "data:image/png;base64,AAAA".into(),
                        },
                    ],
                    is_error: false,
                }],
            },
        ];
        let json = serde_json::to_value(convert_messages(&input).unwrap()).unwrap();
        assert_eq!(json[1]["type"], "function_call_output");
        assert_eq!(json[1]["output"][0]["type"], "input_text");
        assert_eq!(json[1]["output"][0]["text"], "ok");
        assert_eq!(json[1]["output"][1]["type"], "input_image");
        assert_eq!(
            json[1]["output"][1]["image_url"],
            "data:image/png;base64,AAAA"
        );
    }

    #[test]
    fn image_only_tool_result_has_no_empty_text_part() {
        let input = [
            assistant_tool(None, "call_1", "view_image", serde_json::json!({})),
            Message::ToolResults {
                tool_use_results: vec![ToolResult {
                    id: "call_1".into(),
                    content: vec![Content::Image {
                        source: "data:image/png;base64,AAAA".into(),
                    }],
                    is_error: false,
                }],
            },
        ];
        let json = serde_json::to_value(convert_messages(&input).unwrap()).unwrap();
        assert_eq!(json[1]["output"].as_array().unwrap().len(), 1);
        assert_eq!(json[1]["output"][0]["type"], "input_image");
    }

    /// A function_call item without call_id/name must fail the stream, not
    /// flow an empty id into persisted history (which would 400 every later
    /// request, resume included).
    #[test]
    fn function_call_without_call_id_fails_loud() {
        let mut acc = StreamAccumulator::default();
        let err = acc
            .handle_event(ResponsesStreamEvent::ResponseOutputItemAdded {
                output_index: 0,
                item: ResponsesOutputItem::FunctionCall {
                    call_id: None,
                    name: Some("bash".into()),
                    arguments: None,
                },
            })
            .expect_err("missing call_id must be malformed");
        assert!(
            matches!(err, GenerateError::MalformedResponseError(_)),
            "{err:?}"
        );
    }

    #[test]
    fn user_image_becomes_input_image_part() {
        let input = [Message::UserMessage {
            content: vec![
                Content::Image {
                    source: "data:image/png;base64,AAAA".into(),
                },
                Content::Text {
                    text: "what is this? @shot.png".into(),
                },
            ],
        }];
        let items = convert_messages(&input).unwrap();
        let json = serde_json::to_value(&items).unwrap();
        assert_eq!(json[0]["role"], "user");
        assert_eq!(json[0]["content"][0]["type"], "input_image");
        assert_eq!(
            json[0]["content"][0]["image_url"],
            "data:image/png;base64,AAAA"
        );
        assert_eq!(json[0]["content"][1]["type"], "input_text");
        assert_eq!(json[0]["content"][1]["text"], "what is this? @shot.png");
    }

    #[test]
    fn text_only_user_message_stays_plain_string() {
        let input = [user("hi")];
        let json = serde_json::to_value(convert_messages(&input).unwrap()).unwrap();
        assert_eq!(json[0]["content"], "hi");
    }

    #[test]
    fn function_call_input_item_serializes_with_type() {
        let item = ResponsesInputItem::FunctionCall {
            type_: "function_call",
            call_id: "call_1".into(),
            name: "bash".into(),
            arguments: r#"{"command":"ls"}"#.into(),
        };
        let json = serde_json::to_value(&item).unwrap();
        assert_eq!(json["type"], "function_call");
        assert_eq!(json["call_id"], "call_1");
        assert_eq!(json["name"], "bash");
    }

    #[test]
    fn stream_accumulator_reasoning_to_thinking() {
        let mut acc = StreamAccumulator::default();

        let items = acc
            .handle_event(ResponsesStreamEvent::ResponseOutputItemAdded {
                output_index: 0,
                item: ResponsesOutputItem::Reasoning,
            })
            .unwrap();
        assert!(matches!(
            &items[0],
            MessagePart::ContentStart(ContentStart::Thinking {
                index: 0,
                redacted: false,
                ..
            })
        ));

        let items = acc
            .handle_event(ResponsesStreamEvent::ResponseReasoningSummaryTextDelta {
                output_index: 0,
                delta: "step one".into(),
            })
            .unwrap();
        expect_thinking_delta(&items[0], 0, "step one");

        let items = acc
            .handle_event(ResponsesStreamEvent::ResponseOutputItemAdded {
                output_index: 1,
                item: ResponsesOutputItem::Message,
            })
            .unwrap();
        assert!(matches!(
            items[0],
            MessagePart::ContentStart(ContentStart::Text { index: 1 })
        ));
    }

    #[test]
    fn stream_accumulator_ignores_raw_reasoning_text() {
        let mut acc = StreamAccumulator::default();

        let items = acc
            .handle_event(ResponsesStreamEvent::ResponseOutputItemAdded {
                output_index: 0,
                item: ResponsesOutputItem::Reasoning,
            })
            .unwrap();
        assert!(matches!(
            &items[0],
            MessagePart::ContentStart(ContentStart::Thinking { index: 0, .. })
        ));

        // Raw reasoning_text must not produce Thinking deltas (summary-only API).
        let items = acc
            .handle_event(ResponsesStreamEvent::ResponseReasoningTextDelta)
            .unwrap();
        assert!(items.is_empty());

        // Summaries still flow.
        let items = acc
            .handle_event(ResponsesStreamEvent::ResponseReasoningSummaryTextDelta {
                output_index: 0,
                delta: "brief summary".into(),
            })
            .unwrap();
        expect_thinking_delta(&items[0], 0, "brief summary");
    }

    #[test]
    fn stream_accumulator_text_and_tool() {
        let mut acc = StreamAccumulator::default();

        let items = acc
            .handle_event(ResponsesStreamEvent::ResponseOutputItemAdded {
                output_index: 0,
                item: ResponsesOutputItem::Message,
            })
            .unwrap();
        assert!(matches!(
            items[0],
            MessagePart::ContentStart(ContentStart::Text { index: 0 })
        ));

        let items = acc
            .handle_event(ResponsesStreamEvent::ResponseOutputTextDelta {
                output_index: 0,
                delta: "Hi".into(),
            })
            .unwrap();
        expect_text_delta(&items[0], 0, "Hi");

        let items = acc
            .handle_event(ResponsesStreamEvent::ResponseOutputItemAdded {
                output_index: 1,
                item: ResponsesOutputItem::FunctionCall {
                    call_id: Some("call_1".into()),
                    name: Some("get_weather".into()),
                    arguments: Some(String::new()),
                },
            })
            .unwrap();
        expect_tool_start(&items[0], 0, "call_1", "get_weather");

        let items = acc
            .handle_event(ResponsesStreamEvent::ResponseFunctionCallArgumentsDelta {
                output_index: 1,
                delta: r#"{"city":"SF"}"#.into(),
            })
            .unwrap();
        expect_tool_args_delta(&items[0], 0, r#"{"city":"SF"}"#);

        let items = acc
            .handle_event(ResponsesStreamEvent::ResponseCompleted {
                response: ResponsesCompletedBody {
                    status: Some("completed".into()),
                    incomplete_details: None,
                    error: None,
                    usage: None,
                },
            })
            .unwrap();
        expect_turn_end(&items[0], TurnEndReason::ToolUse);
        acc.finish().unwrap();
    }

    #[test]
    fn stream_accumulator_end_turn_without_tools() {
        let mut acc = StreamAccumulator::default();
        acc.handle_event(ResponsesStreamEvent::ResponseOutputTextDelta {
            output_index: 0,
            delta: "ok".into(),
        })
        .unwrap();
        let items = acc
            .handle_event(ResponsesStreamEvent::ResponseCompleted {
                response: ResponsesCompletedBody {
                    status: Some("completed".into()),
                    incomplete_details: None,
                    error: None,
                    usage: None,
                },
            })
            .unwrap();
        expect_turn_end(&items[0], TurnEndReason::EndTurn);
        acc.finish().unwrap();
    }

    #[test]
    fn request_disables_server_side_store() {
        let request = ResponsesCreateRequest {
            model: "grok-4.5-build",
            input: &[],
            instructions: None,
            tools: None,
            max_output_tokens: Some(128),
            store: false,
            stream: true,
            reasoning: None,
        };
        let json = serde_json::to_value(&request).unwrap();
        assert_eq!(json["store"], false);
        assert_eq!(json["stream"], true);
    }

    #[test]
    fn arguments_delta_without_item_added_is_malformed() {
        let mut acc = StreamAccumulator::default();
        let err = acc
            .handle_event(ResponsesStreamEvent::ResponseFunctionCallArgumentsDelta {
                output_index: 0,
                delta: r#"{"x":1}"#.into(),
            })
            .unwrap_err();
        match err {
            GenerateError::MalformedResponseError(msg) => {
                assert!(msg.contains("without a prior"), "{msg}");
            }
            other => panic!("expected MalformedResponseError, got {other:?}"),
        }
    }

    #[test]
    fn incomplete_content_filter_is_refusal() {
        let mut acc = StreamAccumulator::default();
        let err = acc
            .handle_event(ResponsesStreamEvent::ResponseCompleted {
                response: ResponsesCompletedBody {
                    status: Some("incomplete".into()),
                    incomplete_details: Some(ResponsesIncompleteDetails {
                        reason: Some("content_filter".into()),
                    }),
                    error: None,
                    usage: None,
                },
            })
            .unwrap_err();
        assert!(matches!(err, GenerateError::RefusalError(_)), "{err:?}");
    }
}
