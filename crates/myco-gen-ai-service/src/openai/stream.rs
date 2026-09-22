use serde_json::Value;

use crate::driver::Decoded;
use crate::types::{field, index};
use crate::{Delta, DeltaKind, Error};

pub(super) fn decode(event: &Value) -> Result<Decoded, Error> {
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
        kind,
        text: field(event, "delta")?.into(),
    }))
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
