//! Scripted generative model for integration tests.
//!
//! Each `generate` call consumes the next pre-baked [`GenerateOutput`] (FIFO)
//! and replays it as a stream of [`MessagePart`]s — same shape the agent sees
//! from a real provider.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use futures::stream;
use myco::generative_model::{
    Content, ContentDelta, ContentStart, GenerateError, GenerateOutput, GenerativeModel, Message,
    MessagePart, ToolUseDelta, ToolUseStart,
};

/// Scripted model: each `generate` call yields the next pre-baked output.
pub struct ScriptedModel {
    scripts: Mutex<VecDeque<GenerateOutput>>,
}

impl ScriptedModel {
    pub fn new(scripts: Vec<GenerateOutput>) -> Arc<Self> {
        Arc::new(Self {
            scripts: Mutex::new(scripts.into()),
        })
    }

    /// How many scripted turns remain (for assertions).
    pub fn remaining(&self) -> usize {
        self.scripts.lock().expect("scripts lock").len()
    }
}

impl GenerativeModel for ScriptedModel {
    fn generate(
        &self,
        _input: &[Message],
    ) -> myco::core::AsyncStream<Result<MessagePart, GenerateError>> {
        let output = self
            .scripts
            .lock()
            .expect("scripts lock")
            .pop_front()
            .expect("scripted model ran out of outputs");

        let mut parts = vec![MessagePart::MessageStart];
        for (i, c) in output.content.iter().enumerate() {
            match c {
                Content::Text { text } => {
                    parts.push(MessagePart::ContentStart(ContentStart::Text { index: i }));
                    parts.push(MessagePart::ContentDelta(ContentDelta::Text {
                        index: i,
                        delta: text.clone(),
                    }));
                }
                Content::Image { source } => {
                    parts.push(MessagePart::ContentStart(ContentStart::Image { index: i }));
                    parts.push(MessagePart::ContentDelta(ContentDelta::Image {
                        index: i,
                        delta: source.clone(),
                    }));
                }
                Content::Thinking {
                    text,
                    signature,
                    redacted,
                } => {
                    parts.push(MessagePart::ContentStart(ContentStart::Thinking {
                        index: i,
                        signature: signature.clone(),
                        redacted: *redacted,
                    }));
                    if !text.is_empty() && !*redacted {
                        parts.push(MessagePart::ContentDelta(ContentDelta::Thinking {
                            index: i,
                            delta: text.clone(),
                        }));
                    }
                }
            }
        }
        for (i, tu) in output.tool_uses.iter().enumerate() {
            parts.push(MessagePart::ToolUseStart(ToolUseStart {
                index: i,
                id: tu.id.clone(),
                name: tu.name.clone(),
            }));
            parts.push(MessagePart::ToolUseDelta(ToolUseDelta {
                index: i,
                input_json_delta: tu.input.to_string(),
            }));
        }
        parts.push(MessagePart::TurnEndReason(output.turn_end_reason));

        Box::pin(stream::iter(parts.into_iter().map(Ok)))
    }
}
