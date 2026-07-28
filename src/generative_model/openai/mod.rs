//! OpenAI-dialect backends: one driver, two wire dialects.
//!
//! **Responses** (`{base_url}/responses` — OpenAI, xAI, OpenRouter) and **Chat
//! Completions** (`{base_url}/chat/completions` — the older dialect llama.cpp,
//! Ollama, vLLM, LM Studio, DeepSeek and Groq speak). The catalog protocol
//! picks the [`Dialect`], which owns the only two things that differ: how a
//! request body is built and how its stream is decoded. Credentials, HTTP, SSE
//! framing, history rendering, and reasoning effort are shared.
//!
//! Refs:
//! - <https://platform.openai.com/docs/api-reference/responses>
//! - <https://platform.openai.com/docs/api-reference/chat/create>

use std::sync::Arc;

use crate::core::*;

use super::*;

mod completions;
mod responses;

/// Settings for either OpenAI dialect ([`BackendConfig::OpenAIResponses`] /
/// [`BackendConfig::OpenAICompletions`]).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct OpenAIBackendConfig {
    /// Base URL including any path prefix, e.g. `https://api.x.ai/v1` or
    /// `http://localhost:11434/v1`.
    pub base_url: String,
    pub auth_token: String,
    pub max_output_tokens: Option<usize>,
    pub debug_dump_api_requests: bool,
    /// When set, request provider reasoning at this effort.
    ///
    /// Sent as `reasoning.effort` (Responses) or `reasoning_effort` (Chat
    /// Completions). Servers that predate the field ignore it; OpenAI rejects
    /// it on non-reasoning models, so those need `thinking = "none"` in the
    /// catalog. Defaults to [`Effort::DEFAULT`] so reasoning is always requested.
    pub effort: Option<Effort>,
}

impl Default for OpenAIBackendConfig {
    fn default() -> Self {
        Self {
            // No built-in gateway: the catalog (config.toml) supplies base_url.
            base_url: String::new(),
            auth_token: String::new(),
            max_output_tokens: Some(8192),
            debug_dump_api_requests: false,
            effort: Some(Effort::DEFAULT),
        }
    }
}

/// Which wire dialect a model is served over. Resolved once from the catalog
/// protocol so request building and stream decoding cannot disagree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Dialect {
    Responses,
    Completions,
}

impl Dialect {
    fn of(protocol: Protocol) -> Option<Self> {
        match protocol {
            Protocol::OpenAIResponses => Some(Dialect::Responses),
            Protocol::OpenAICompletions => Some(Dialect::Completions),
            Protocol::AnthropicMessages => None,
        }
    }

    /// Path appended to the gateway base URL.
    fn path(self) -> &'static str {
        match self {
            Dialect::Responses => "responses",
            Dialect::Completions => "chat/completions",
        }
    }

    /// Name for user-facing errors.
    fn label(self) -> &'static str {
        match self {
            Dialect::Responses => "OpenAI Responses",
            Dialect::Completions => "OpenAI Chat Completions",
        }
    }

    fn request_body(self, driver: &OpenAIGenerativeModel, input: &[Message]) -> serde_json::Value {
        match self {
            Dialect::Responses => responses::request_body(driver, input),
            Dialect::Completions => completions::request_body(driver, input),
        }
    }

    fn decoder(self) -> Decoder {
        match self {
            Dialect::Responses => Decoder::Responses(responses::Decoder::default()),
            Dialect::Completions => Decoder::Completions(completions::Decoder::default()),
        }
    }
}

/// Stateless OpenAI-dialect driver. Conversation history is owned by the caller.
pub struct OpenAIGenerativeModel {
    model: ModelSpec,
    dialect: Dialect,
    system_prompt: String,
    tools: Vec<ToolSpec>,
    backend: OpenAIBackendConfig,
    client: reqwest::Client,
}

impl OpenAIGenerativeModel {
    pub fn new(
        config: GenerativeModelConfig,
        backend: OpenAIBackendConfig,
    ) -> Result<Arc<Self>, ModelCreationError> {
        let dialect = Dialect::of(config.model.protocol).ok_or_else(|| {
            ModelCreationError::BadConfig(format!(
                "model `{}` speaks {}, not {} or {}",
                config.model,
                config.model.protocol,
                Protocol::OpenAIResponses,
                Protocol::OpenAICompletions
            ))
        })?;

        let mut headers = reqwest::header::HeaderMap::from_iter([(
            reqwest::header::CONTENT_TYPE,
            "application/json".parse().unwrap(),
        )]);
        // Empty token = `auth = "none"` in the catalog (local servers); send no
        // Authorization header. Credential *presence* is the catalog's job
        // (`ModelCatalog::get`), not the driver's.
        if !backend.auth_token.is_empty() {
            headers.insert(
                reqwest::header::AUTHORIZATION,
                // Never echo the token into the error: it ends up in logs.
                format!("Bearer {}", backend.auth_token)
                    .parse()
                    .map_err(|e| {
                        ModelCreationError::BadConfig(format!(
                            "auth token is not a valid HTTP header value: {e}"
                        ))
                    })?,
            );
        }
        let client = reqwest::ClientBuilder::new()
            .default_headers(headers)
            .build()
            .map_err(|e| ModelCreationError::Uncategorized(format!("{e:?}")))?;

        Ok(Arc::new(Self {
            model: config.model,
            dialect,
            system_prompt: config.system_prompt,
            tools: config.tools,
            backend,
            client,
        }))
    }
}

impl GenerativeModel for OpenAIGenerativeModel {
    fn generate(&self, input: &[Message]) -> AsyncStream<Result<MessagePart, GenerateError>> {
        // Build the request while we still have `&self`: the spawned task needs
        // only the finished body, the URL, and the dialect.
        let request = self.dialect.request_body(self, input);
        if self.backend.debug_dump_api_requests {
            eprintln!("{}", serde_json::to_string_pretty(&request).unwrap());
        }
        let url = format!(
            "{}/{}",
            self.backend.base_url.trim_end_matches('/'),
            self.dialect.path()
        );
        let dialect = self.dialect;
        let client = self.client.clone();

        let (tx, rx) = tokio::sync::mpsc::channel::<Result<MessagePart, GenerateError>>(32);

        tokio::spawn(async move {
            let response = match post_stream(&client, &url, &request, dialect).await {
                Ok(r) => r,
                Err(e) => {
                    let _ = tx.send(Err(e)).await;
                    return;
                }
            };

            if let Err(e) = drive_sse_stream(response, dialect, tx.clone()).await {
                let _ = tx.send(Err(e)).await;
            }
        });

        Box::pin(futures::stream::unfold(rx, |mut rx| async move {
            rx.recv().await.map(|item| (item, rx))
        }))
    }
}

async fn post_stream(
    client: &reqwest::Client,
    url: &str,
    request: &serde_json::Value,
    dialect: Dialect,
) -> Result<reqwest::Response, GenerateError> {
    let raw_response = client
        .post(url)
        .json(request)
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
            "{} API returned HTTP {status}: {body}",
            dialect.label()
        )));
    }

    Ok(raw_response)
}

async fn drive_sse_stream(
    response: reqwest::Response,
    dialect: Dialect,
    tx: tokio::sync::mpsc::Sender<Result<MessagePart, GenerateError>>,
) -> Result<(), GenerateError> {
    if tx.send(Ok(MessagePart::MessageStart)).await.is_err() {
        // Consumer dropped (turn cancelled): stop reading so the response
        // body drops and the provider stops generating/billing.
        return Ok(());
    }

    let mut byte_stream = response.bytes_stream();
    let mut sse = SseParser::default();
    let mut decoder = dialect.decoder();

    while let Some(chunk) = byte_stream.next().await {
        let chunk = chunk.map_err(|e| {
            GenerateError::ExecutionError(format!(
                "Error reading {} stream body: {e:?}",
                dialect.label()
            ))
        })?;

        for data in sse.push(&chunk) {
            for item in decoder.handle(&data)? {
                if tx.send(Ok(item)).await.is_err() {
                    // Consumer dropped (turn cancelled): stop reading so the
                    // response body drops and the provider stops generating.
                    return Ok(());
                }
            }
            if decoder.finished() {
                break;
            }
        }

        if decoder.finished() {
            break;
        }
    }

    decoder.finish()
}

/// Per-dialect stream state machine: SSE `data:` payloads in, [`MessagePart`]s out.
enum Decoder {
    Responses(responses::Decoder),
    Completions(completions::Decoder),
}

impl Decoder {
    fn handle(&mut self, data: &str) -> Result<Vec<MessagePart>, GenerateError> {
        match self {
            Decoder::Responses(d) => d.handle(data),
            Decoder::Completions(d) => d.handle(data),
        }
    }

    /// Whether the turn is over on the wire. Responses says so explicitly with
    /// `response.completed`; Chat Completions keeps reading past its
    /// `finish_reason` because the usage totals arrive in a later chunk.
    fn finished(&self) -> bool {
        match self {
            Decoder::Responses(d) => d.finished,
            Decoder::Completions(_) => false,
        }
    }

    fn finish(self) -> Result<(), GenerateError> {
        match self {
            Decoder::Responses(d) => d.finish(),
            Decoder::Completions(d) => d.finish(),
        }
    }
}

//
// Shared history → request conversion
//

/// History text: `Text` blocks joined by newlines. Thinking is never echoed
/// back to the provider; images travel as their own content parts.
fn text_of(content: &[Content]) -> String {
    content
        .iter()
        .filter_map(|c| match c {
            Content::Text { text } => Some(text.as_str()),
            Content::Image { .. } | Content::Thinking { .. } => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Image URL fields accept http(s) and `data:` URLs. Same source policy as the
/// Anthropic driver: pass URLs through, treat anything else as raw base64 PNG.
fn image_url(source: &str) -> String {
    if source.starts_with("http://")
        || source.starts_with("https://")
        || source.starts_with("data:")
    {
        return source.to_string();
    }
    format!("data:image/png;base64,{source}")
}

/// Image sources in arrival order (e.g. a `view_image` tool result).
fn images_of(content: &[Content]) -> Vec<&str> {
    content
        .iter()
        .filter_map(|c| match c {
            Content::Image { source } => Some(source.as_str()),
            Content::Text { .. } | Content::Thinking { .. } => None,
        })
        .collect()
}

/// The text half of a tool result, error-prefixed. Images are carried
/// separately by each dialect ([`images_of`]).
fn tool_result_text(result: &ToolResult) -> String {
    let text = text_of(&result.content);
    if result.is_error && !text.is_empty() {
        format!("Error: {text}")
    } else if result.is_error {
        "Error".into()
    } else {
        text
    }
}

/// Wire value for `reasoning.effort` / `reasoning_effort`, or `None` when the
/// catalog opted out (`thinking = "none"`).
///
/// Unknown *fields* are typically ignored by OpenAI-compatible servers, but an
/// unknown *value* is a 400: `max` is Anthropic-only (both dialects take
/// minimal|low|medium|high), so clamp it.
fn reasoning_effort(driver: &OpenAIGenerativeModel) -> Option<&'static str> {
    if driver.model.thinking != ThinkingMode::Effort {
        return None;
    }
    driver.backend.effort.map(|effort| match effort {
        Effort::Max => Effort::High.as_str(),
        other => other.as_str(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A driver wired to `protocol`, for request-shape tests. Callers tweak the
    /// fields they care about (system prompt, tools, thinking, …).
    pub(super) fn driver(protocol: Protocol) -> OpenAIGenerativeModel {
        OpenAIGenerativeModel {
            model: ModelSpec {
                key: "test".into(),
                api_id: "test-model".into(),
                protocol,
                thinking: ThinkingMode::default_for(protocol),
                context_window_tokens: 100_000,
            },
            dialect: Dialect::of(protocol).expect("openai protocol"),
            system_prompt: String::new(),
            tools: Vec::new(),
            backend: OpenAIBackendConfig {
                max_output_tokens: Some(128),
                ..Default::default()
            },
            client: reqwest::Client::new(),
        }
    }

    #[test]
    fn dialect_endpoints_follow_the_catalog_protocol() {
        assert_eq!(
            Dialect::of(Protocol::OpenAIResponses).unwrap().path(),
            "responses"
        );
        assert_eq!(
            Dialect::of(Protocol::OpenAICompletions).unwrap().path(),
            "chat/completions"
        );
        assert_eq!(Dialect::of(Protocol::AnthropicMessages), None);
    }

    #[test]
    fn new_rejects_a_non_openai_model() {
        let config = GenerativeModelConfig {
            model: ModelSpec {
                key: "opus".into(),
                api_id: "claude-opus-4-8".into(),
                protocol: Protocol::AnthropicMessages,
                thinking: ThinkingMode::Adaptive,
                context_window_tokens: 1_000,
            },
            tools: Vec::new(),
            system_prompt: String::new(),
            backend_config: BackendConfig::OpenAIResponses(OpenAIBackendConfig::default()),
        };
        let err = OpenAIGenerativeModel::new(config, OpenAIBackendConfig::default())
            .err()
            .expect("anthropic model rejected");
        assert!(err.to_string().contains("anthropic-messages"), "{err}");
    }

    #[test]
    fn reasoning_effort_clamps_max_and_honors_thinking_none() {
        let mut d = driver(Protocol::OpenAIResponses);
        d.backend.effort = Some(Effort::Max);
        assert_eq!(reasoning_effort(&d), Some("high"));
        d.backend.effort = Some(Effort::Low);
        assert_eq!(reasoning_effort(&d), Some("low"));
        d.model.thinking = ThinkingMode::None;
        assert_eq!(reasoning_effort(&d), None);
    }

    #[test]
    fn image_url_passes_urls_and_wraps_raw_base64() {
        assert_eq!(image_url("https://x.test/a.png"), "https://x.test/a.png");
        assert_eq!(
            image_url("data:image/jpeg;base64,AA"),
            "data:image/jpeg;base64,AA"
        );
        assert_eq!(image_url("iVBOR"), "data:image/png;base64,iVBOR");
    }

    #[test]
    fn text_and_images_split_by_kind() {
        let content = [
            Content::Thinking {
                text: "hidden".into(),
                signature: None,
                redacted: false,
            },
            Content::Text { text: "one".into() },
            Content::Image {
                source: "iVBOR".into(),
            },
            Content::Text { text: "two".into() },
        ];
        assert_eq!(text_of(&content), "one\ntwo");
        assert_eq!(images_of(&content), ["iVBOR"]);
    }

    #[test]
    fn tool_result_errors_are_prefixed() {
        assert_eq!(tool_result_text(&ToolResult::text("ok")), "ok");
        assert_eq!(tool_result_text(&ToolResult::err("boom")), "Error: boom");
        assert_eq!(tool_result_text(&ToolResult::err("")), "Error");
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
}
