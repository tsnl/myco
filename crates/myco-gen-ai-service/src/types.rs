use serde_json::{Map, Value};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Protocol {
    OpenAiResponses,
    AnthropicMessages,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Request {
    pub model: String,
    pub instructions: String,
    pub messages: Vec<Message>,
    pub tools: Vec<Tool>,
    pub max_output_tokens: u32,
    /// Additional provider settings, such as `reasoning` or `thinking`.
    /// Core request fields cannot be overridden.
    pub provider_options: Map<String, Value>,
}

impl Request {
    pub fn new(model: impl Into<String>, messages: Vec<Message>, max_output_tokens: u32) -> Self {
        Self {
            model: model.into(),
            instructions: String::new(),
            messages,
            tools: vec![],
            max_output_tokens,
            provider_options: Map::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Message {
    User(String),
    Assistant(Response),
    ToolResult {
        call_id: String,
        output: String,
        is_error: bool,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct Tool {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    /// JSON text. A truncated response may contain incomplete arguments.
    /// The caller must check the finish reason and validate before execution.
    pub arguments: String,
}

impl ToolCall {
    pub(crate) fn validate(&self) -> Result<(), Error> {
        if self.id.is_empty() || self.name.is_empty() {
            return Err(Error::Protocol("tool call has an empty ID or name".into()));
        }
        self.validate_arguments()
    }

    fn validate_arguments(&self) -> Result<(), Error> {
        let arguments: Value = serde_json::from_str(&self.arguments).map_err(|e| {
            Error::Protocol(format!("invalid arguments for tool call {}: {e}", self.id))
        })?;
        if !arguments.is_object() {
            return Err(Error::Protocol(format!(
                "arguments for tool call {} must be an object",
                self.id
            )));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Output {
    Text(String),
    Reasoning(String),
    Refusal(String),
    ToolCall(ToolCall),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Finish {
    Stop,
    ToolCalls,
    Length,
    Refusal,
    /// An unrecognized stop or incomplete reason, retained for caller policy.
    Other(String),
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Usage {
    /// Total input tokens, including cache reads and writes.
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cache_read_tokens: Option<u64>,
    pub cache_write_tokens: Option<u64>,
}

/// Immutable output and its provider continuation. Applications own storage
/// encodings and can reconstruct a response with [`Response::from_provider`].
#[derive(Debug, Clone, PartialEq)]
pub struct Response {
    pub(crate) output: Vec<Output>,
    pub(crate) finish: Finish,
    pub(crate) usage: Usage,
    pub(crate) provider: Option<ProviderResponse>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ProviderResponse {
    pub protocol: Protocol,
    /// The terminal Responses object, or the Messages object assembled from
    /// streaming blocks and metadata. Raw stream events are delivered separately.
    pub body: Value,
}

impl Response {
    /// Construct a response without provider state, for example in an evaluation.
    pub fn new(output: Vec<Output>, finish: Finish, usage: Usage) -> Self {
        Self {
            output,
            finish,
            usage,
            provider: None,
        }
    }

    pub fn from_provider(protocol: Protocol, body: Value) -> Result<Self, Error> {
        let mut response = match protocol {
            Protocol::OpenAiResponses => crate::openai::response(&body)?,
            Protocol::AnthropicMessages => crate::anthropic::response(&body)?,
        };
        response.validate_call_ids()?;
        response.provider = Some(ProviderResponse { protocol, body });
        Ok(response)
    }

    fn validate_call_ids(&self) -> Result<(), Error> {
        let mut calls = std::collections::HashSet::new();
        for output in &self.output {
            if let Output::ToolCall(call) = output
                && !calls.insert(&call.id)
            {
                return Err(Error::Protocol(format!(
                    "duplicate tool call ID {}",
                    call.id
                )));
            }
        }
        Ok(())
    }

    pub fn output(&self) -> &[Output] {
        &self.output
    }
    pub fn finish(&self) -> &Finish {
        &self.finish
    }
    pub fn usage(&self) -> &Usage {
        &self.usage
    }
    pub fn provider(&self) -> Option<&ProviderResponse> {
        self.provider.as_ref()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeltaKind {
    Text,
    Reasoning,
    Refusal,
    ToolArguments,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Delta {
    /// Responses output-item index or Messages content-block index.
    /// Further provider coordinates remain available in the raw event.
    pub index: usize,
    pub kind: DeltaKind,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// Exact JSON request body, without authentication headers.
    Request { protocol: Protocol, body: Value },
    /// Provider event plus an optional projection suitable for live rendering.
    /// Partial tool arguments are observations, not executable calls.
    Progress { raw: Value, delta: Option<Delta> },
    /// Validated inference outcome. This service does not persist a thread turn.
    Completed(Response),
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid inference request: {0}")]
    InvalidRequest(String),
    #[error("inference transport failed: {0}")]
    Transport(#[from] reqwest::Error),
    #[error("inference endpoint returned HTTP {status}: {body}")]
    Http {
        status: u16,
        body: String,
        request_id: Option<String>,
        retry_after: Option<String>,
    },
    #[error("invalid inference stream: {0}")]
    Protocol(String),
    #[error("provider reported an inference failure: {0}")]
    Provider(Value),
}

pub(crate) fn field<'a>(value: &'a Value, name: &str) -> Result<&'a str, Error> {
    value
        .get(name)
        .and_then(Value::as_str)
        .ok_or_else(|| Error::Protocol(format!("missing string field {name}")))
}

pub(crate) fn array<'a>(value: &'a Value, name: &str) -> Result<&'a Vec<Value>, Error> {
    value
        .get(name)
        .and_then(Value::as_array)
        .ok_or_else(|| Error::Protocol(format!("missing array field {name}")))
}

pub(crate) fn index(value: &Value, name: &str) -> Result<usize, Error> {
    value[name]
        .as_u64()
        .and_then(|n| usize::try_from(n).ok())
        .ok_or_else(|| Error::Protocol(format!("missing {name}")))
}
