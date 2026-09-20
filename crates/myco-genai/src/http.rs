use std::{
    collections::{HashSet, VecDeque},
    time::Duration,
};

use futures::{StreamExt, stream};
use reqwest::{
    Client, Url,
    header::{HeaderMap, HeaderValue},
};
use serde_json::Value;

use crate::{Error, Event, GenerationStream, Message, Model, Output, Protocol, Request, Response};
use crate::{anthropic, openai, sse::Sse, types::field};

/// Reusable transport. The endpoint is the complete URL, including `/responses`
/// or `/messages`; credentials and model selection are supplied by the caller.
#[derive(Clone)]
pub struct HttpModel {
    protocol: Protocol,
    endpoint: Url,
    client: Client,
}

impl HttpModel {
    pub fn new(protocol: Protocol, endpoint: &str, api_key: &str) -> Result<Self, Error> {
        let endpoint = Url::parse(endpoint)
            .map_err(|e| Error::InvalidRequest(format!("invalid endpoint: {e}")))?;
        if !matches!(endpoint.scheme(), "http" | "https") {
            return Err(Error::InvalidRequest(
                "endpoint must use HTTP or HTTPS".into(),
            ));
        }
        let mut headers = HeaderMap::new();
        headers.insert("accept", HeaderValue::from_static("text/event-stream"));
        if !api_key.is_empty() {
            let (name, value) = match protocol {
                Protocol::OpenAiResponses => ("authorization", format!("Bearer {api_key}")),
                Protocol::AnthropicMessages => ("x-api-key", api_key.into()),
            };
            let mut value = HeaderValue::from_str(&value).map_err(|_| {
                Error::InvalidRequest("API key is not a valid HTTP header value".into())
            })?;
            value.set_sensitive(true);
            headers.insert(name, value);
        }
        if protocol == Protocol::AnthropicMessages {
            headers.insert("anthropic-version", HeaderValue::from_static("2023-06-01"));
        }
        let client = Client::builder()
            .default_headers(headers)
            .connect_timeout(Duration::from_secs(30))
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .build()?;
        Ok(Self {
            protocol,
            endpoint,
            client,
        })
    }

    /// Inspect the exact provider payload without opening a connection.
    pub fn request_body(&self, request: &Request) -> Result<Value, Error> {
        if request.model.is_empty() || request.messages.is_empty() || request.max_output_tokens == 0
        {
            return Err(Error::InvalidRequest(
                "model, messages, and a positive output limit are required".into(),
            ));
        }
        let mut names = HashSet::new();
        for tool in &request.tools {
            if tool.name.is_empty() || !names.insert(&tool.name) || !tool.parameters.is_object() {
                return Err(Error::InvalidRequest(
                    "tools require unique nonempty names and object schemas".into(),
                ));
            }
        }
        let mut calls = HashSet::new();
        let mut results = HashSet::new();
        for message in &request.messages {
            match message {
                Message::Assistant(response) => {
                    if response
                        .provider()
                        .is_some_and(|p| p.protocol != self.protocol)
                    {
                        return Err(Error::InvalidRequest(
                            "assistant continuation belongs to another provider protocol".into(),
                        ));
                    }
                    for output in response.output() {
                        if let Output::ToolCall(call) = output {
                            call.validate()
                                .map_err(|e| Error::InvalidRequest(e.to_string()))?;
                            if !calls.insert(&call.id) {
                                return Err(Error::InvalidRequest(format!(
                                    "duplicate tool call ID {}",
                                    call.id
                                )));
                            }
                        }
                    }
                }
                Message::ToolResult { call_id, .. } => {
                    if !calls.contains(call_id) || !results.insert(call_id) {
                        return Err(Error::InvalidRequest(format!(
                            "unmatched or duplicate tool result {call_id}"
                        )));
                    }
                }
                Message::User(_) => {}
            }
        }
        if calls.len() != results.len() {
            return Err(Error::InvalidRequest(
                "history has tool calls without results".into(),
            ));
        }
        let mut body = match self.protocol {
            Protocol::OpenAiResponses => openai::request(request)?,
            Protocol::AnthropicMessages => anthropic::request(request)?,
        };
        for (name, value) in &request.provider_options {
            if matches!(
                name.as_str(),
                "model"
                    | "input"
                    | "messages"
                    | "instructions"
                    | "system"
                    | "tools"
                    | "max_tokens"
                    | "max_output_tokens"
                    | "stream"
                    | "store"
                    | "include"
                    | "previous_response_id"
                    | "conversation"
                    | "background"
            ) {
                return Err(Error::InvalidRequest(format!(
                    "provider option {name} overrides a managed request field"
                )));
            }
            body[name] = value.clone();
        }
        Ok(body)
    }
}

impl Model for HttpModel {
    fn generate(&self, request: Request) -> GenerationStream {
        let prepared = self.request_body(&request).and_then(|body| {
            let request = self
                .client
                .post(self.endpoint.clone())
                .json(&body)
                .build()?;
            Ok((body, request))
        });
        let (body, request) = match prepared {
            Ok(prepared) => prepared,
            Err(error) => return stream::once(async { Err(error) }).boxed(),
        };
        let state = State {
            client: self.client.clone(),
            protocol: self.protocol,
            request: Some(request),
            response: None,
            pending: VecDeque::from([Event::Request {
                protocol: self.protocol,
                body,
            }]),
            frames: VecDeque::new(),
            sse: Sse::default(),
            anthropic: anthropic::Accumulator::default(),
            eof: false,
            finished: false,
            failure: None,
        };
        stream::try_unfold(state, |mut state| async move {
            loop {
                if let Some(event) = state.pending.pop_front() {
                    return Ok(Some((event, state)));
                }
                if let Some(error) = state.failure.take() {
                    return Err(error);
                }
                if state.finished {
                    return Ok(None);
                }
                if let Some(frame) = state.frames.pop_front() {
                    if frame.is_empty() {
                        continue;
                    }
                    let event: Value = serde_json::from_str(&frame)
                        .map_err(|e| Error::Protocol(format!("invalid SSE JSON: {e}")))?;
                    let (delta, completed) = match state.event(&event) {
                        Ok(decoded) => decoded,
                        Err(error) => {
                            state.failure = Some(error);
                            state.finished = true;
                            (None, None)
                        }
                    };
                    // Keep the offending provider event observable before an
                    // error ends the stream, including malformed final output.
                    state
                        .pending
                        .push_back(Event::Progress { raw: event, delta });
                    if let Some(response) = completed {
                        state.pending.push_back(Event::Completed(response));
                        state.finished = true;
                    }
                    if state.finished {
                        state.response = None;
                        state.frames.clear();
                    }
                    continue;
                }
                if state.eof {
                    return Err(Error::Protocol(
                        "stream ended before its terminal event".into(),
                    ));
                }
                if let Some(request) = state.request.take() {
                    let response = state.client.execute(request).await?;
                    if !response.status().is_success() {
                        let status = response.status().as_u16();
                        let header = |name| {
                            response
                                .headers()
                                .get(name)
                                .and_then(|v| v.to_str().ok())
                                .map(str::to_owned)
                        };
                        let request_id = header("x-request-id").or_else(|| header("request-id"));
                        let retry_after = header("retry-after");
                        return Err(Error::Http {
                            status,
                            body: response.text().await?,
                            request_id,
                            retry_after,
                        });
                    }
                    let is_sse = response
                        .headers()
                        .get("content-type")
                        .and_then(|v| v.to_str().ok())
                        .is_some_and(|v| {
                            v.split(';')
                                .next()
                                .is_some_and(|v| v.trim().eq_ignore_ascii_case("text/event-stream"))
                        });
                    if !is_sse {
                        return Err(Error::Protocol(
                            "expected text/event-stream content type".into(),
                        ));
                    }
                    state.response = Some(response);
                }
                let chunk = state
                    .response
                    .as_mut()
                    .expect("request opened")
                    .chunk()
                    .await?;
                state.eof = chunk.is_none();
                state.frames.extend(
                    state
                        .sse
                        .push(chunk.as_deref().unwrap_or_default(), state.eof)?,
                );
            }
        })
        .boxed()
    }
}

struct State {
    client: Client,
    protocol: Protocol,
    request: Option<reqwest::Request>,
    response: Option<reqwest::Response>,
    pending: VecDeque<Event>,
    frames: VecDeque<String>,
    sse: Sse,
    anthropic: anthropic::Accumulator,
    eof: bool,
    finished: bool,
    failure: Option<Error>,
}

impl State {
    fn event(&mut self, event: &Value) -> Result<(Option<crate::Delta>, Option<Response>), Error> {
        let (delta, body) = match self.protocol {
            Protocol::OpenAiResponses => match field(event, "type")? {
                "error" | "response.failed" => return Err(Error::Provider(event.clone())),
                "response.completed" | "response.incomplete" => {
                    let body = event
                        .get("response")
                        .ok_or_else(|| Error::Protocol("missing final response".into()))?;
                    let expected = if event["type"] == "response.completed" {
                        "completed"
                    } else {
                        "incomplete"
                    };
                    if body["status"] != expected {
                        return Err(Error::Protocol(
                            "terminal event disagrees with response status".into(),
                        ));
                    }
                    (None, Some(body.clone()))
                }
                _ => (openai::delta(event)?, None),
            },
            Protocol::AnthropicMessages => self.anthropic.event(event)?,
        };
        Ok((
            delta,
            body.map(|body| Response::from_provider(self.protocol, body))
                .transpose()?,
        ))
    }
}
