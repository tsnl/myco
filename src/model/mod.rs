#![doc = include_str!("README.md")]

use std::{
    pin::Pin,
    task::{Context, Poll},
};

use futures_core::{Stream, stream::FusedStream};
use serde_json::{Map, Value};

mod anthropic_backend;
mod backend_helpers;
mod http_helpers;
mod openai_responses_backend;

use backend_helpers::{Driver, EventStream};

/// Complete endpoint URLs; empty keys omit authentication.
pub enum Config {
    OpenAi { endpoint: String, api_key: String },
    Anthropic { endpoint: String, api_key: String },
}

pub struct GenAiClient {
    driver: Box<dyn Driver>,
}

impl GenAiClient {
    pub fn new(config: Config) -> Result<Self, Error> {
        Ok(Self {
            driver: backend_helpers::driver(config)?,
        })
    }

    /// Validates and encodes immediately; network I/O waits for polling.
    pub fn generate(&self, request: Request) -> Result<Generation<'_>, Error> {
        backend_helpers::generate(self.driver.as_ref(), request)
    }
}

/// Completion, error, or drop releases the attempt's HTTP request.
pub struct Generation<'a> {
    inner: Option<EventStream<'a>>,
}

impl Stream for Generation<'_> {
    type Item = Result<Event, Error>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        backend_helpers::poll_generation(self.get_mut(), cx)
    }
}

impl FusedStream for Generation<'_> {
    fn is_terminated(&self) -> bool {
        self.inner.is_none()
    }
}

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
    pub driver_options: Map<String, Value>,
}

impl Request {
    pub fn new(model: impl Into<String>, messages: Vec<Message>, max_output_tokens: u32) -> Self {
        Self {
            model: model.into(),
            instructions: String::new(),
            messages,
            tools: vec![],
            max_output_tokens,
            driver_options: Map::new(),
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
    pub arguments: Result<Value, String>,
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

#[derive(Debug, Clone, PartialEq)]
pub struct Response {
    output: Vec<Output>,
    finish: Finish,
    usage: Usage,
    provider: Option<ProviderResponse>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ProviderResponse {
    pub protocol: Protocol,
    /// Native Responses object or assembled Messages object.
    pub body: Value,
}

impl Response {
    pub fn new(output: Vec<Output>, finish: Finish, usage: Usage) -> Self {
        Self {
            output,
            finish,
            usage,
            provider: None,
        }
    }

    pub fn from_provider(protocol: Protocol, body: Value) -> Result<Self, Error> {
        backend_helpers::from_provider(protocol, body)
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
    /// Provider output-item or content-block index.
    pub index: usize,
    pub kind: DeltaKind,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    Request { protocol: Protocol, body: Value },
    Progress { raw: Value, delta: Option<Delta> },
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
