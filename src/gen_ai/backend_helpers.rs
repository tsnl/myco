use std::{
    collections::{HashMap, HashSet},
    pin::Pin,
    task::{Context, Poll},
};

use futures_core::Stream;
use serde_json::{Map, Value, json};

use super::{
    Config, ContentPart, Delta, Error, Event, Finish, Generation, InputContentPart, Message,
    MessageKind, Request, ToolCall, Usage, anthropic_backend, openai_responses_backend,
};

//
// Driver
//

pub(super) type EventStream<'a> = Pin<Box<dyn Stream<Item = Result<Event, Error>> + Send + 'a>>;

pub(super) trait Driver: Send + Sync {
    fn encode(&self, request: &Request) -> Result<Value, Error>;
    fn generate(&self, body: Value) -> EventStream<'_>;
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Protocol {
    OpenAiResponses,
    AnthropicMessages,
}

impl Protocol {
    fn namespace(self) -> &'static str {
        match self {
            Self::OpenAiResponses => "openai.responses",
            Self::AnthropicMessages => "anthropic.messages",
        }
    }
}

pub(super) enum Decoded {
    Progress(Option<Delta>),
    Completed(Value),
}

pub(super) struct Completion {
    pub(super) output: Vec<ContentPart>,
    pub(super) finish: Finish,
    pub(super) usage: Usage,
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

//
// Generation
//

pub(super) fn generate(driver: &dyn Driver, request: Request) -> Result<Generation<'_>, Error> {
    validate(&request)?;
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
        Poll::Ready(None | Some(Err(_)) | Some(Ok(Event::Completed { .. })))
    ) {
        generation.inner = None;
    }
    next
}

pub(super) fn completed(protocol: Protocol, body: &Value) -> Result<Event, Error> {
    let completion = match protocol {
        Protocol::OpenAiResponses => openai_responses_backend::decode_response(body)?,
        Protocol::AnthropicMessages => anthropic_backend::decode_response(body)?,
    };
    validate_call_ids(&completion.output)?;
    Ok(Event::Completed {
        message: portable_message(protocol, completion.output),
        finish: completion.finish,
        usage: completion.usage,
    })
}

//
// Provider metadata and tool identities
//

fn portable_message(protocol: Protocol, mut content: Vec<ContentPart>) -> Message {
    let mut ids = Map::new();
    for part in &mut content {
        if let ContentPart::ToolCall(call) = part {
            let native = std::mem::replace(&mut call.id, uuid::Uuid::new_v4().to_string());
            ids.insert(call.id.clone(), native.into());
        }
    }
    let mut message = Message::new(MessageKind::Assistant { content });
    if !ids.is_empty() {
        message.provider_info.insert(
            protocol.namespace().into(),
            json!({"version":1,"tool_call_ids":ids}),
        );
    }
    message
}

pub(super) fn wire_messages(
    protocol: Protocol,
    input: &[Message],
) -> Result<Vec<MessageKind>, Error> {
    let ids = wire_call_ids(protocol, input)?;
    Ok(input
        .iter()
        .map(|message| wire_message(&message.kind, &ids))
        .collect())
}

fn wire_message(kind: &MessageKind, ids: &HashMap<String, String>) -> MessageKind {
    let mut kind = kind.clone();
    match &mut kind {
        MessageKind::Assistant { content } => {
            for part in content {
                if let ContentPart::ToolCall(call) = part {
                    call.id = ids[&call.id].clone();
                }
            }
        }
        MessageKind::ToolResult { call_id, .. } => *call_id = ids[call_id].clone(),
        MessageKind::User { .. } => {}
    }
    kind
}

fn wire_call_ids(protocol: Protocol, input: &[Message]) -> Result<HashMap<String, String>, Error> {
    let mut ids = HashMap::new();
    let mut used = HashSet::new();
    for (index, message) in input.iter().enumerate() {
        let native = native_ids(message, protocol)?;
        if let MessageKind::Assistant { content } = &message.kind {
            assign_call_ids(index, content, native, &mut ids, &mut used)?;
        }
    }
    Ok(ids)
}

fn native_ids(message: &Message, protocol: Protocol) -> Result<Option<&Map<String, Value>>, Error> {
    let Some(info) = message.provider_info.get(protocol.namespace()) else {
        return Ok(None);
    };
    if info["version"] != 1 {
        return Err(Error::InvalidRequest(
            "unsupported provider metadata version".into(),
        ));
    }
    info.get("tool_call_ids")
        .map(|ids| {
            ids.as_object().ok_or_else(|| {
                Error::InvalidRequest("provider tool_call_ids must be an object".into())
            })
        })
        .transpose()
}

fn assign_call_ids(
    message: usize,
    content: &[ContentPart],
    native: Option<&Map<String, Value>>,
    ids: &mut HashMap<String, String>,
    used: &mut HashSet<String>,
) -> Result<(), Error> {
    for (part, content) in content.iter().enumerate() {
        if let ContentPart::ToolCall(call) = content {
            let preferred = native
                .and_then(|ids| ids.get(&call.id))
                .map(native_id)
                .transpose()?;
            let wire = unique_wire_id(preferred, message, part, used);
            ids.insert(call.id.clone(), wire);
        }
    }
    Ok(())
}

fn native_id(value: &Value) -> Result<&str, Error> {
    value
        .as_str()
        .filter(|id| {
            !id.is_empty()
                && id
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-'))
        })
        .ok_or_else(|| Error::InvalidRequest("invalid provider tool call ID".into()))
}

fn unique_wire_id(
    preferred: Option<&str>,
    message: usize,
    part: usize,
    used: &mut HashSet<String>,
) -> String {
    if let Some(id) = preferred.filter(|id| !used.contains(*id)) {
        used.insert(id.into());
        return id.into();
    }
    let base = format!("call_{message}_{part}");
    let mut id = base.clone();
    let mut suffix = 0;
    while !used.insert(id.clone()) {
        suffix += 1;
        id = format!("{base}_{suffix}");
    }
    id
}

//
// Validation
//

fn validate_call_ids(output: &[ContentPart]) -> Result<(), Error> {
    let mut calls = HashSet::new();
    for output in output {
        if let ContentPart::ToolCall(call) = output
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

fn validate(request: &Request) -> Result<(), Error> {
    if request.model.is_empty() || request.messages.is_empty() || request.max_output_tokens == 0 {
        return Err(Error::InvalidRequest(
            "model, messages, and a positive output limit are required".into(),
        ));
    }
    validate_tools(request)?;
    validate_history(&request.messages)
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

fn validate_history(messages: &[Message]) -> Result<(), Error> {
    let mut history = History::default();
    for message in messages {
        history.message(message)?;
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
    fn message(&mut self, message: &'a Message) -> Result<(), Error> {
        match &message.kind {
            MessageKind::Assistant { content } => self.assistant(content),
            MessageKind::ToolResult {
                call_id, content, ..
            } => {
                validate_input(content)?;
                self.result(call_id)
            }
            MessageKind::User { content } => validate_input(content),
        }
    }

    fn assistant(&mut self, content: &'a [ContentPart]) -> Result<(), Error> {
        if self.calls.len() != self.results.len() {
            return Err(Error::InvalidRequest(
                "assistant turn interrupts an unanswered tool batch".into(),
            ));
        }
        for part in content {
            if let ContentPart::ToolCall(call) = part {
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

fn validate_input(content: &[InputContentPart]) -> Result<(), Error> {
    for part in content {
        if let InputContentPart::Image { media_type, data } = part
            && (data.is_empty()
                || !matches!(
                    media_type.as_str(),
                    "image/png" | "image/jpeg" | "image/gif" | "image/webp"
                ))
        {
            return Err(Error::InvalidRequest(
                "images require bytes and a supported image media type".into(),
            ));
        }
    }
    Ok(())
}

//
// Driver options
//

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

//
// JSON fields
//

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
