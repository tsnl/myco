//! Test doubles and message-shaped fixtures, ported from v2's
//! `test_support`. Compiled unconditionally so downstream crates' tests can
//! script model turns; nothing in the production paths reaches in here.

#![allow(dead_code)]

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use futures::stream;

use crate::{
    AsyncStream, Content, ContentDelta, ContentStart, GenerateError, GenerateOutput,
    GenerativeModel, Message, MessagePart, ToolResult, ToolUse, ToolUseDelta, ToolUseStart,
    TurnEndReason,
};

/// A [`GenerativeModel`] that replays a script of outputs as streams, in
/// order — the deterministic stand-in every turn-engine test drives.
pub struct ScriptedModel {
    scripts: Mutex<VecDeque<GenerateOutput>>,
    fail: Mutex<Option<GenerateError>>,
}

impl ScriptedModel {
    pub fn new(scripts: Vec<GenerateOutput>) -> Arc<Self> {
        Arc::new(Self {
            scripts: Mutex::new(scripts.into()),
            fail: Mutex::new(None),
        })
    }

    /// One plain-text, end-turn reply — the common case, spelled short.
    pub fn replying(texts: &[&str]) -> Arc<Self> {
        Self::new(texts.iter().map(|t| text_output(t)).collect())
    }

    /// Append an output after construction — for tests whose later turns
    /// depend on ids that exist only once the earlier turns have run.
    pub fn push(&self, output: GenerateOutput) {
        self.scripts.lock().expect("scripts lock").push_back(output);
    }

    /// Fail every call after the scripts drain (`new(vec![]).then_fail(err)`
    /// is a model that always fails).
    pub fn then_fail(self: Arc<Self>, err: GenerateError) -> Arc<Self> {
        *self.fail.lock().expect("fail lock") = Some(err);
        self
    }

    /// How many scripted turns remain (for assertions).
    pub fn remaining(&self) -> usize {
        self.scripts.lock().expect("scripts lock").len()
    }
}

/// A one-block plain-text [`GenerateOutput`] ending the turn.
pub fn text_output(text: &str) -> GenerateOutput {
    GenerateOutput {
        content: vec![Content::Text { text: text.into() }],
        tool_uses: vec![],
        turn_end_reason: TurnEndReason::EndTurn,
        usage: None,
    }
}

// [`GenerateError`] is not `Clone`; rebuild it so the configured failure can
// be replayed on every drained call.
fn clone_err(err: &GenerateError) -> GenerateError {
    match err {
        GenerateError::ExecutionError(m) => GenerateError::ExecutionError(m.clone()),
        GenerateError::RefusalError(m) => GenerateError::RefusalError(m.clone()),
        GenerateError::MalformedResponseError(m) => {
            GenerateError::MalformedResponseError(m.clone())
        }
        GenerateError::RequestTooLargeError(m) => GenerateError::RequestTooLargeError(m.clone()),
    }
}

impl GenerativeModel for ScriptedModel {
    fn generate(&self, _input: &[Message]) -> AsyncStream<Result<MessagePart, GenerateError>> {
        let Some(output) = self.scripts.lock().expect("scripts lock").pop_front() else {
            let err = self.fail.lock().expect("fail lock").as_ref().map(clone_err);
            let err = err.expect("scripted model ran out of outputs");
            return Box::pin(stream::once(async move { Err(err) }));
        };

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
        if let Some(usage) = output.usage {
            parts.push(MessagePart::Usage(usage));
        }
        parts.push(MessagePart::TurnEndReason(output.turn_end_reason));

        Box::pin(stream::iter(parts.into_iter().map(Ok)))
    }
}

/// A model whose stream opens and then hangs forever — the double for
/// cancel/interject tests, where the turn must still be *running* when the
/// verb lands.
pub struct HangingModel;

impl GenerativeModel for HangingModel {
    fn generate(&self, _input: &[Message]) -> AsyncStream<Result<MessagePart, GenerateError>> {
        use futures::StreamExt as _;
        Box::pin(stream::iter([Ok(MessagePart::MessageStart)]).chain(stream::pending()))
    }
}

pub fn user(text: &str) -> Message {
    Message::UserMessage {
        content: vec![Content::Text { text: text.into() }],
    }
}

pub fn assistant(text: &str) -> Message {
    Message::AssistantMessage {
        content: vec![Content::Text { text: text.into() }],
        tool_uses: vec![],
        turn_end_reason: Some(TurnEndReason::EndTurn),
    }
}

pub fn assistant_tool(
    text: Option<&str>,
    id: &str,
    name: &str,
    input: serde_json::Value,
) -> Message {
    Message::AssistantMessage {
        content: text
            .map(|t| vec![Content::Text { text: t.into() }])
            .unwrap_or_default(),
        tool_uses: vec![ToolUse {
            id: id.into(),
            name: name.into(),
            input,
        }],
        turn_end_reason: Some(TurnEndReason::ToolUse),
    }
}

pub fn tool_results(results: &[(&str, &str)]) -> Message {
    Message::ToolResults {
        tool_use_results: results
            .iter()
            .map(|(id, text)| ToolResult::text(*text).with_id(*id))
            .collect(),
    }
}

#[track_caller]
pub fn expect_text_delta(part: &MessagePart, index: usize, text: &str) {
    match part {
        MessagePart::ContentDelta(ContentDelta::Text { index: i, delta })
            if *i == index && delta == text => {}
        other => panic!("expected text delta {index}/{text:?}, got {other:?}"),
    }
}

#[track_caller]
pub fn expect_thinking_delta(part: &MessagePart, index: usize, text: &str) {
    match part {
        MessagePart::ContentDelta(ContentDelta::Thinking { index: i, delta })
            if *i == index && delta == text => {}
        other => panic!("expected thinking delta {index}/{text:?}, got {other:?}"),
    }
}

#[track_caller]
pub fn expect_tool_start(part: &MessagePart, index: usize, id: &str, name: &str) {
    match part {
        MessagePart::ToolUseStart(ToolUseStart {
            index: i,
            id: got_id,
            name: got_name,
        }) if *i == index && got_id == id && got_name == name => {}
        other => panic!("expected tool start {index}/{id}/{name}, got {other:?}"),
    }
}

#[track_caller]
pub fn expect_tool_args_delta(part: &MessagePart, index: usize, fragment: &str) {
    match part {
        MessagePart::ToolUseDelta(ToolUseDelta {
            index: i,
            input_json_delta,
        }) if *i == index && input_json_delta == fragment => {}
        other => panic!("expected tool args delta {index}/{fragment:?}, got {other:?}"),
    }
}

#[track_caller]
pub fn expect_turn_end(part: &MessagePart, reason: TurnEndReason) {
    match part {
        MessagePart::TurnEndReason(got) if *got == reason => {}
        other => panic!("expected turn end {reason:?}, got {other:?}"),
    }
}
