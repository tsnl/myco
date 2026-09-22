use serde_json::Value;

use crate::model::types::{array, field};
use crate::model::{Error, Finish, Output, Response, ToolCall, Usage};

pub(crate) fn decode(body: &Value) -> Result<Response, Error> {
    if field(body, "type")? != "message" {
        return Err(Error::Protocol("expected an Anthropic message".into()));
    }
    let output = array(body, "content")?
        .iter()
        .filter_map(|b| block(b).transpose())
        .collect::<Result<_, _>>()?;
    Ok(Response::new(output, finish(body)?, usage(&body["usage"])?))
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
        "tool_use" => Some(Output::ToolCall(tool_call(block)?)),
        _ => None,
    })
}

fn tool_call(block: &Value) -> Result<ToolCall, Error> {
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
