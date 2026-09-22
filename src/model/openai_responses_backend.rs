use serde_json::{Value, json};

use super::backend_helpers::{
    Completion, Decoded, Driver, EventStream, Protocol, array, field, index,
};
use super::http_helpers::Transport;
use super::{
    ContentPart, Delta, DeltaKind, Error, Finish, Message, Request, Tool, ToolCall, Usage,
};

pub(super) struct Backend {
    transport: Transport,
}

impl Backend {
    pub(super) fn new(endpoint: &str, api_key: &str) -> Result<Self, Error> {
        Ok(Self {
            transport: Transport::new(Protocol::OpenAiResponses, endpoint, api_key)?,
        })
    }
}

impl Driver for Backend {
    fn encode(&self, request: &Request) -> Result<Value, Error> {
        encode_request(request)
    }

    fn generate(&self, body: Value) -> EventStream<'_> {
        self.transport
            .generate(Protocol::OpenAiResponses, body, decode_event)
    }
}

fn encode_request(request: &Request) -> Result<Value, Error> {
    let input = request
        .messages
        .iter()
        .map(encode_message)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(json!({
        "model": request.model, "instructions": request.instructions,
        "input": input.into_iter().flatten().collect::<Vec<_>>(),
        "tools": request.tools.iter().map(tool).collect::<Vec<_>>(),
        "max_output_tokens": request.max_output_tokens,
        "stream": true, "store": false, "include": ["reasoning.encrypted_content"],
    }))
}

fn encode_message(message: &Message) -> Result<Vec<Value>, Error> {
    match message {
        Message::User(text) => Ok(vec![json!({"role": "user", "content": text})]),
        Message::Assistant { content } => assistant(content),
        Message::ToolResult {
            call_id,
            output,
            is_error,
        } => Ok(vec![tool_result(call_id, output, *is_error)]),
    }
}

fn assistant(content: &[ContentPart]) -> Result<Vec<Value>, Error> {
    content
        .iter()
        .filter_map(|part| encode_part(part).transpose())
        .collect()
}

fn encode_part(part: &ContentPart) -> Result<Option<Value>, Error> {
    Ok(match part {
        ContentPart::Text(text) | ContentPart::Refusal(text) => {
            Some(json!({"role":"assistant", "content":text}))
        }
        ContentPart::ToolCall(call) => Some(json!({"type":"function_call", "call_id":call.id,
            "name":call.name, "arguments":call.arguments()?.to_string()})),
        ContentPart::EncryptedReasoning { id, summary, data } => Some(json!({
            "type":"reasoning", "id":id, "encrypted_content":data,
            "summary":summary.iter().map(|text| json!({"type":"summary_text", "text":text})).collect::<Vec<_>>(),
        })),
        ContentPart::Reasoning {
            signature: None, ..
        } => None,
        _ => {
            return Err(Error::InvalidRequest(
                "signed reasoning belongs to another backend".into(),
            ));
        }
    })
}

fn tool_result(id: &str, output: &str, is_error: bool) -> Value {
    // Responses has no error flag; the observation itself must carry the error.
    let output = if is_error {
        format!("Tool error: {output}")
    } else {
        output.into()
    };
    json!({"type": "function_call_output", "call_id": id, "output": output})
}

fn tool(tool: &Tool) -> Value {
    json!({"type": "function", "name": tool.name, "description": tool.description,
        "parameters": tool.parameters, "strict": false})
}

pub(super) fn decode_response(body: &Value) -> Result<Completion, Error> {
    let status = field(body, "status")?;
    if !matches!(status, "completed" | "incomplete") {
        return Err(Error::Provider(body.clone()));
    }
    let output = outputs(body, status == "completed")?;
    let finish = finish(body, &output)?;
    Ok(Completion {
        output,
        finish,
        usage: usage(body),
    })
}

fn outputs(body: &Value, complete: bool) -> Result<Vec<ContentPart>, Error> {
    let mut output = vec![];
    for item in array(body, "output")? {
        match field(item, "type")? {
            "message" => output.extend(decode_message(item)?),
            "reasoning" => output.extend(reasoning(item)?),
            "function_call" => output.push(ContentPart::ToolCall(tool_call(item, complete)?)),
            _ => {} // Unsupported items remain available in raw events.
        }
    }
    Ok(output)
}

fn decode_message(item: &Value) -> Result<Vec<ContentPart>, Error> {
    let mut output = vec![];
    for part in array(item, "content")? {
        match field(part, "type")? {
            "output_text" => output.push(ContentPart::Text(field(part, "text")?.into())),
            "refusal" => output.push(ContentPart::Refusal(field(part, "refusal")?.into())),
            _ => {}
        }
    }
    Ok(output)
}

fn reasoning(item: &Value) -> Result<Vec<ContentPart>, Error> {
    if item.get("encrypted_content").is_some_and(|v| !v.is_null()) {
        return Ok(vec![encrypted_reasoning(item)?]);
    }
    Ok(reasoning_text(item)?
        .into_iter()
        .map(|text| ContentPart::Reasoning {
            text,
            signature: None,
        })
        .collect())
}

fn encrypted_reasoning(item: &Value) -> Result<ContentPart, Error> {
    Ok(ContentPart::EncryptedReasoning {
        id: field(item, "id")?.into(),
        summary: array(item, "summary")?
            .iter()
            .map(|part| field(part, "text").map(str::to_owned))
            .collect::<Result<_, _>>()?,
        data: field(item, "encrypted_content")?.into(),
    })
}

fn reasoning_text(item: &Value) -> Result<Vec<String>, Error> {
    item["summary"]
        .as_array()
        .filter(|parts| !parts.is_empty())
        .or_else(|| item["content"].as_array())
        .into_iter()
        .flatten()
        .map(|part| field(part, "text").map(str::to_owned))
        .collect()
}

fn tool_call(item: &Value, complete: bool) -> Result<ToolCall, Error> {
    let call = ToolCall {
        id: field(item, "call_id")?.into(),
        name: field(item, "name")?.into(),
        arguments: serde_json::from_str(field(item, "arguments")?)
            .map_err(|error| error.to_string()),
    };
    if complete {
        call.validate()?;
    }
    Ok(call)
}

fn finish(body: &Value, output: &[ContentPart]) -> Result<Finish, Error> {
    if body["status"] == "incomplete" {
        return incomplete(&body["incomplete_details"]);
    }
    Ok(
        if output.iter().any(|o| matches!(o, ContentPart::Refusal(_))) {
            Finish::Refusal
        } else if output.iter().any(|o| matches!(o, ContentPart::ToolCall(_))) {
            Finish::ToolCalls
        } else {
            Finish::Stop
        },
    )
}

fn incomplete(details: &Value) -> Result<Finish, Error> {
    Ok(match field(details, "reason")? {
        "max_output_tokens" => Finish::Length,
        reason => Finish::Other(format!("incomplete: {reason}")),
    })
}

fn usage(body: &Value) -> Usage {
    Usage {
        input_tokens: body["usage"]["input_tokens"].as_u64(),
        output_tokens: body["usage"]["output_tokens"].as_u64(),
        cache_read_tokens: body["usage"]["input_tokens_details"]["cached_tokens"].as_u64(),
        cache_write_tokens: None,
    }
}

fn decode_event(event: &Value) -> Result<Decoded, Error> {
    match field(event, "type")? {
        "error" | "response.failed" => Err(Error::Provider(event.clone())),
        "response.completed" | "response.incomplete" => terminal(event).map(Decoded::Completed),
        _ => delta(event).map(Decoded::Progress),
    }
}

fn terminal(event: &Value) -> Result<Value, Error> {
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
    Ok(body.clone())
}

fn delta(event: &Value) -> Result<Option<Delta>, Error> {
    let Some(kind) = delta_kind(field(event, "type")?) else {
        return Ok(None);
    };
    Ok(Some(Delta {
        index: index(event, "output_index")?,
        part: part_index(event)?,
        kind,
        text: field(event, "delta")?.into(),
    }))
}

fn part_index(event: &Value) -> Result<usize, Error> {
    ["content_index", "summary_index"]
        .into_iter()
        .find(|name| event.get(name).is_some())
        .map(|name| index(event, name))
        .transpose()
        .map(|part| part.unwrap_or(0))
}

fn delta_kind(kind: &str) -> Option<DeltaKind> {
    match kind {
        "response.output_text.delta" => Some(DeltaKind::Text),
        "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
            Some(DeltaKind::Reasoning)
        }
        "response.refusal.delta" => Some(DeltaKind::Refusal),
        "response.function_call_arguments.delta" => Some(DeltaKind::ToolArguments),
        _ => None,
    }
}
