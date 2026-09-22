use serde_json::{Map, Value, json};

use crate::driver::Decoded;
use crate::types::{array, field, index};
use crate::{Delta, DeltaKind, Error};

#[derive(Default)]
pub(super) struct Accumulator {
    message: Option<Value>,
    blocks: Vec<Block>,
}

impl Accumulator {
    pub fn event(&mut self, event: &Value) -> Result<Decoded, Error> {
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
