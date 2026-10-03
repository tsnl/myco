#![doc = include_str!("README.md")]

use std::{
    pin::Pin,
    task::{Context, Poll},
};

use futures_core::{Stream, stream::FusedStream};
use serde_json::{Map, Value};

use crate::blob::{BlobError, BlobRef, BlobStore};

mod anthropic_backend;
mod backend_helpers;
mod http_helpers;
mod openai_completions_backend;
mod openai_responses_backend;

use backend_helpers::{Driver, EventStream};

//
// GenAiClient
//

pub struct GenAiClient {
    driver: Box<dyn Driver>,
    blobs: BlobStore,
}
pub enum Config {
    OpenAiResponses { endpoint: String, api_key: String },
    OpenAiCompletions { endpoint: String, api_key: String },
    Anthropic { endpoint: String, api_key: String },
}
impl GenAiClient {
    pub fn new(config: Config, blobs: BlobStore) -> Result<Self, Error> {
        Ok(Self {
            driver: backend_helpers::driver(config)?,
            blobs,
        })
    }
    pub fn blobs(&self) -> &BlobStore {
        &self.blobs
    }
    pub fn generate(&self, request: Request) -> Result<Generation<'_>, Error> {
        backend_helpers::generate(self.driver.as_ref(), request, &self.blobs)
    }
}

//
// Generation
//

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

//
// Requests and messages
//

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Request {
    pub model: String,
    pub instructions: String,
    pub messages: Vec<Message>,
    pub tools: Vec<Tool>,
    pub max_output_tokens: u32,
    pub driver_options: Map<String, Value>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Message {
    pub kind: MessageKind,
}

impl Message {
    pub fn new(kind: MessageKind) -> Self {
        Self { kind }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum MessageKind {
    User {
        content: Vec<InputContentPart>,
    },
    Assistant {
        content: Vec<ContentPart>,
    },
    ToolResult {
        call_id: String,
        content: Vec<InputContentPart>,
        is_error: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputContentPart {
    Text { content: String },
    Image { blob: BlobRef },
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
    pub provider_id: Option<String>,
    pub name: String,
    pub arguments: Result<Value, String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContentPart {
    Text(String),
    Reasoning {
        text: String,
        signature: Option<String>,
    },
    EncryptedReasoning {
        provider_id: String,
        summary: Vec<String>,
        data: String,
    },
    RedactedReasoning(String),
    Refusal(String),
    ToolCall(ToolCall),
}

//
// Completion
//

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

//
// Events
//

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeltaKind {
    Text,
    Reasoning,
    Refusal,
    ToolArguments,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Delta {
    /// Output-item or content-block index; Chat Completions reserves zero for message content.
    pub index: usize,
    pub part: usize,
    pub kind: DeltaKind,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    Request {
        body: Value,
    },
    Progress {
        raw: Value,
    },
    Delta(Delta),
    Completed {
        message: Message,
        finish: Finish,
        usage: Usage,
    },
}

//
// Errors
//

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Blob(#[from] BlobError),
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
