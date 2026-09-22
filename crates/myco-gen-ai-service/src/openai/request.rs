use serde_json::{Value, json};

use crate::types::array;
use crate::{Error, Message, Output, Request, Response, Tool};

pub(super) fn encode(request: &Request) -> Result<Value, Error> {
    let input = request
        .messages
        .iter()
        .map(message)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(json!({
        "model": request.model, "instructions": request.instructions,
        "input": input.into_iter().flatten().collect::<Vec<_>>(),
        "tools": request.tools.iter().map(tool).collect::<Vec<_>>(),
        "max_output_tokens": request.max_output_tokens,
        "stream": true, "store": false, "include": ["reasoning.encrypted_content"],
    }))
}

fn message(message: &Message) -> Result<Vec<Value>, Error> {
    match message {
        Message::User(text) => Ok(vec![json!({"role": "user", "content": text})]),
        Message::Assistant(response) => assistant(response),
        Message::ToolResult {
            call_id,
            output,
            is_error,
        } => Ok(vec![tool_result(call_id, output, *is_error)]),
    }
}

fn assistant(response: &Response) -> Result<Vec<Value>, Error> {
    if let Some(native) = response.provider() {
        return Ok(array(&native.body, "output")?.clone());
    }
    response.output().iter().map(output).collect()
}

fn output(output: &Output) -> Result<Value, Error> {
    match output {
        Output::Text(text) | Output::Refusal(text) => {
            Ok(json!({"role": "assistant", "content": text}))
        }
        Output::ToolCall(call) => Ok(
            json!({"type": "function_call", "call_id": call.id, "name": call.name, "arguments": call.arguments}),
        ),
        Output::Reasoning(_) => Err(Error::InvalidRequest(
            "reasoning continuation requires the original provider response".into(),
        )),
    }
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
