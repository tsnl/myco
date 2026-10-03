use std::collections::BTreeMap;

use base64::Engine as _;
use serde_json::{Value, json};

use crate::blob::{BlobRef, BlobStore};

use super::backend_helpers::{
    Completion, Decoded, Driver, EventStream, Protocol, array, field, image_blob, index,
    wire_messages,
};
use super::http_helpers::{Frame, Transport};
use super::{
    ContentPart, Delta, DeltaKind, Error, Finish, InputContentPart, MessageKind, Request, Tool,
    ToolCall, Usage,
};

//
// Backend
//

pub(super) struct Backend {
    transport: Transport,
}

impl Backend {
    pub(super) fn new(endpoint: &str, api_key: &str) -> Result<Self, Error> {
        Ok(Self {
            transport: Transport::new(Protocol::OpenAiCompletions, endpoint, api_key)?,
        })
    }
}

impl Driver for Backend {
    fn encode(&self, request: &Request, blobs: &BlobStore) -> Result<Value, Error> {
        encode_request(request, blobs)
    }

    fn generate(&self, body: Value) -> EventStream<'_> {
        let mut accumulator = Accumulator::default();
        self.transport
            .generate(body, move |frame| accumulator.event(frame))
    }
}

//
// Request encoding
//

fn encode_request(request: &Request, blobs: &BlobStore) -> Result<Value, Error> {
    let mut body = json!({
        "model": request.model, "messages": messages(request, blobs)?,
        "max_completion_tokens": request.max_output_tokens,
        "stream": true, "store": false, "n": 1,
        "stream_options": {"include_usage": true},
    });
    if !request.tools.is_empty() {
        body["tools"] = request.tools.iter().map(tool).collect();
    }
    Ok(body)
}

fn messages(request: &Request, blobs: &BlobStore) -> Result<Vec<Value>, Error> {
    let mut messages = vec![];
    if !request.instructions.is_empty() {
        messages.push(json!({"role":"system", "content":request.instructions}));
    }
    for message in wire_messages(&request.messages)? {
        messages.extend(encode_message(&message, blobs)?);
    }
    Ok(messages)
}

fn encode_message(message: &MessageKind, blobs: &BlobStore) -> Result<Option<Value>, Error> {
    match message {
        MessageKind::User { content } => Ok(Some(
            json!({"role":"user", "content":input_content(content, blobs)?}),
        )),
        MessageKind::Assistant { content } => assistant(content),
        MessageKind::ToolResult {
            call_id,
            content,
            is_error,
        } => tool_result(call_id, content, *is_error).map(Some),
    }
}

fn assistant(content: &[ContentPart]) -> Result<Option<Value>, Error> {
    let mut parts = vec![];
    let mut calls = vec![];
    for part in content {
        match part {
            ContentPart::ToolCall(call) => calls.push(encode_call(call)?),
            _ => parts.extend(assistant_part(part)?),
        }
    }
    if parts.is_empty() && calls.is_empty() {
        return Ok(None);
    }
    let content = if parts.is_empty() {
        Value::Null
    } else {
        parts.into()
    };
    let mut message = json!({"role":"assistant", "content":content});
    if !calls.is_empty() {
        message["tool_calls"] = calls.into();
    }
    Ok(Some(message))
}

fn assistant_part(part: &ContentPart) -> Result<Option<Value>, Error> {
    Ok(match part {
        ContentPart::Text(text) => Some(json!({"type":"text", "text":text})),
        ContentPart::Refusal(text) => Some(json!({"type":"refusal", "refusal":text})),
        ContentPart::Reasoning {
            signature: None, ..
        } => None,
        _ => {
            return Err(Error::InvalidRequest(
                "Chat Completions cannot replay signed or encrypted reasoning".into(),
            ));
        }
    })
}

fn encode_call(call: &ToolCall) -> Result<Value, Error> {
    Ok(json!({"id":call.id, "type":"function", "function":{
        "name":call.name, "arguments":call.arguments()?.to_string(),
    }}))
}

fn tool_result(id: &str, content: &[InputContentPart], is_error: bool) -> Result<Value, Error> {
    let mut parts = content
        .iter()
        .map(tool_text)
        .collect::<Result<Vec<_>, _>>()?;
    if is_error {
        parts.insert(0, json!({"type":"text", "text":"Tool error:"}));
    }
    Ok(json!({"role":"tool", "tool_call_id":id, "content":parts}))
}

fn tool_text(part: &InputContentPart) -> Result<Value, Error> {
    match part {
        InputContentPart::Text { content } => Ok(json!({"type":"text", "text":content})),
        InputContentPart::Image { .. } => Err(Error::InvalidRequest(
            "Chat Completions tool results support only text".into(),
        )),
    }
}

fn input_content(content: &[InputContentPart], blobs: &BlobStore) -> Result<Value, Error> {
    if let [InputContentPart::Text { content }] = content {
        return Ok(content.clone().into());
    }
    content
        .iter()
        .map(|part| match part {
            InputContentPart::Text { content } => Ok(json!({"type":"text", "text":content})),
            InputContentPart::Image { blob } => image(*blob, blobs),
        })
        .collect()
}

fn image(reference: BlobRef, blobs: &BlobStore) -> Result<Value, Error> {
    let blob = image_blob(reference, blobs)?;
    let encoded = base64::engine::general_purpose::STANDARD.encode(&blob.data);
    let url = format!("data:{};base64,{encoded}", blob.media_type.as_str());
    Ok(json!({"type":"image_url", "image_url":{"url":url, "detail":"auto"}}))
}

fn tool(tool: &Tool) -> Value {
    json!({"type":"function", "function":{
        "name":tool.name, "description":tool.description,
        "parameters":tool.parameters, "strict":false,
    }})
}

//
// Streaming
//

#[derive(Default)]
struct Accumulator {
    id: Option<String>,
    text: Option<String>,
    refusal: Option<String>,
    calls: BTreeMap<usize, Call>,
    finish: Option<String>,
    usage: Value,
}

impl Accumulator {
    fn event(&mut self, frame: &Frame) -> Result<Decoded, Error> {
        match frame {
            Frame::Json(event) => self.chunk(event).map(Decoded::Progress),
            Frame::Done => self.complete().map(Decoded::Completed),
        }
    }

    fn chunk(&mut self, event: &Value) -> Result<Vec<Delta>, Error> {
        self.identity(event)?;
        self.usage(event)?;
        let choices = array(event, "choices")?;
        if choices.is_empty() && self.finish.is_some() && event["usage"].is_object() {
            return Ok(vec![]);
        }
        if choices.len() != 1 || self.finish.is_some() || index(&choices[0], "index")? != 0 {
            return Err(Error::Protocol(
                "expected one unfinished completion choice".into(),
            ));
        }
        let deltas = self.delta(&choices[0]["delta"])?;
        self.finish = optional_text(&choices[0], "finish_reason")?.map(str::to_owned);
        Ok(deltas)
    }

    fn identity(&mut self, event: &Value) -> Result<(), Error> {
        if event.get("error").is_some_and(|error| !error.is_null()) {
            return Err(Error::Provider(event.clone()));
        }
        let id = field(event, "id")?;
        if id.is_empty()
            || event["object"] != "chat.completion.chunk"
            || self.id.as_deref().is_some_and(|previous| previous != id)
        {
            return Err(Error::Protocol(
                "invalid or changed completion identity".into(),
            ));
        }
        self.id.get_or_insert_with(|| id.into());
        Ok(())
    }

    fn usage(&mut self, event: &Value) -> Result<(), Error> {
        if let Some(usage) = event.get("usage").filter(|usage| !usage.is_null()) {
            if !usage.is_object() {
                return Err(Error::Protocol("usage must be an object".into()));
            }
            self.usage = usage.clone();
        }
        Ok(())
    }

    fn delta(&mut self, delta: &Value) -> Result<Vec<Delta>, Error> {
        validate_delta(delta)?;
        let mut deltas = vec![];
        deltas.extend(text_delta(
            &mut self.text,
            delta,
            "content",
            DeltaKind::Text,
            0,
        )?);
        deltas.extend(text_delta(
            &mut self.refusal,
            delta,
            "refusal",
            DeltaKind::Refusal,
            1,
        )?);
        if let Some(calls) = delta.get("tool_calls").filter(|calls| !calls.is_null()) {
            let calls = calls
                .as_array()
                .ok_or_else(|| Error::Protocol("invalid tool deltas".into()))?;
            for call in calls {
                let index = index(call, "index")?;
                deltas.extend(self.calls.entry(index).or_default().delta(index, call)?);
            }
        }
        Ok(deltas)
    }
}

fn validate_delta(delta: &Value) -> Result<(), Error> {
    if !delta.is_object() || optional_text(delta, "role")?.is_some_and(|role| role != "assistant") {
        return Err(Error::Protocol("expected an assistant delta".into()));
    }
    if ["function_call", "audio"]
        .iter()
        .any(|key| delta.get(key).is_some_and(|v| !v.is_null()))
    {
        return Err(Error::Protocol("unsupported completion delta".into()));
    }
    Ok(())
}

fn text_delta(
    accumulated: &mut Option<String>,
    delta: &Value,
    name: &str,
    kind: DeltaKind,
    part: usize,
) -> Result<Option<Delta>, Error> {
    let Some(text) = optional_text(delta, name)? else {
        return Ok(None);
    };
    accumulated.get_or_insert_with(String::new).push_str(text);
    Ok(Some(Delta {
        index: 0,
        part,
        kind,
        text: text.into(),
    }))
}

//
// Tool calls
//

#[derive(Default)]
struct Call {
    id: String,
    name: String,
    arguments: String,
}

impl Call {
    fn delta(&mut self, index: usize, delta: &Value) -> Result<Option<Delta>, Error> {
        if optional_text(delta, "type")?.is_some_and(|kind| kind != "function") {
            return Err(Error::Protocol("unsupported tool call type".into()));
        }
        self.id
            .push_str(optional_text(delta, "id")?.unwrap_or_default());
        let Some(function) = delta.get("function") else {
            return Ok(None);
        };
        if !function.is_object() {
            return Err(Error::Protocol("expected a function delta".into()));
        }
        self.name
            .push_str(optional_text(function, "name")?.unwrap_or_default());
        self.arguments(index, function)
    }

    fn arguments(&mut self, index: usize, function: &Value) -> Result<Option<Delta>, Error> {
        let Some(text) = optional_text(function, "arguments")? else {
            return Ok(None);
        };
        self.arguments.push_str(text);
        let index = index
            .checked_add(1)
            .ok_or_else(|| Error::Protocol("tool index overflow".into()))?;
        Ok(Some(Delta {
            index,
            part: 0,
            kind: DeltaKind::ToolArguments,
            text: text.into(),
        }))
    }

    fn complete(&self, truncated: bool) -> Result<ContentPart, Error> {
        let call = ToolCall {
            id: self.id.clone(),
            provider_id: None,
            name: self.name.clone(),
            arguments: serde_json::from_str(&self.arguments).map_err(|error| error.to_string()),
        };
        if !truncated {
            call.validate()?;
        }
        Ok(ContentPart::ToolCall(call))
    }
}

//
// Completion
//

impl Accumulator {
    fn complete(&self) -> Result<Completion, Error> {
        let reason = self
            .finish
            .as_deref()
            .filter(|reason| !reason.is_empty())
            .ok_or_else(|| Error::Protocol("[DONE] before finish_reason".into()))?;
        if reason == "tool_calls" && self.calls.is_empty() {
            return Err(Error::Protocol("tool_calls finish without calls".into()));
        }
        let output = self.output(matches!(reason, "length" | "content_filter"))?;
        Ok(Completion {
            finish: finish(reason, &output),
            output,
            usage: usage(&self.usage),
        })
    }

    fn output(&self, truncated: bool) -> Result<Vec<ContentPart>, Error> {
        let mut output = vec![];
        output.extend(self.text.clone().map(ContentPart::Text));
        output.extend(self.refusal.clone().map(ContentPart::Refusal));
        for (expected, (index, call)) in self.calls.iter().enumerate() {
            if *index != expected {
                return Err(Error::Protocol("noncontiguous tool call indices".into()));
            }
            output.push(call.complete(truncated)?);
        }
        Ok(output)
    }
}

fn finish(reason: &str, output: &[ContentPart]) -> Finish {
    match reason {
        "length" => Finish::Length,
        "content_filter" => Finish::Refusal,
        "tool_calls" => Finish::ToolCalls,
        "stop"
            if output
                .iter()
                .any(|part| matches!(part, ContentPart::Refusal(_))) =>
        {
            Finish::Refusal
        }
        "stop" => Finish::Stop,
        reason => Finish::Other(reason.into()),
    }
}

fn usage(usage: &Value) -> Usage {
    Usage {
        input_tokens: usage["prompt_tokens"].as_u64(),
        output_tokens: usage["completion_tokens"].as_u64(),
        cache_read_tokens: usage["prompt_tokens_details"]["cached_tokens"].as_u64(),
        cache_write_tokens: None,
    }
}

fn optional_text<'a>(value: &'a Value, name: &str) -> Result<Option<&'a str>, Error> {
    match value.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_str()
            .map(Some)
            .ok_or_else(|| Error::Protocol(format!("invalid string field {name}"))),
    }
}
