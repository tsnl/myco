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
            MessagePart::ToolUseStart(ToolUseStart {
                index,
                name,
                thought_signature,
            }) => {
                ensure_slot(
                    &mut self.tool_uses,
                    *index,
                    IncompleteToolUse {
                        name: name.clone(),
                        input_json: String::new(),
                        thought_signature: thought_signature.clone(),
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
            .collect::<Result<Vec<_>, _>>()?;
        let turn_end_reason = self
            .turn_end_reason
            .ok_or_else(|| malformed("no turn end reason provided"))?;
        if turn_end_reason == TurnEndReason::ToolUse && tool_uses.is_empty() {
            return Err(malformed(
                "turn ended in tool_use but streamed zero tool uses",
            ));
        }
        Ok(GenerateOutput {
            content,
            tool_uses,
            turn_end_reason,
            usage: self.usage,
        })
    }
}

impl GenerateOutput {
    /// The part stream [`MessageAccumulator`] assembles back into this
    /// output, one delta per block: a stand-in for a provider stream in
    /// scripted models.
    ///
    /// # Panics
    ///
    /// On a [`Content::System`] block, which is runtime input and never
    /// model output.
    pub fn into_parts(self) -> Vec<MessagePart> {
        let mut parts = vec![MessagePart::MessageStart];
        for (index, content) in self.content.into_iter().enumerate() {
            match content {
                Content::System { .. } => {
                    panic!("system parts are runtime input, not model output")
                }
                Content::Text { text } => parts.extend([
                    MessagePart::ContentStart(ContentStart::Text { index }),
                    MessagePart::ContentDelta(ContentDelta::Text { index, delta: text }),
                ]),
                Content::Image { source } => parts.extend([
                    MessagePart::ContentStart(ContentStart::Image { index }),
                    MessagePart::ContentDelta(ContentDelta::Image {
                        index,
                        delta: source,
                    }),
                ]),
                Content::Thinking {
                    text,
                    signature,
                    redacted,
                } => {
                    parts.push(MessagePart::ContentStart(ContentStart::Thinking {
                        index,
                        signature,
                        redacted,
                    }));
                    if !text.is_empty() && !redacted {
                        parts.push(MessagePart::ContentDelta(ContentDelta::Thinking {
                            index,
                            delta: text,
                        }));
                    }
                }
            }
        }
        for (index, tool_use) in self.tool_uses.into_iter().enumerate() {
            parts.extend([
                MessagePart::ToolUseStart(ToolUseStart {
                    index,
                    name: tool_use.name,
                    thought_signature: tool_use.thought_signature,
                }),
                MessagePart::ToolUseDelta(ToolUseDelta {
                    index,
                    input_json_delta: tool_use.input.to_string(),
                }),
            ]);
        }
        parts.extend(self.usage.map(MessagePart::Usage));
        parts.push(MessagePart::TurnEndReason(self.turn_end_reason));
        parts
    }
}

struct IncompleteToolUse {
    name: String,
    input_json: String,
    thought_signature: Option<String>,
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
            thought_signature: self.thought_signature,
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
        | ContentDelta::Thinking { index, .. }
        | ContentDelta::Signature { index, .. } => *index,
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
        (
            Some(Content::Thinking {
                signature,
                redacted,
                ..
            }),
            ContentDelta::Signature { delta, .. },
        ) => {
            if !*redacted {
                signature.get_or_insert_with(String::new).push_str(delta);
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
    fn into_parts_reassembles_into_the_same_output() {
        let output = GenerateOutput {
            content: vec![
                Content::Thinking {
                    text: "plan".into(),
                    signature: Some("sig".into()),
                    redacted: false,
                },
                Content::Thinking {
                    text: String::new(),
                    signature: Some("opaque".into()),
                    redacted: true,
                },
                Content::Text {
                    text: "answer".into(),
                },
                Content::Image {
                    source: "data:image/png;base64,AA".into(),
                },
            ],
            tool_uses: vec![ToolUse {
                name: "bash".into(),
                input: serde_json::json!({"command": "pwd"}),
                thought_signature: Some("tool-sig".into()),
            }],
            turn_end_reason: TurnEndReason::ToolUse,
            usage: Some(TokenUsage {
                input_tokens: 10,
                output_tokens: 2,
                cached_input_tokens: 4,
            }),
        };
        let assembled = assemble(&output.clone().into_parts()).unwrap();
        assert_eq!(assembled.content, output.content);
        assert_eq!(assembled.tool_uses, output.tool_uses);
        assert_eq!(assembled.turn_end_reason, output.turn_end_reason);
        assert_eq!(assembled.usage, output.usage);
    }

    #[test]
    fn tool_input_fragments_form_one_json_value() {
        let output = assemble(&[
            MessagePart::MessageStart,
            MessagePart::ToolUseStart(ToolUseStart {
                index: 0,
                name: "bash".into(),
                thought_signature: None,
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
                    thought_signature: None,
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

    #[test]
    fn tool_use_requires_a_call_but_empty_end_turn_is_valid() {
        let output = assemble(&[
            MessagePart::MessageStart,
            MessagePart::TurnEndReason(TurnEndReason::EndTurn),
        ])
        .unwrap();
        assert!(output.content.is_empty());
        assert!(output.tool_uses.is_empty());
        for content in [false, true] {
            let mut parts = vec![MessagePart::MessageStart];
            if content {
                parts.extend([
                    MessagePart::ContentStart(ContentStart::Text { index: 0 }),
                    MessagePart::ContentDelta(ContentDelta::Text {
                        index: 0,
                        delta: "unfinished draft".into(),
                    }),
                ]);
            }
            parts.push(MessagePart::TurnEndReason(TurnEndReason::ToolUse));
            assert!(matches!(
                assemble(&parts),
                Err(GenerateError::MalformedResponseError(_))
            ));
        }
    }
}
