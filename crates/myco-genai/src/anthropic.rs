use serde_json::{Value, json};

use crate::types::{array, field};
use crate::{Delta, DeltaKind, Error, Finish, Message, Output, Request, Response, ToolCall, Usage};

pub(crate) fn request(request: &Request) -> Result<Value, Error> {
    let mut messages: Vec<Value> = vec![];
    for message in &request.messages {
        let (role, content, result) = match message {
            Message::User(text) => ("user", vec![json!({"type": "text", "text": text})], false),
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
            Message::Assistant(response) => {
                let content = if let Some(native) = response.provider() {
                    array(&native.body, "content")?.clone()
                } else {
                    response.output().iter().map(|output| match output {
                        Output::Text(text) | Output::Refusal(text) => Ok(json!({"type": "text", "text": text})),
                        Output::ToolCall(call) => {
                            let input: Value = serde_json::from_str(&call.arguments)
                                .map_err(|e| Error::InvalidRequest(format!("invalid tool arguments: {e}")))?;
                            Ok(json!({"type": "tool_use", "id": call.id, "name": call.name, "input": input}))
                        }
                        Output::Reasoning(_) => Err(Error::InvalidRequest("reasoning continuation requires the original provider response".into())),
                    }).collect::<Result<Vec<_>, _>>()?
                };
                ("assistant", content, false)
            }
        };
        if let Some(last) = messages.last_mut().filter(|last| last["role"] == role) {
            let previous = last["content"]
                .as_array_mut()
                .expect("constructed content array");
            if result {
                // Tool observations must precede ordinary user text in the
                // message answering an assistant's tool calls.
                let index = previous
                    .iter()
                    .take_while(|v| v["type"] == "tool_result")
                    .count();
                previous.splice(index..index, content);
            } else {
                previous.extend(content);
            }
        } else {
            messages.push(json!({"role": role, "content": content}));
        }
    }
    let tools: Vec<_> = request
        .tools
        .iter()
        .map(|tool| {
            json!({
                "name": tool.name, "description": tool.description, "input_schema": tool.parameters,
            })
        })
        .collect();
    let mut body = json!({
        "model": request.model, "messages": messages,
        "max_tokens": request.max_output_tokens, "stream": true,
    });
    if !request.instructions.is_empty() {
        body["system"] = request.instructions.clone().into();
    }
    if !tools.is_empty() {
        body["tools"] = tools.into();
    }
    Ok(body)
}

pub(crate) fn response(body: &Value) -> Result<Response, Error> {
    if field(body, "type")? != "message" || field(body, "role")? != "assistant" {
        return Err(Error::Protocol(
            "expected an Anthropic assistant message".into(),
        ));
    }
    let finish = match field(body, "stop_reason")? {
        "end_turn" | "stop_sequence" => Finish::Stop,
        "tool_use" => Finish::ToolCalls,
        "max_tokens" => Finish::Length,
        "refusal" => Finish::Refusal,
        reason => Finish::Other(reason.into()),
    };
    let mut output = vec![];
    for block in array(body, "content")? {
        match field(block, "type")? {
            "text" => output.push(Output::Text(field(block, "text")?.into())),
            "thinking" => {
                field(block, "signature")?;
                output.push(Output::Reasoning(field(block, "thinking")?.into()));
            }
            "tool_use" => {
                let input = block
                    .get("input")
                    .filter(|v| v.is_object())
                    .ok_or_else(|| Error::Protocol("tool_use.input must be an object".into()))?;
                let call = ToolCall {
                    id: field(block, "id")?.into(),
                    name: field(block, "name")?.into(),
                    arguments: input.to_string(),
                };
                call.validate()?;
                output.push(Output::ToolCall(call));
            }
            _ => {}
        }
    }
    let usage = &body["usage"];
    let read = usage["cache_read_input_tokens"].as_u64();
    let write = usage["cache_creation_input_tokens"].as_u64();
    let input_tokens = usage["input_tokens"]
        .as_u64()
        .map(|input| {
            input
                .checked_add(read.unwrap_or(0))
                .and_then(|n| n.checked_add(write.unwrap_or(0)))
                .ok_or_else(|| Error::Protocol("input token count overflow".into()))
        })
        .transpose()?;
    Ok(Response::new(
        output,
        finish,
        Usage {
            input_tokens,
            output_tokens: usage["output_tokens"].as_u64(),
            cache_read_tokens: read,
            cache_write_tokens: write,
        },
    ))
}

#[derive(Default)]
pub(crate) struct Accumulator {
    message: Option<Value>,
    blocks: Vec<Block>,
}

struct Block {
    value: Value,
    arguments: Option<String>,
    open: bool,
}

impl Accumulator {
    pub fn event(&mut self, event: &Value) -> Result<(Option<Delta>, Option<Value>), Error> {
        let kind = field(event, "type")?;
        if kind == "error" {
            return Err(Error::Provider(event.clone()));
        }
        if kind == "message_start" {
            if self.message.is_some() {
                return Err(Error::Protocol("duplicate message_start".into()));
            }
            let message = event
                .get("message")
                .filter(|v| v.is_object())
                .ok_or_else(|| Error::Protocol("missing message_start.message".into()))?;
            if !array(message, "content")?.is_empty() {
                return Err(Error::Protocol(
                    "message_start already contains content".into(),
                ));
            }
            self.message = Some(message.clone());
            return Ok((None, None));
        }
        if kind.starts_with("content_block_") {
            if self.message.is_none() {
                return Err(Error::Protocol("content before message_start".into()));
            }
            let index = event["index"]
                .as_u64()
                .and_then(|n| usize::try_from(n).ok())
                .ok_or_else(|| Error::Protocol("missing content block index".into()))?;
            if kind == "content_block_start" {
                if index != self.blocks.len() {
                    return Err(Error::Protocol(
                        "duplicate or out-of-order content block start".into(),
                    ));
                }
                let value = event
                    .get("content_block")
                    .filter(|v| v.is_object())
                    .ok_or_else(|| Error::Protocol("missing content_block".into()))?
                    .clone();
                field(&value, "type")?;
                self.blocks.push(Block {
                    value,
                    arguments: None,
                    open: true,
                });
                return Ok((None, None));
            }
            let block = self
                .blocks
                .get_mut(index)
                .filter(|b| b.open)
                .ok_or_else(|| {
                    Error::Protocol("content event references an absent or closed block".into())
                })?;
            if kind == "content_block_stop" {
                if let Some(arguments) = block.arguments.take() {
                    block.value["input"] = serde_json::from_str(&arguments)
                        .map_err(|e| Error::Protocol(format!("invalid tool arguments: {e}")))?;
                }
                block.open = false;
            } else if kind == "content_block_delta" {
                let delta = &event["delta"];
                let (target, source, output) = match field(delta, "type")? {
                    "text_delta" if block.value["type"] == "text" => {
                        ("text", "text", Some(DeltaKind::Text))
                    }
                    "thinking_delta" if block.value["type"] == "thinking" => {
                        ("thinking", "thinking", Some(DeltaKind::Reasoning))
                    }
                    "signature_delta" if block.value["type"] == "thinking" => {
                        ("signature", "signature", None)
                    }
                    "input_json_delta"
                        if matches!(
                            block.value["type"].as_str(),
                            Some("tool_use" | "server_tool_use")
                        ) =>
                    {
                        let text = field(delta, "partial_json")?;
                        block
                            .arguments
                            .get_or_insert_with(String::new)
                            .push_str(text);
                        return Ok((
                            Some(Delta {
                                index,
                                kind: DeltaKind::ToolArguments,
                                text: text.into(),
                            }),
                            None,
                        ));
                    }
                    "citations_delta" if block.value["type"] == "text" => {
                        let citation = delta
                            .get("citation")
                            .filter(|v| v.is_object())
                            .ok_or_else(|| Error::Protocol("missing citation".into()))?;
                        if block.value.get("citations").is_none_or(Value::is_null) {
                            block.value["citations"] = json!([]);
                        }
                        block.value["citations"]
                            .as_array_mut()
                            .ok_or_else(|| Error::Protocol("citations must be an array".into()))?
                            .push(citation.clone());
                        return Ok((None, None));
                    }
                    other => {
                        return Err(Error::Protocol(format!(
                            "unsupported or mismatched content delta {other}"
                        )));
                    }
                };
                let text = field(delta, source)?;
                let value = block
                    .value
                    .as_object_mut()
                    .expect("checked at start")
                    .entry(target)
                    .or_insert_with(|| Value::String(String::new()));
                let Value::String(value) = value else {
                    return Err(Error::Protocol(format!(
                        "content block {target} must be a string"
                    )));
                };
                value.push_str(text);
                return Ok((
                    output.map(|kind| Delta {
                        index,
                        kind,
                        text: text.into(),
                    }),
                    None,
                ));
            }
        } else if kind == "message_delta" {
            let message = self
                .message
                .as_mut()
                .ok_or_else(|| Error::Protocol("message_delta before message_start".into()))?;
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
            message
                .as_object_mut()
                .expect("checked at start")
                .extend(delta.clone());
            if let Some(usage) = event.get("usage") {
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
            }
        } else if kind == "message_stop" {
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
            return Ok((None, Some(message)));
        }
        Ok((None, None))
    }
}
