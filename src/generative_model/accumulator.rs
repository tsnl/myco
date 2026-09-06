//! Assemble validated message parts without owning a stream, retry policy, or event sink.

use super::{
    Content, ContentDelta, ContentStart, GenerateError, GenerateOutput, MessagePart, TokenUsage,
    ToolUse, ToolUseDelta, ToolUseStart, TurnEndReason,
};

#[derive(Default)]
pub struct MessageAccumulator {
    started: bool,
    content: Vec<Option<Content>>,
    tool_uses: Vec<Option<IncompleteToolUse>>,
    turn_end_reason: Option<TurnEndReason>,
    usage: Option<TokenUsage>,
}

impl MessageAccumulator {
    pub fn push(&mut self, part: &MessagePart) -> Result<(), GenerateError> {
        if !self.started {
            return self.start(part);
        }
        match part {
            MessagePart::MessageStart => return Err(malformed("unexpected MessageStart")),
            MessagePart::ContentStart(start) => {
                let (index, block) = start_block(start.clone());
                ensure_slot(&mut self.content, index, block);
            }
            MessagePart::ContentDelta(delta) => apply_content_delta(&mut self.content, delta)?,
            MessagePart::ToolUseStart(ToolUseStart { index, name }) => {
                ensure_slot(
                    &mut self.tool_uses,
                    *index,
                    IncompleteToolUse {
                        name: name.clone(),
                        input_json: String::new(),
                    },
                );
            }
            MessagePart::ToolUseDelta(delta) => self.append_tool_input(delta)?,
            MessagePart::TurnEndReason(reason) => self.turn_end_reason = Some(reason.clone()),
            MessagePart::Usage(usage) => {
                self.usage = Some(self.usage.map_or(*usage, |prev| prev.merge(*usage)))
            }
        }
        Ok(())
    }

    fn start(&mut self, part: &MessagePart) -> Result<(), GenerateError> {
        if !matches!(part, MessagePart::MessageStart) {
            return Err(malformed(
                "first item is not MessageStart. Did you accidentally drain the stream already?",
            ));
        }
        self.started = true;
        Ok(())
    }

    fn append_tool_input(&mut self, delta: &ToolUseDelta) -> Result<(), GenerateError> {
        let index = delta.index;
        let tool = self
            .tool_uses
            .get_mut(index)
            .and_then(Option::as_mut)
            .ok_or_else(|| malformed(format!("tool use delta index {index} is out of bounds")))?;
        tool.input_json.push_str(&delta.input_json_delta);
        Ok(())
    }

    pub fn finish(self) -> Result<GenerateOutput, GenerateError> {
        if !self.started {
            return Err(malformed(
                "empty stream. Did you accidentally drain the stream already?",
            ));
        }
        let content = filled_slots(self.content, "content block")?;
        let tool_uses = filled_slots(self.tool_uses, "tool use")?
            .into_iter()
            .map(IncompleteToolUse::finish)
            .collect::<Result<_, _>>()?;
        let turn_end_reason = self
            .turn_end_reason
            .ok_or_else(|| malformed("no turn end reason provided"))?;
        Ok(GenerateOutput {
            content,
            tool_uses,
            turn_end_reason,
            usage: self.usage,
        })
    }
}

struct IncompleteToolUse {
    name: String,
    input_json: String,
}

impl IncompleteToolUse {
    fn finish(self) -> Result<ToolUse, GenerateError> {
        let json = if self.input_json.is_empty() {
            "{}"
        } else {
            &self.input_json
        };
        let input = serde_json::from_str(json)
            .map_err(|error| malformed(format!("tool use input JSON is invalid: {error}")))?;
        Ok(ToolUse {
            name: self.name,
            input,
        })
    }
}

fn malformed(message: impl std::fmt::Display) -> GenerateError {
    GenerateError::MalformedResponseError(format!("Malformed stream: {message}"))
}

fn filled_slots<T>(slots: Vec<Option<T>>, name: &str) -> Result<Vec<T>, GenerateError> {
    slots
        .into_iter()
        .enumerate()
        .map(|(index, slot)| {
            slot.ok_or_else(|| malformed(format!("missing {name} at index {index}")))
        })
        .collect()
}

fn ensure_slot<T>(slots: &mut Vec<Option<T>>, index: usize, value: T) {
    while slots.len() <= index {
        slots.push(None);
    }
    slots[index] = Some(value);
}

/// The empty [`Content`] block a [`ContentStart`] opens, with its index.
fn start_block(start: ContentStart) -> (usize, Content) {
    match start {
        ContentStart::Text { index } => (
            index,
            Content::Text {
                text: String::new(),
            },
        ),
        ContentStart::Image { index } => (
            index,
            Content::Image {
                source: String::new(),
            },
        ),
        ContentStart::Thinking {
            index,
            signature,
            redacted,
        } => (
            index,
            Content::Thinking {
                text: String::new(),
                signature,
                redacted,
            },
        ),
    }
}

/// Append a [`ContentDelta`] to its opened block; the slot must exist and be
/// the matching kind (redacted thinking swallows its deltas).
fn apply_content_delta(
    content: &mut [Option<Content>],
    delta: &ContentDelta,
) -> Result<(), GenerateError> {
    let index = match delta {
        ContentDelta::Text { index, .. }
        | ContentDelta::Image { index, .. }
        | ContentDelta::Thinking { index, .. } => *index,
    };
    match (content.get_mut(index).and_then(Option::as_mut), delta) {
        (Some(Content::Text { text }), ContentDelta::Text { delta, .. }) => text.push_str(delta),
        (Some(Content::Image { source }), ContentDelta::Image { delta, .. }) => {
            source.push_str(delta);
        }
        (Some(Content::Thinking { text, redacted, .. }), ContentDelta::Thinking { delta, .. }) => {
            if !*redacted {
                text.push_str(delta);
            }
        }
        _ => {
            return Err(GenerateError::MalformedResponseError(format!(
                "Malformed stream: content delta at index {index}: out of bounds or wrong kind"
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assemble(parts: &[MessagePart]) -> Result<GenerateOutput, GenerateError> {
        let mut accumulator = MessageAccumulator::default();
        for part in parts {
            accumulator.push(part)?;
        }
        accumulator.finish()
    }

    #[test]
    fn tool_input_fragments_form_one_json_value() {
        let output = assemble(&[
            MessagePart::MessageStart,
            MessagePart::ToolUseStart(ToolUseStart {
                index: 0,
                name: "bash".into(),
            }),
            MessagePart::ToolUseDelta(ToolUseDelta {
                index: 0,
                input_json_delta: "{\"command\":".into(),
            }),
            MessagePart::ToolUseDelta(ToolUseDelta {
                index: 0,
                input_json_delta: "\"pwd\"}".into(),
            }),
            MessagePart::TurnEndReason(TurnEndReason::ToolUse),
        ])
        .unwrap();
        assert_eq!(output.tool_uses.len(), 1);
        assert_eq!(output.tool_uses[0].name, "bash");
        assert_eq!(
            output.tool_uses[0].input,
            serde_json::json!({"command": "pwd"})
        );
    }

    #[test]
    fn incomplete_and_malformed_messages_are_rejected() {
        let cases = [
            vec![],
            vec![MessagePart::TurnEndReason(TurnEndReason::EndTurn)],
            vec![MessagePart::MessageStart],
            vec![MessagePart::MessageStart, MessagePart::MessageStart],
            vec![
                MessagePart::MessageStart,
                MessagePart::ContentDelta(ContentDelta::Text {
                    index: 0,
                    delta: "unopened".into(),
                }),
            ],
            vec![
                MessagePart::MessageStart,
                MessagePart::ContentStart(ContentStart::Text { index: 1 }),
                MessagePart::TurnEndReason(TurnEndReason::EndTurn),
            ],
            vec![
                MessagePart::MessageStart,
                MessagePart::ToolUseDelta(ToolUseDelta {
                    index: 0,
                    input_json_delta: "{}".into(),
                }),
            ],
            vec![
                MessagePart::MessageStart,
                MessagePart::ToolUseStart(ToolUseStart {
                    index: 0,
                    name: "bash".into(),
                }),
                MessagePart::ToolUseDelta(ToolUseDelta {
                    index: 0,
                    input_json_delta: "{".into(),
                }),
                MessagePart::TurnEndReason(TurnEndReason::ToolUse),
            ],
        ];
        for parts in cases {
            assert!(
                matches!(
                    assemble(&parts),
                    Err(GenerateError::MalformedResponseError(_))
                ),
                "{parts:?}"
            );
        }
    }
}
