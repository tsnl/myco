use serde_json::Value;

use crate::model::types::{array, field};
use crate::model::{Error, Finish, Output, Response, ToolCall, Usage};

pub(crate) fn decode(body: &Value) -> Result<Response, Error> {
    let status = field(body, "status")?;
    if !matches!(status, "completed" | "incomplete") {
        return Err(Error::Provider(body.clone()));
    }
    let output = outputs(body, status == "completed")?;
    let finish = finish(body, &output)?;
    Ok(Response::new(output, finish, usage(body)))
}

fn outputs(body: &Value, complete: bool) -> Result<Vec<Output>, Error> {
    let mut output = vec![];
    for item in array(body, "output")? {
        match field(item, "type")? {
            "message" => output.extend(message(item)?),
            "reasoning" => output.extend(reasoning(item)),
            "function_call" => output.push(Output::ToolCall(tool_call(item, complete)?)),
            _ => {} // Opaque items remain in the native response and continuation.
        }
    }
    Ok(output)
}

fn message(item: &Value) -> Result<Vec<Output>, Error> {
    let mut output = vec![];
    for part in array(item, "content")? {
        match field(part, "type")? {
            "output_text" => output.push(Output::Text(field(part, "text")?.into())),
            "refusal" => output.push(Output::Refusal(field(part, "refusal")?.into())),
            _ => {}
        }
    }
    Ok(output)
}

fn reasoning(item: &Value) -> Vec<Output> {
    let parts = item["summary"]
        .as_array()
        .filter(|parts| !parts.is_empty())
        .or_else(|| item["content"].as_array());
    parts
        .into_iter()
        .flatten()
        .filter_map(|part| {
            part["text"]
                .as_str()
                .map(|text| Output::Reasoning(text.into()))
        })
        .collect()
}

fn tool_call(item: &Value, complete: bool) -> Result<ToolCall, Error> {
    let call = ToolCall {
        id: field(item, "call_id")?.into(),
        name: field(item, "name")?.into(),
        arguments: field(item, "arguments")?.into(),
    };
    if complete {
        call.validate()?;
    }
    Ok(call)
}

fn finish(body: &Value, output: &[Output]) -> Result<Finish, Error> {
    if body["status"] == "incomplete" {
        return incomplete(&body["incomplete_details"]);
    }
    Ok(if output.iter().any(|o| matches!(o, Output::Refusal(_))) {
        Finish::Refusal
    } else if output.iter().any(|o| matches!(o, Output::ToolCall(_))) {
        Finish::ToolCalls
    } else {
        Finish::Stop
    })
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
