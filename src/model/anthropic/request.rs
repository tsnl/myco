use serde_json::{Value, json};

use crate::model::types::array;
use crate::model::{Error, Message, Output, Request, Response, Tool, ToolCall};

pub(super) fn encode(request: &Request) -> Result<Value, Error> {
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
        Message::Assistant(response) => ("assistant", assistant(response)?, false),
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

fn assistant(response: &Response) -> Result<Vec<Value>, Error> {
    if let Some(native) = response.provider() {
        return Ok(array(&native.body, "content")?.clone());
    }
    response.output().iter().map(output).collect()
}

fn output(output: &Output) -> Result<Value, Error> {
    match output {
        Output::Text(text) | Output::Refusal(text) => Ok(json!({"type": "text", "text": text})),
        Output::ToolCall(call) => tool_call(call),
        Output::Reasoning(_) => Err(Error::InvalidRequest(
            "reasoning continuation requires the original provider response".into(),
        )),
    }
}

fn tool_call(call: &ToolCall) -> Result<Value, Error> {
    let input: Value = serde_json::from_str(&call.arguments)
        .map_err(|e| Error::InvalidRequest(format!("invalid tool arguments: {e}")))?;
    Ok(json!({"type": "tool_use", "id": call.id, "name": call.name, "input": input}))
}

fn tool(tool: &Tool) -> Value {
    json!({"name": tool.name, "description": tool.description, "input_schema": tool.parameters})
}
