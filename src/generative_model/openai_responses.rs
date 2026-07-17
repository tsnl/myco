//! OpenAI Responses API backend (also used by xAI / Grok gateways).
//!
//! Ref: https://platform.openai.com/docs/api-reference/responses
//! Streaming: https://platform.openai.com/docs/guides/streaming-responses?api-mode=responses
//! xAI: https://docs.x.ai/docs/guides/function-calling

use std::sync::Arc;

use crate::core::*;

use super::*;

/// OpenAI Responses API settings ([`BackendConfig::OpenAIResponses`]).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct OpenAIResponsesBackendConfig {
    /// Base URL including any path prefix, e.g. `https://api.x.ai/v1`.
    /// Requests go to `{base_url}/responses`.
    pub base_url: String,
    pub auth_token: String,
    pub max_output_tokens: Option<usize>,
    pub debug_dump_api_requests: bool,
    /// When set, request provider reasoning/thinking at this effort.
    ///
    /// OpenAI-style gateways may honor `reasoning.effort`; others may ignore it.
    /// Only reasoning *summaries* (`reasoning_summary_text`) are mapped to
    /// [`Content::Thinking`]; raw `reasoning_text` streams are ignored.
    /// Defaults to [`Effort::DEFAULT`] so reasoning is always requested.
    pub effort: Option<Effort>,
}

impl Default for OpenAIResponsesBackendConfig {
    fn default() -> Self {
        Self {
            base_url: "https://api.x.ai/v1".into(),
            auth_token: String::new(),
            max_output_tokens: Some(8192),
            debug_dump_api_requests: false,
            effort: Some(Effort::DEFAULT),
        }
    }
}

impl OpenAIResponsesBackendConfig {
    pub fn default_from_env() -> Self {
        let base_url = std::env::var("OPENAI_BASE_URL")
            .or_else(|_| std::env::var("XAI_API_BASE_URL"))
            .unwrap_or_else(|_| "https://api.x.ai/v1".into());

        let auth_token = std::env::var("OPENAI_API_KEY")
            .or_else(|_| std::env::var("XAI_API_KEY"))
            .or_else(|_| std::env::var("ANTHROPIC_AUTH_TOKEN"))
            .or_else(|_| std::env::var("ANTHROPIC_API_KEY"))
            .unwrap_or_default();

        Self {
            base_url,
            auth_token,
            ..Default::default()
        }
    }
}

/// Stateless OpenAI Responses driver. Conversation history is owned by the caller.
pub struct OpenAIResponsesGenerativeModel {
    model: Model,
    system_prompt: String,
    tools: Vec<ResponsesTool>,
    backend: OpenAIResponsesBackendConfig,
    client: reqwest::Client,
}

impl OpenAIResponsesGenerativeModel {
    pub fn new(
        config: GenerativeModelConfig,
        backend: OpenAIResponsesBackendConfig,
    ) -> Result<Arc<Self>, ModelCreationError> {
        if config.model.backend_kind() != BackendKind::OpenAIResponses {
            return Err(ModelCreationError::BadConfig(format!(
                "Model `{}` is not supported by the OpenAI Responses backend \
                 (expected models for {})",
                config.model,
                BackendKind::OpenAIResponses
            )));
        }

        if backend.auth_token.is_empty() {
            return Err(ModelCreationError::BadConfig(
                "OpenAI Responses auth token is empty (set auth_token, OPENAI_API_KEY, \
                 XAI_API_KEY, or ANTHROPIC_AUTH_TOKEN)"
                    .into(),
            ));
        }

        let client = reqwest::ClientBuilder::new()
            .default_headers(reqwest::header::HeaderMap::from_iter([
                (
                    reqwest::header::CONTENT_TYPE,
                    "application/json".parse().unwrap(),
                ),
                (
                    reqwest::header::AUTHORIZATION,
                    // Never echo the token into the error: it ends up in logs.
                    format!("Bearer {}", backend.auth_token)
                        .parse()
                        .map_err(|e| {
                            ModelCreationError::BadConfig(format!(
                                "auth token is not a valid HTTP header value: {e}"
                            ))
                        })?,
                ),
            ]))
            .build()
            .map_err(|e| ModelCreationError::Uncategorized(format!("{e:?}")))?;

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

    async fn start_response_stream(
        &self,
        input: &[ResponsesInputItem],
    ) -> Result<reqwest::Response, GenerateError> {
        // Best-effort enablement across OpenAI-compatible gateways.
        // Unknown fields are typically ignored by servers that don't support
        // them — but an unknown *value* is a 400: `max` is Anthropic-only
        // (Responses accepts minimal|low|medium|high), so clamp it.
        let reasoning = self.backend.effort.map(|effort| ResponsesReasoningConfig {
            effort: Some(match effort {
                Effort::Max => Effort::High.as_str(),
                other => other.as_str(),
            }),
        });
        let request = ResponsesCreateRequest {
            model: self.model.api_id(),
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

        if self.backend.debug_dump_api_requests {
            eprintln!("{}", serde_json::to_string_pretty(&request).unwrap());
        }

        let base = self.backend.base_url.trim_end_matches('/');
        let raw_response = self
            .client
            .post(format!("{base}/responses"))
            .json(&request)
            .send()
            .await
            .map_err(|e| GenerateError::ExecutionError(format!("{e:?}")))?;

        if !raw_response.status().is_success() {
            let status = raw_response.status();
            let body = raw_response
                .text()
                .await
                .unwrap_or_else(|e| format!("<failed to read body: {e:?}>"));
            return Err(GenerateError::ExecutionError(format!(
                "OpenAI Responses API returned HTTP {status}: {body}"
            )));
        }

        Ok(raw_response)
    }
}

impl GenerativeModel for OpenAIResponsesGenerativeModel {
    fn generate(&self, input: &[Message]) -> AsyncStream<Result<MessagePart, GenerateError>> {
        let input_items = convert_messages(input);
        let model = self.model;
        let system_prompt = self.system_prompt.clone();
        let tools = self.tools.clone();
        let backend = self.backend.clone();
        let client = self.client.clone();

        let (tx, rx) = tokio::sync::mpsc::channel::<Result<MessagePart, GenerateError>>(32);

        tokio::spawn(async move {
            let driver = OpenAIResponsesGenerativeModel {
                model,
                system_prompt,
                tools,
                backend,
                client,
            };

            let response = match driver.start_response_stream(&input_items).await {
                Ok(r) => r,
                Err(e) => {
                    let _ = tx.send(Err(e)).await;
                    return;
                }
            };

            if let Err(e) = drive_responses_sse_stream(response, tx.clone()).await {
                let _ = tx.send(Err(e)).await;
            }
        });

        Box::pin(futures::stream::unfold(rx, |mut rx| async move {
            rx.recv().await.map(|item| (item, rx))
        }))
    }
}

//
// Message conversion → Responses `input` list
//

fn convert_messages(input: &[Message]) -> Vec<ResponsesInputItem> {
    let mut out = Vec::new();

    for message in input {
        match message {
            Message::UserMessage { content } => {
                out.push(ResponsesInputItem::Message {
                    role: "user".into(),
                    content: content_to_input_text(content),
                });
            }
            Message::ToolResults { tool_use_results } => {
                for result in tool_use_results {
                    out.push(ResponsesInputItem::FunctionCallOutput {
                        type_: "function_call_output",
                        call_id: result.id.clone(),
                        output: tool_result_to_string(result),
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
                let text = content_to_input_text(content);
                if !text.is_empty() {
                    out.push(ResponsesInputItem::Message {
                        role: "assistant".into(),
                        content: text,
                    });
                }
                for tool_use in tool_uses {
                    out.push(ResponsesInputItem::FunctionCall {
                        type_: "function_call",
                        call_id: tool_use.id.clone(),
                        name: tool_use.name.clone(),
                        arguments: tool_use.input.to_string(),
                    });
                }
            }
        }
    }

    out
}

fn content_to_input_text(content: &[Content]) -> String {
    content
        .iter()
        .filter_map(|c| match c {
            Content::Text { text } => Some(text.as_str()),
            // Thinking is not sent as ordinary assistant text on OpenAI Responses.
            Content::Image { .. } | Content::Thinking { .. } => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn tool_result_to_string(result: &ToolResult) -> String {
    let text = result
        .content
        .iter()
        .filter_map(|c| match c {
            Content::Text { text } => Some(text.as_str()),
            Content::Image { source } => Some(source.as_str()),
            Content::Thinking { .. } => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    if result.is_error && !text.is_empty() {
        format!("Error: {text}")
    } else if result.is_error {
        "Error".into()
    } else {
        text
    }
}

//
// SSE streaming
//

async fn drive_responses_sse_stream(
    response: reqwest::Response,
    tx: tokio::sync::mpsc::Sender<Result<MessagePart, GenerateError>>,
) -> Result<(), GenerateError> {
    if tx.send(Ok(MessagePart::MessageStart)).await.is_err() {
        // Consumer dropped (turn cancelled): stop reading so the response
        // body drops and the provider stops generating/billing.
        return Ok(());
    }

    let mut byte_stream = response.bytes_stream();
    let mut sse = SseParser::default();
    let mut acc = StreamAccumulator::default();

    while let Some(chunk) = byte_stream.next().await {
        let chunk = chunk.map_err(|e| {
            GenerateError::ExecutionError(format!(
                "Error reading OpenAI Responses stream body: {e:?}"
            ))
        })?;

        for data in sse.push(&chunk) {
            let event: ResponsesStreamEvent = serde_json::from_str(&data).map_err(|e| {
                GenerateError::MalformedResponseError(format!(
                    "Failed to parse OpenAI Responses SSE event JSON: {e}; data={data}"
                ))
            })?;

            for item in acc.handle_event(event)? {
                if tx.send(Ok(item)).await.is_err() {
                    // Consumer dropped (turn cancelled): stop reading so the
                    // response body drops and the provider stops generating.
                    return Ok(());
                }
            }

            if acc.finished {
                break;
            }
        }

        if acc.finished {
            break;
        }
    }

    acc.finish()?;
    Ok(())
}

/// Maps Responses `output_index` slots onto separate content / tool-use index spaces.
#[derive(Default)]
struct StreamAccumulator {
    output_kinds: Vec<Option<OutputKind>>,
    tool_input_json: Vec<Option<String>>,
    saw_tool_call: bool,
    stop_reason: Option<TurnEndReason>,
    finished: bool,
}

#[derive(Clone, Copy)]
enum OutputKind {
    Content { index: usize },
    Thinking { index: usize },
    ToolUse { index: usize },
    Ignored,
}

impl StreamAccumulator {
    fn ensure_slot(&mut self, output_index: usize) {
        while self.output_kinds.len() <= output_index {
            self.output_kinds.push(None);
            self.tool_input_json.push(None);
        }
    }

    fn content_count(&self) -> usize {
        self.output_kinds
            .iter()
            .filter(|k| {
                matches!(
                    k,
                    Some(OutputKind::Content { .. } | OutputKind::Thinking { .. })
                )
            })
            .count()
    }

    fn tool_use_count(&self) -> usize {
        self.output_kinds
            .iter()
            .filter(|k| matches!(k, Some(OutputKind::ToolUse { .. })))
            .count()
    }

    fn handle_event(
        &mut self,
        event: ResponsesStreamEvent,
    ) -> Result<Vec<MessagePart>, GenerateError> {
        let mut out = Vec::new();

        match event {
            ResponsesStreamEvent::ResponseCreated { .. }
            | ResponsesStreamEvent::ResponseInProgress { .. } => {}
            ResponsesStreamEvent::ResponseOutputItemAdded { output_index, item } => {
                self.ensure_slot(output_index);
                match item {
                    ResponsesOutputItem::Message { .. } => {
                        let content_index = self.content_count();
                        self.output_kinds[output_index] = Some(OutputKind::Content {
                            index: content_index,
                        });
                        out.push(MessagePart::ContentStart(ContentStart::Text {
                            index: content_index,
                        }));
                    }
                    ResponsesOutputItem::Reasoning { .. } => {
                        let content_index = self.content_count();
                        self.output_kinds[output_index] = Some(OutputKind::Thinking {
                            index: content_index,
                        });
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
                        ..
                    } => {
                        let tool_index = self.tool_use_count();
                        self.output_kinds[output_index] =
                            Some(OutputKind::ToolUse { index: tool_index });
                        self.tool_input_json[output_index] = Some(arguments.unwrap_or_default());
                        self.saw_tool_call = true;
                        out.push(MessagePart::ToolUseStart(ToolUseStart {
                            index: tool_index,
                            id: call_id.unwrap_or_default(),
                            name: name.unwrap_or_default(),
                        }));
                    }
                    ResponsesOutputItem::Other => {
                        self.output_kinds[output_index] = Some(OutputKind::Ignored);
                    }
                }
            }
            ResponsesStreamEvent::ResponseOutputTextDelta {
                output_index,
                delta,
                ..
            } => {
                self.ensure_slot(output_index);
                // Lazy-open a content slot if the gateway skips output_item.added for text.
                if self.output_kinds[output_index].is_none() {
                    let content_index = self.content_count();
                    self.output_kinds[output_index] = Some(OutputKind::Content {
                        index: content_index,
                    });
                    out.push(MessagePart::ContentStart(ContentStart::Text {
                        index: content_index,
                    }));
                }
                if let Some(OutputKind::Content { index }) = self.output_kinds[output_index] {
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
                self.ensure_slot(output_index);
                // Lazy-open a thinking slot if the gateway streams deltas without item.added.
                if self.output_kinds[output_index].is_none()
                    || matches!(self.output_kinds[output_index], Some(OutputKind::Ignored))
                {
                    let content_index = self.content_count();
                    self.output_kinds[output_index] = Some(OutputKind::Thinking {
                        index: content_index,
                    });
                    out.push(MessagePart::ContentStart(ContentStart::Thinking {
                        index: content_index,
                        signature: None,
                        redacted: false,
                    }));
                }
                match self.output_kinds[output_index] {
                    Some(OutputKind::Thinking { index }) if !delta.is_empty() => {
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
            ResponsesStreamEvent::ResponseReasoningTextDelta { .. } => {}
            ResponsesStreamEvent::ResponseFunctionCallArgumentsDelta {
                output_index,
                delta,
                ..
            } => {
                self.ensure_slot(output_index);
                match self.output_kinds.get(output_index).and_then(|k| *k) {
                    Some(OutputKind::ToolUse { index }) => {
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
                ..
            } => {
                self.ensure_slot(output_index);
                if let Some(arguments) = arguments {
                    // Prefer the final assembled arguments when the gateway provides them.
                    if let Some(OutputKind::ToolUse { index }) =
                        self.output_kinds.get(output_index).and_then(|k| *k)
                    {
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
                    self.tool_input_json[output_index] = Some(arguments);
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

    fn finish(self) -> Result<(), GenerateError> {
        if self.stop_reason.is_none() {
            return Err(GenerateError::MalformedResponseError(
                "OpenAI Responses stream ended without response.completed".into(),
            ));
        }

        for (i, json) in self.tool_input_json.into_iter().enumerate() {
            if let Some(json) = json {
                let json = if json.is_empty() { "{}" } else { json.as_str() };
                if let Err(e) = serde_json::from_str::<serde_json::Value>(json) {
                    return Err(GenerateError::MalformedResponseError(format!(
                        "Malformed stream: function call arguments at output {i} invalid: {e}"
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
        content: String,
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
        output: String,
    },
}

#[derive(Debug, serde::Deserialize)]
#[serde(tag = "type")]
enum ResponsesStreamEvent {
    #[serde(rename = "response.created")]
    ResponseCreated {
        #[serde(default)]
        #[allow(dead_code)]
        response: serde_json::Value,
    },
    #[serde(rename = "response.in_progress")]
    ResponseInProgress {
        #[serde(default)]
        #[allow(dead_code)]
        response: serde_json::Value,
    },
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
        #[serde(default)]
        #[allow(dead_code)]
        content_index: Option<usize>,
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
    ResponseReasoningTextDelta {
        #[serde(default)]
        #[allow(dead_code)]
        output_index: usize,
        #[allow(dead_code)]
        delta: String,
    },
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
        #[serde(default)]
        #[allow(dead_code)]
        name: Option<String>,
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
    Message {
        #[serde(default)]
        #[allow(dead_code)]
        id: Option<String>,
        #[serde(default)]
        #[allow(dead_code)]
        role: Option<String>,
    },
    #[serde(rename = "reasoning")]
    Reasoning {
        #[serde(default)]
        #[allow(dead_code)]
        id: Option<String>,
    },
    #[serde(rename = "function_call")]
    FunctionCall {
        #[serde(default)]
        #[allow(dead_code)]
        id: Option<String>,
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
    usage: Option<ResponsesUsage>,
}

#[derive(Debug, Clone, serde::Deserialize)]
struct ResponsesUsage {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
    #[serde(default)]
    input_tokens_details: Option<ResponsesInputTokensDetails>,
}

#[derive(Debug, Clone, serde::Deserialize)]
struct ResponsesInputTokensDetails {
    #[serde(default)]
    cached_tokens: Option<u64>,
}

impl ResponsesUsage {
    fn into_token_usage(self) -> crate::generative_model::TokenUsage {
        crate::generative_model::TokenUsage {
            input_tokens: self.input_tokens,
            output_tokens: self.output_tokens,
            cache_read_tokens: self.input_tokens_details.and_then(|d| d.cached_tokens),
            cache_creation_tokens: None,
        }
    }
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

    #[test]
    fn convert_user_and_tool_results() {
        let input = [
            Message::UserMessage {
                content: vec![Content::Text { text: "hi".into() }],
            },
            Message::AssistantMessage {
                content: vec![],
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
        let items = convert_messages(&input);
        assert_eq!(items.len(), 3);
        assert!(matches!(
            &items[0],
            ResponsesInputItem::Message { role, content }
                if role == "user" && content == "hi"
        ));
        assert!(matches!(
            &items[1],
            ResponsesInputItem::FunctionCall {
                call_id,
                name,
                ..
            } if call_id == "call_1" && name == "bash"
        ));
        assert!(matches!(
            &items[2],
            ResponsesInputItem::FunctionCallOutput {
                call_id,
                output,
                ..
            } if call_id == "call_1" && output == "hi\n"
        ));
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
    fn sse_parser_basic() {
        let mut parser = SseParser::default();
        let chunk = b"event: response.created\ndata: {\"type\":\"response.created\"}\n\n\
                      data: {\"type\":\"response.output_text.delta\",\"delta\":\"Hi\"}\n\n";
        let events = parser.push(chunk);
        assert_eq!(events.len(), 2);
        assert!(events[0].contains("response.created"));
        assert!(events[1].contains("output_text.delta"));
    }

    #[test]
    fn stream_accumulator_reasoning_to_thinking() {
        let mut acc = StreamAccumulator::default();

        let items = acc
            .handle_event(ResponsesStreamEvent::ResponseOutputItemAdded {
                output_index: 0,
                item: ResponsesOutputItem::Reasoning {
                    id: Some("r1".into()),
                },
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
        match &items[0] {
            MessagePart::ContentDelta(ContentDelta::Thinking { index, delta }) => {
                assert_eq!(*index, 0);
                assert_eq!(delta, "step one");
            }
            other => panic!("expected thinking delta, got {other:?}"),
        }

        let items = acc
            .handle_event(ResponsesStreamEvent::ResponseOutputItemAdded {
                output_index: 1,
                item: ResponsesOutputItem::Message {
                    id: None,
                    role: Some("assistant".into()),
                },
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
                item: ResponsesOutputItem::Reasoning {
                    id: Some("r1".into()),
                },
            })
            .unwrap();
        assert!(matches!(
            &items[0],
            MessagePart::ContentStart(ContentStart::Thinking { index: 0, .. })
        ));

        // Raw reasoning_text must not produce Thinking deltas (summary-only API).
        let items = acc
            .handle_event(ResponsesStreamEvent::ResponseReasoningTextDelta {
                output_index: 0,
                delta: "secret chain of thought".into(),
            })
            .unwrap();
        assert!(items.is_empty());

        // Summaries still flow.
        let items = acc
            .handle_event(ResponsesStreamEvent::ResponseReasoningSummaryTextDelta {
                output_index: 0,
                delta: "brief summary".into(),
            })
            .unwrap();
        match &items[0] {
            MessagePart::ContentDelta(ContentDelta::Thinking { delta, .. }) => {
                assert_eq!(delta, "brief summary");
            }
            other => panic!("expected summary thinking delta, got {other:?}"),
        }
    }

    #[test]
    fn stream_accumulator_text_and_tool() {
        let mut acc = StreamAccumulator::default();

        let items = acc
            .handle_event(ResponsesStreamEvent::ResponseOutputItemAdded {
                output_index: 0,
                item: ResponsesOutputItem::Message {
                    id: None,
                    role: Some("assistant".into()),
                },
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
                content_index: Some(0),
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
            .handle_event(ResponsesStreamEvent::ResponseOutputItemAdded {
                output_index: 1,
                item: ResponsesOutputItem::FunctionCall {
                    id: Some("fc_1".into()),
                    call_id: Some("call_1".into()),
                    name: Some("get_weather".into()),
                    arguments: Some(String::new()),
                },
            })
            .unwrap();
        match &items[0] {
            MessagePart::ToolUseStart(ToolUseStart { index, id, name }) => {
                assert_eq!(*index, 0);
                assert_eq!(id, "call_1");
                assert_eq!(name, "get_weather");
            }
            _ => panic!(),
        }

        let items = acc
            .handle_event(ResponsesStreamEvent::ResponseFunctionCallArgumentsDelta {
                output_index: 1,
                delta: r#"{"city":"SF"}"#.into(),
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
        assert!(matches!(
            items[0],
            MessagePart::TurnEndReason(TurnEndReason::ToolUse)
        ));
        acc.finish().unwrap();
    }

    #[test]
    fn stream_accumulator_end_turn_without_tools() {
        let mut acc = StreamAccumulator::default();
        acc.handle_event(ResponsesStreamEvent::ResponseOutputTextDelta {
            output_index: 0,
            delta: "ok".into(),
            content_index: None,
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
        assert!(matches!(
            items[0],
            MessagePart::TurnEndReason(TurnEndReason::EndTurn)
        ));
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
