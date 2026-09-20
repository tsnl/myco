use serde_json::{Value, json};

use crate::types::{array, field};
use crate::{Delta, DeltaKind, Error, Finish, Message, Output, Request, Response, ToolCall, Usage};

pub(crate) fn request(request: &Request) -> Result<Value, Error> {
    let mut input = vec![];
    for message in &request.messages {
        match message {
            Message::User(text) => input.push(json!({"role": "user", "content": text})),
            Message::ToolResult {
                call_id,
                output,
                is_error,
            } => {
                // Responses has no tool-result error flag. Keep errors visible
                // in the model-facing observation as well as the caller's record.
                let output = if *is_error {
                    format!("Tool error: {output}")
                } else {
                    output.clone()
                };
                input.push(
                    json!({"type": "function_call_output", "call_id": call_id, "output": output}),
                );
            }
            Message::Assistant(response) => {
                if let Some(native) = response.provider() {
                    input.extend(array(&native.body, "output")?.iter().cloned());
                } else {
                    for output in response.output() {
                        input.push(match output {
                            Output::Text(text) | Output::Refusal(text) => json!({"role": "assistant", "content": text}),
                            Output::ToolCall(call) => json!({"type": "function_call", "call_id": call.id, "name": call.name, "arguments": call.arguments}),
                            Output::Reasoning(_) => return Err(Error::InvalidRequest("reasoning continuation requires the original provider response".into())),
                        });
                    }
                }
            }
        }
    }
    let tools: Vec<_> = request
        .tools
        .iter()
        .map(|tool| {
            json!({
                "type": "function", "name": tool.name, "description": tool.description,
                "parameters": tool.parameters, "strict": false,
            })
        })
        .collect();
    Ok(json!({
        "model": request.model, "instructions": request.instructions,
        "input": input, "tools": tools, "max_output_tokens": request.max_output_tokens,
        "stream": true, "store": false, "include": ["reasoning.encrypted_content"],
    }))
}

pub(crate) fn response(body: &Value) -> Result<Response, Error> {
    let status = field(body, "status")?;
    if !matches!(status, "completed" | "incomplete") {
        return Err(Error::Provider(body.clone()));
    }
    let mut output = vec![];
    for item in array(body, "output")? {
        match field(item, "type")? {
            "message" => {
                for part in array(item, "content")? {
                    match field(part, "type")? {
                        "output_text" => output.push(Output::Text(field(part, "text")?.into())),
                        "refusal" => output.push(Output::Refusal(field(part, "refusal")?.into())),
                        _ => {}
                    }
                }
            }
            "reasoning" => {
                let reasoning = item["summary"]
                    .as_array()
                    .filter(|parts| !parts.is_empty())
                    .or_else(|| item["content"].as_array());
                if let Some(reasoning) = reasoning {
                    for part in reasoning {
                        if let Some(text) = part["text"].as_str() {
                            output.push(Output::Reasoning(text.into()));
                        }
                    }
                }
            }
            "function_call" => {
                let call = ToolCall {
                    id: field(item, "call_id")?.into(),
                    name: field(item, "name")?.into(),
                    arguments: field(item, "arguments")?.into(),
                };
                if status == "completed" {
                    call.validate()?;
                }
                output.push(Output::ToolCall(call));
            }
            _ => {} // Opaque items remain in the provider response and continuation.
        }
    }
    let finish = if status == "incomplete" {
        match field(&body["incomplete_details"], "reason")? {
            "max_output_tokens" => Finish::Length,
            reason => Finish::Other(format!("incomplete: {reason}")),
        }
    } else if output.iter().any(|o| matches!(o, Output::Refusal(_))) {
        Finish::Refusal
    } else if output.iter().any(|o| matches!(o, Output::ToolCall(_))) {
        Finish::ToolCalls
    } else {
        Finish::Stop
    };
    Ok(Response::new(
        output,
        finish,
        Usage {
            input_tokens: body["usage"]["input_tokens"].as_u64(),
            output_tokens: body["usage"]["output_tokens"].as_u64(),
            cache_read_tokens: body["usage"]["input_tokens_details"]["cached_tokens"].as_u64(),
            cache_write_tokens: None,
        },
    ))
}

pub(crate) fn delta(event: &Value) -> Result<Option<Delta>, Error> {
    let kind = match field(event, "type")? {
        "response.output_text.delta" => DeltaKind::Text,
        "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
            DeltaKind::Reasoning
        }
        "response.refusal.delta" => DeltaKind::Refusal,
        "response.function_call_arguments.delta" => DeltaKind::ToolArguments,
        _ => return Ok(None),
    };
    let index = event["output_index"]
        .as_u64()
        .and_then(|n| usize::try_from(n).ok())
        .ok_or_else(|| Error::Protocol("missing output_index".into()))?;
    Ok(Some(Delta {
        index,
        kind,
        text: field(event, "delta")?.into(),
    }))
}
