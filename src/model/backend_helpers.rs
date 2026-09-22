use std::{
    collections::HashSet,
    pin::Pin,
    task::{Context, Poll},
};

use futures_core::Stream;
use serde_json::Value;

use super::{
    Config, Delta, Error, Event, Generation, Message, Output, Protocol, ProviderResponse, Request,
    Response, ToolCall, anthropic_backend, openai_responses_backend,
};

pub(super) type EventStream<'a> = Pin<Box<dyn Stream<Item = Result<Event, Error>> + Send + 'a>>;

pub(super) trait Driver: Send + Sync {
    fn protocol(&self) -> Protocol;
    fn encode(&self, request: &Request) -> Result<Value, Error>;
    fn generate(&self, body: Value) -> EventStream<'_>;
}

pub(super) enum Decoded {
    Progress(Option<Delta>),
    Completed(Value),
}

impl Decoded {
    pub(super) fn delta(&self) -> Option<Delta> {
        match self {
            Self::Progress(delta) => delta.clone(),
            Self::Completed(_) => None,
        }
    }
}

pub(super) fn driver(config: Config) -> Result<Box<dyn Driver>, Error> {
    Ok(match config {
        Config::OpenAi { endpoint, api_key } => {
            Box::new(openai_responses_backend::Backend::new(&endpoint, &api_key)?)
        }
        Config::Anthropic { endpoint, api_key } => {
            Box::new(anthropic_backend::Backend::new(&endpoint, &api_key)?)
        }
    })
}

pub(super) fn generate(driver: &dyn Driver, request: Request) -> Result<Generation<'_>, Error> {
    validate(&request, driver.protocol())?;
    let mut body = driver.encode(&request)?;
    apply_options(&mut body, &request)?;
    Ok(Generation {
        inner: Some(driver.generate(body)),
    })
}

pub(super) fn poll_generation(
    generation: &mut Generation<'_>,
    cx: &mut Context<'_>,
) -> Poll<Option<Result<Event, Error>>> {
    let Some(inner) = &mut generation.inner else {
        return Poll::Ready(None);
    };
    let next = inner.as_mut().poll_next(cx);
    if matches!(
        &next,
        Poll::Ready(None | Some(Err(_)) | Some(Ok(Event::Completed(_))))
    ) {
        generation.inner = None;
    }
    next
}

pub(super) fn from_provider(protocol: Protocol, body: Value) -> Result<Response, Error> {
    let mut response = match protocol {
        Protocol::OpenAiResponses => openai_responses_backend::decode_response(&body)?,
        Protocol::AnthropicMessages => anthropic_backend::decode_response(&body)?,
    };
    response.validate_call_ids()?;
    response.provider = Some(ProviderResponse { protocol, body });
    Ok(response)
}

impl Response {
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
}

impl ToolCall {
    pub(super) fn validate(&self) -> Result<(), Error> {
        if self.id.is_empty() || self.name.is_empty() {
            return Err(Error::Protocol("tool call has an empty ID or name".into()));
        }
        if !self.arguments()?.is_object() {
            return Err(Error::Protocol(format!(
                "arguments for tool call {} must be an object",
                self.id
            )));
        }
        Ok(())
    }

    pub(super) fn arguments(&self) -> Result<&Value, Error> {
        self.arguments.as_ref().map_err(|error| {
            Error::Protocol(format!(
                "invalid arguments for tool call {}: {error}",
                self.id
            ))
        })
    }
}

fn validate(request: &Request, protocol: Protocol) -> Result<(), Error> {
    if request.model.is_empty() || request.messages.is_empty() || request.max_output_tokens == 0 {
        return Err(Error::InvalidRequest(
            "model, messages, and a positive output limit are required".into(),
        ));
    }
    validate_tools(request)?;
    validate_history(&request.messages, protocol)
}

fn validate_tools(request: &Request) -> Result<(), Error> {
    let mut names = HashSet::new();
    for tool in &request.tools {
        if tool.name.is_empty() || !names.insert(&tool.name) || !tool.parameters.is_object() {
            return Err(Error::InvalidRequest(
                "tools require unique nonempty names and object schemas".into(),
            ));
        }
    }
    Ok(())
}

fn validate_history(messages: &[Message], protocol: Protocol) -> Result<(), Error> {
    let mut history = History::default();
    for message in messages {
        history.message(message, protocol)?;
    }
    if history.calls.len() != history.results.len() {
        return Err(Error::InvalidRequest(
            "history has tool calls without results".into(),
        ));
    }
    Ok(())
}

#[derive(Default)]
struct History<'a> {
    calls: HashSet<&'a str>,
    results: HashSet<&'a str>,
}

impl<'a> History<'a> {
    fn message(&mut self, message: &'a Message, protocol: Protocol) -> Result<(), Error> {
        match message {
            Message::Assistant(response) => self.assistant(response, protocol),
            Message::ToolResult { call_id, .. } => self.result(call_id),
            Message::User(_) => Ok(()),
        }
    }

    fn assistant(&mut self, response: &'a Response, protocol: Protocol) -> Result<(), Error> {
        if response.provider().is_some_and(|p| p.protocol != protocol) {
            return Err(Error::InvalidRequest(
                "assistant continuation belongs to another provider protocol".into(),
            ));
        }
        for output in response.output() {
            if let Output::ToolCall(call) = output {
                self.call(call)?;
            }
        }
        Ok(())
    }

    fn call(&mut self, call: &'a ToolCall) -> Result<(), Error> {
        call.validate()
            .map_err(|e| Error::InvalidRequest(e.to_string()))?;
        if !self.calls.insert(&call.id) {
            return Err(Error::InvalidRequest(format!(
                "duplicate tool call ID {}",
                call.id
            )));
        }
        Ok(())
    }

    fn result(&mut self, id: &'a str) -> Result<(), Error> {
        if !self.calls.contains(id) || !self.results.insert(id) {
            return Err(Error::InvalidRequest(format!(
                "unmatched or duplicate tool result {id}"
            )));
        }
        Ok(())
    }
}

fn apply_options(body: &mut Value, request: &Request) -> Result<(), Error> {
    for (name, value) in &request.driver_options {
        if managed_field(name) {
            return Err(Error::InvalidRequest(format!(
                "driver option {name} overrides a managed request field"
            )));
        }
        body[name] = value.clone();
    }
    Ok(())
}

fn managed_field(name: &str) -> bool {
    matches!(
        name,
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
    )
}

pub(super) fn field<'a>(value: &'a Value, name: &str) -> Result<&'a str, Error> {
    value
        .get(name)
        .and_then(Value::as_str)
        .ok_or_else(|| Error::Protocol(format!("missing string field {name}")))
}

pub(super) fn array<'a>(value: &'a Value, name: &str) -> Result<&'a Vec<Value>, Error> {
    value
        .get(name)
        .and_then(Value::as_array)
        .ok_or_else(|| Error::Protocol(format!("missing array field {name}")))
}

pub(super) fn index(value: &Value, name: &str) -> Result<usize, Error> {
    value[name]
        .as_u64()
        .and_then(|n| usize::try_from(n).ok())
        .ok_or_else(|| Error::Protocol(format!("missing {name}")))
}
