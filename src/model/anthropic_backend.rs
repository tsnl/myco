use serde_json::{Map, Value, json};

use super::backend_helpers::{
    Completion, Decoded, Driver, EventStream, Protocol, array, field, index,
};
use super::http_helpers::Transport;
use super::{Delta, DeltaKind, Error, Finish, Message, Output, Request, Tool, ToolCall, Usage};

pub(super) struct Backend {
    transport: Transport,
}

impl Backend {
    pub(super) fn new(endpoint: &str, api_key: &str) -> Result<Self, Error> {
        Ok(Self {
            transport: Transport::new(Protocol::AnthropicMessages, endpoint, api_key)?,
        })
    }
}

impl Driver for Backend {
    fn protocol(&self) -> Protocol {
        Protocol::AnthropicMessages
    }

    fn encode(&self, request: &Request) -> Result<Value, Error> {
        encode_request(request)
    }

    fn generate(&self, body: Value) -> EventStream<'_> {
        let mut accumulator = Accumulator::default();
        self.transport
            .generate(self.protocol(), body, move |event| accumulator.event(event))
    }
}

fn encode_request(request: &Request) -> Result<Value, Error> {
    let mut body = json!({"model": request.model, "messages": messages(&request.messages)?,
        "max_tokens": request.max_output_tokens, "stream": true});
    if !request.instructions.is_empty() {
        body["system"] = request.instructions.clone().into();
    }
    if !request.tools.is_empty() {
        body["tools"] = request.tools.iter().map(tool).collect();
    }
    Ok(body)
}

struct Content {
    role: &'static str,
    blocks: Vec<Value>,
    tool_result: bool,
}

fn messages(input: &[Message]) -> Result<Vec<Value>, Error> {
    let mut messages: Vec<Value> = vec![];
    for message in input {
        let content = content(message)?;
        if let Some(last) = messages
            .last_mut()
            .filter(|last| last["role"] == content.role)
        {
            merge(last, content);
        } else {
            messages.push(json!({"role": content.role, "content": content.blocks}));
        }
    }
    Ok(messages)
}

fn content(message: &Message) -> Result<Content, Error> {
    let (role, blocks, tool_result) = match message {
        Message::User(text) => ("user", vec![json!({"type": "text", "text": text})], false),
        Message::Assistant {
            output,
            continuation,
        } => (
            "assistant",
            assistant(output, continuation.as_ref())?,
            false,
        ),
        Message::ToolResult {
            call_id,
            output,
            is_error,
        } => (
            "user",
            vec![json!({
                "type": "tool_result", "tool_use_id": call_id, "content": output, "is_error": is_error,
            })],
            true,
        ),
    };
    Ok(Content {
        role,
        blocks,
        tool_result,
    })
}

fn merge(message: &mut Value, content: Content) {
    let previous = message["content"]
        .as_array_mut()
        .expect("constructed content array");
    if content.tool_result {
        // Tool observations precede ordinary text in the user message.
        let index = previous
            .iter()
            .take_while(|v| v["type"] == "tool_result")
            .count();
        previous.splice(index..index, content.blocks);
    } else {
        previous.extend(content.blocks);
    }
}

fn assistant(outputs: &[Output], continuation: Option<&Value>) -> Result<Vec<Value>, Error> {
    if let Some(continuation) = continuation {
        let body = Protocol::AnthropicMessages.body(continuation)?;
        return Ok(array(body, "content")?.clone());
    }
    outputs.iter().map(output).collect()
}

fn output(output: &Output) -> Result<Value, Error> {
    match output {
        Output::Text(text) | Output::Refusal(text) => Ok(json!({"type": "text", "text": text})),
        Output::ToolCall(call) => encode_tool_call(call),
        Output::Reasoning(_) => Err(Error::InvalidRequest(
            "reasoning continuation requires the original provider response".into(),
        )),
    }
}

fn encode_tool_call(call: &ToolCall) -> Result<Value, Error> {
    let input = call.arguments()?;
    Ok(json!({"type": "tool_use", "id": call.id, "name": call.name, "input": input}))
}

fn tool(tool: &Tool) -> Value {
    json!({"name": tool.name, "description": tool.description, "input_schema": tool.parameters})
}

pub(super) fn decode_response(body: &Value) -> Result<Completion, Error> {
    if field(body, "type")? != "message" {
        return Err(Error::Protocol("expected an Anthropic message".into()));
    }
    let output = array(body, "content")?
        .iter()
        .filter_map(|b| block(b).transpose())
        .collect::<Result<_, _>>()?;
    Ok(Completion {
        output,
        finish: finish(body)?,
        usage: usage(&body["usage"])?,
    })
}

fn finish(body: &Value) -> Result<Finish, Error> {
    Ok(match field(body, "stop_reason")? {
        "end_turn" | "stop_sequence" => Finish::Stop,
        "tool_use" => Finish::ToolCalls,
        "max_tokens" => Finish::Length,
        "refusal" => Finish::Refusal,
        reason => Finish::Other(reason.into()),
    })
}

fn block(block: &Value) -> Result<Option<Output>, Error> {
    Ok(match field(block, "type")? {
        "text" => Some(Output::Text(field(block, "text")?.into())),
        "thinking" => {
            field(block, "signature")?;
            Some(Output::Reasoning(field(block, "thinking")?.into()))
        }
        "tool_use" => Some(Output::ToolCall(decode_tool_call(block)?)),
        _ => None,
    })
}

fn decode_tool_call(block: &Value) -> Result<ToolCall, Error> {
    let input = block
        .get("input")
        .filter(|v| v.is_object())
        .ok_or_else(|| Error::Protocol("tool_use.input must be an object".into()))?;
    let call = ToolCall {
        id: field(block, "id")?.into(),
        name: field(block, "name")?.into(),
        arguments: Ok(input.clone()),
    };
    call.validate()?;
    Ok(call)
}

fn usage(usage: &Value) -> Result<Usage, Error> {
    let read = usage["cache_read_input_tokens"].as_u64();
    let write = usage["cache_creation_input_tokens"].as_u64();
    Ok(Usage {
        input_tokens: input_tokens(usage, read, write)?,
        output_tokens: usage["output_tokens"].as_u64(),
        cache_read_tokens: read,
        cache_write_tokens: write,
    })
}

fn input_tokens(
    usage: &Value,
    read: Option<u64>,
    write: Option<u64>,
) -> Result<Option<u64>, Error> {
    usage["input_tokens"]
        .as_u64()
        .map(|input| {
            input
                .checked_add(read.unwrap_or(0))
                .and_then(|n| n.checked_add(write.unwrap_or(0)))
                .ok_or_else(|| Error::Protocol("input token count overflow".into()))
        })
        .transpose()
}

#[derive(Default)]
struct Accumulator {
    message: Option<Value>,
    blocks: Vec<Block>,
}

impl Accumulator {
    fn event(&mut self, event: &Value) -> Result<Decoded, Error> {
        let delta = match field(event, "type")? {
            "error" => return Err(Error::Provider(event.clone())),
            "message_start" => {
                self.start(event)?;
                None
            }
            "message_delta" => {
                self.metadata(event)?;
                None
            }
            "message_stop" => return self.finish().map(Decoded::Completed),
            "content_block_start" => {
                self.start_block(event)?;
                None
            }
            "content_block_delta" => self.delta(event)?,
            "content_block_stop" => {
                self.block(event)?.stop()?;
                None
            }
            kind if kind.starts_with("content_block_") => {
                self.block(event)?;
                None
            }
            _ => None,
        };
        Ok(Decoded::Progress(delta))
    }

    fn start(&mut self, event: &Value) -> Result<(), Error> {
        if self.message.is_some() {
            return Err(Error::Protocol("duplicate message_start".into()));
        }
        let message = object(event, "message")?;
        if !array(&message, "content")?.is_empty() {
            return Err(Error::Protocol(
                "message_start already contains content".into(),
            ));
        }
        self.message = Some(message);
        Ok(())
    }

    fn require_message(&self) -> Result<(), Error> {
        self.message
            .as_ref()
            .map(|_| ())
            .ok_or_else(|| Error::Protocol("content before message_start".into()))
    }

    fn start_block(&mut self, event: &Value) -> Result<(), Error> {
        self.require_message()?;
        if index(event, "index")? != self.blocks.len() {
            return Err(Error::Protocol(
                "duplicate or out-of-order content block start".into(),
            ));
        }
        self.blocks
            .push(Block::new(object(event, "content_block")?)?);
        Ok(())
    }

    fn block(&mut self, event: &Value) -> Result<&mut Block, Error> {
        self.require_message()?;
        self.blocks
            .get_mut(index(event, "index")?)
            .filter(|b| b.open)
            .ok_or_else(|| {
                Error::Protocol("content event references an absent or closed block".into())
            })
    }

    fn delta(&mut self, event: &Value) -> Result<Option<Delta>, Error> {
        self.block(event)?
            .delta(index(event, "index")?, &event["delta"])
    }

    fn metadata(&mut self, event: &Value) -> Result<(), Error> {
        let message = self
            .message
            .as_mut()
            .ok_or_else(|| Error::Protocol("message_delta before message_start".into()))?;
        let delta = metadata_delta(event)?;
        message
            .as_object_mut()
            .expect("checked at start")
            .extend(delta.clone());
        if let Some(usage) = event.get("usage") {
            update_usage(message, usage)?;
        }
        Ok(())
    }

    fn finish(&mut self) -> Result<Value, Error> {
        if self.blocks.iter().any(|b| b.open) {
            return Err(Error::Protocol(
                "message stopped with an open content block".into(),
            ));
        }
        let mut message = self
            .message
            .take()
            .ok_or_else(|| Error::Protocol("message_stop before message_start".into()))?;
        message["content"] = self.blocks.drain(..).map(|b| b.value).collect();
        Ok(message)
    }
}

fn object(event: &Value, name: &str) -> Result<Value, Error> {
    event
        .get(name)
        .filter(|v| v.is_object())
        .cloned()
        .ok_or_else(|| Error::Protocol(format!("missing {name} object")))
}

fn metadata_delta(event: &Value) -> Result<&Map<String, Value>, Error> {
    let delta = event["delta"]
        .as_object()
        .ok_or_else(|| Error::Protocol("missing message delta".into()))?;
    // Identity and content come from their own events, never a metadata patch.
    if delta
        .keys()
        .any(|k| matches!(k.as_str(), "content" | "id" | "type" | "role"))
    {
        return Err(Error::Protocol(
            "message delta replaces identity or content".into(),
        ));
    }
    Ok(delta)
}

fn update_usage(message: &mut Value, usage: &Value) -> Result<(), Error> {
    let usage = usage
        .as_object()
        .ok_or_else(|| Error::Protocol("usage must be an object".into()))?;
    if message.get("usage").is_none() {
        message["usage"] = json!({});
    }
    message["usage"]
        .as_object_mut()
        .ok_or_else(|| Error::Protocol("usage must be an object".into()))?
        .extend(usage.clone());
    Ok(())
}

struct Block {
    value: Value,
    arguments: Option<String>,
    open: bool,
}

impl Block {
    fn new(value: Value) -> Result<Self, Error> {
        field(&value, "type")?;
        Ok(Self {
            value,
            arguments: None,
            open: true,
        })
    }

    fn stop(&mut self) -> Result<(), Error> {
        if let Some(arguments) = self.arguments.take() {
            self.value["input"] = serde_json::from_str(&arguments)
                .map_err(|e| Error::Protocol(format!("invalid tool arguments: {e}")))?;
        }
        self.open = false;
        Ok(())
    }

    fn delta(&mut self, index: usize, delta: &Value) -> Result<Option<Delta>, Error> {
        match (field(delta, "type")?, field(&self.value, "type")?) {
            ("text_delta", "text") => self.text(index, delta, "text", Some(DeltaKind::Text)),
            ("thinking_delta", "thinking") => {
                self.text(index, delta, "thinking", Some(DeltaKind::Reasoning))
            }
            ("signature_delta", "thinking") => self.text(index, delta, "signature", None),
            ("input_json_delta", "tool_use" | "server_tool_use") => {
                self.arguments(index, delta).map(Some)
            }
            ("citations_delta", "text") => {
                self.citation(delta)?;
                Ok(None)
            }
            (kind, _) => Err(Error::Protocol(format!(
                "unsupported or mismatched content delta {kind}"
            ))),
        }
    }

    fn text(
        &mut self,
        index: usize,
        delta: &Value,
        name: &str,
        kind: Option<DeltaKind>,
    ) -> Result<Option<Delta>, Error> {
        let text = field(delta, name)?;
        append_text(&mut self.value, name, text)?;
        Ok(kind.map(|kind| Delta {
            index,
            kind,
            text: text.into(),
        }))
    }

    fn arguments(&mut self, index: usize, delta: &Value) -> Result<Delta, Error> {
        let text = field(delta, "partial_json")?;
        self.arguments
            .get_or_insert_with(String::new)
            .push_str(text);
        Ok(Delta {
            index,
            kind: DeltaKind::ToolArguments,
            text: text.into(),
        })
    }

    fn citation(&mut self, delta: &Value) -> Result<(), Error> {
        let citation = object(delta, "citation")?;
        if self.value.get("citations").is_none_or(Value::is_null) {
            self.value["citations"] = json!([]);
        }
        self.value["citations"]
            .as_array_mut()
            .ok_or_else(|| Error::Protocol("citations must be an array".into()))?
            .push(citation);
        Ok(())
    }
}

fn append_text(block: &mut Value, name: &str, text: &str) -> Result<(), Error> {
    let value = block
        .as_object_mut()
        .expect("checked at start")
        .entry(name)
        .or_insert_with(|| Value::String(String::new()));
    let Value::String(value) = value else {
        return Err(Error::Protocol(format!(
            "content block {name} must be a string"
        )));
    };
    value.push_str(text);
    Ok(())
}
