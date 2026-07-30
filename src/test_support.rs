#![allow(dead_code)]
//! Shared fixtures and doubles for unit tests: a scripted [`GenerativeModel`],
//! message/conversation builders, tool-result text extraction, RAII temp-dir
//! and `MYCO_HOME` guards, and [`MessagePart`] stream assertions.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use futures::stream;
use serde_json::json;

use crate::core::AsyncStream;
use crate::generative_model::{
    Content, ContentDelta, ContentStart, GenerateError, GenerateOutput, GenerativeModel, Message,
    MessagePart, ToolResult, ToolUse, ToolUseDelta, ToolUseStart, TurnEndReason,
};

// ---------------------------------------------------------------------------
// ScriptedModel
// ---------------------------------------------------------------------------

/// Scripted model: each `generate` call consumes the next pre-baked
/// [`GenerateOutput`] (FIFO) and replays it as a stream of [`MessagePart`]s —
/// same shape the agent sees from a real provider. Once the scripts drain,
/// every call yields the [`Self::then_fail`] error, or panics if none is set.
pub(crate) struct ScriptedModel {
    scripts: Mutex<VecDeque<GenerateOutput>>,
    fail: Mutex<Option<GenerateError>>,
}

impl ScriptedModel {
    pub(crate) fn new(scripts: Vec<GenerateOutput>) -> Arc<Self> {
        Arc::new(Self {
            scripts: Mutex::new(scripts.into()),
            fail: Mutex::new(None),
        })
    }

    /// Fail every call after the scripts drain (`new(vec![]).then_fail(err)`
    /// is a model that always fails).
    pub(crate) fn then_fail(self: Arc<Self>, err: GenerateError) -> Arc<Self> {
        *self.fail.lock().expect("fail lock") = Some(err);
        self
    }

    /// How many scripted turns remain (for assertions).
    pub(crate) fn remaining(&self) -> usize {
        self.scripts.lock().expect("scripts lock").len()
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

// ---------------------------------------------------------------------------
// Message / conversation fixtures
// ---------------------------------------------------------------------------

pub(crate) fn user(text: &str) -> Message {
    Message::UserMessage {
        content: vec![Content::Text { text: text.into() }],
    }
}

pub(crate) fn assistant(text: &str) -> Message {
    Message::AssistantMessage {
        content: vec![Content::Text { text: text.into() }],
        tool_uses: vec![],
        turn_end_reason: Some(TurnEndReason::EndTurn),
    }
}

pub(crate) fn assistant_tool(
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

pub(crate) fn tool_results(results: &[(&str, &str)]) -> Message {
    Message::ToolResults {
        tool_use_results: results
            .iter()
            .map(|(id, text)| ToolResult::text(*text).with_id(*id))
            .collect(),
    }
}

/// A thinking block as session history stores it (plaintext, unsigned).
pub(crate) fn thinking(text: &str) -> Content {
    Content::Thinking {
        text: text.into(),
        signature: None,
        redacted: false,
    }
}

pub(crate) fn thinking_msg(parts: &[&str]) -> Message {
    Message::AssistantMessage {
        content: parts.iter().map(|t| thinking(t)).collect(),
        tool_uses: vec![],
        turn_end_reason: Some(TurnEndReason::EndTurn),
    }
}

/// Canonical complete tool loop: user → assistant tool call → tool result →
/// final assistant answer.
pub(crate) fn tool_loop() -> Vec<Message> {
    vec![
        user("hello"),
        assistant_tool(
            Some("hi there"),
            "t1",
            "bash",
            json!({"command": "echo hi"}),
        ),
        tool_results(&[("t1", "hi\n")]),
        assistant("done"),
    ]
}

// ---------------------------------------------------------------------------
// ToolResult text extraction
// ---------------------------------------------------------------------------

pub(crate) fn text_parts(result: &ToolResult) -> Vec<String> {
    result
        .content
        .iter()
        .filter_map(|c| match c {
            Content::Text { text } => Some(text.clone()),
            _ => None,
        })
        .collect()
}

pub(crate) fn result_text(result: &ToolResult) -> String {
    text_parts(result).join("\n")
}

// ---------------------------------------------------------------------------
// Filesystem / environment guards
// ---------------------------------------------------------------------------

/// Fresh directory under the system temp root, removed on drop.
pub(crate) struct TempDir(PathBuf);

impl TempDir {
    pub(crate) fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub(crate) fn temp_dir(tag: &str) -> TempDir {
    let dir = std::env::temp_dir().join(format!("myco-{tag}-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    TempDir(dir)
}

/// Points `MYCO_HOME` at a fresh temp dir for the guard's lifetime, holding
/// [`crate::session::lock_myco_home_for_test`] so mutating tests serialize.
/// Cleanup is RAII so a panicking test cannot leak the override to the next.
pub(crate) struct TempHome {
    dir: PathBuf,
    _lock: MutexGuard<'static, ()>,
}

impl TempHome {
    pub(crate) fn path(&self) -> &Path {
        &self.dir
    }
}

impl Drop for TempHome {
    fn drop(&mut self) {
        // Runs before the lock guard field drops, so the env var is cleared
        // while `MYCO_HOME` is still exclusively ours.
        // SAFETY: test-only env override; held under the myco-home lock.
        unsafe {
            std::env::remove_var("MYCO_HOME");
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

pub(crate) fn temp_home(tag: &str) -> TempHome {
    let lock = crate::session::lock_myco_home_for_test();
    let dir = std::env::temp_dir().join(format!("myco-{tag}-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    // SAFETY: test-only env override; held under the myco-home lock.
    unsafe {
        std::env::set_var("MYCO_HOME", &dir);
    }
    TempHome { dir, _lock: lock }
}

// ---------------------------------------------------------------------------
// MessagePart assertions (driver stream tests)
// ---------------------------------------------------------------------------

#[track_caller]
pub(crate) fn expect_text_delta(part: &MessagePart, index: usize, text: &str) {
    match part {
        MessagePart::ContentDelta(ContentDelta::Text { index: i, delta })
            if *i == index && delta == text => {}
        other => panic!("expected text delta {index}/{text:?}, got {other:?}"),
    }
}

#[track_caller]
pub(crate) fn expect_thinking_delta(part: &MessagePart, index: usize, text: &str) {
    match part {
        MessagePart::ContentDelta(ContentDelta::Thinking { index: i, delta })
            if *i == index && delta == text => {}
        other => panic!("expected thinking delta {index}/{text:?}, got {other:?}"),
    }
}

#[track_caller]
pub(crate) fn expect_tool_start(part: &MessagePart, index: usize, id: &str, name: &str) {
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
pub(crate) fn expect_tool_args_delta(part: &MessagePart, index: usize, fragment: &str) {
    match part {
        MessagePart::ToolUseDelta(ToolUseDelta {
            index: i,
            input_json_delta,
        }) if *i == index && input_json_delta == fragment => {}
        other => panic!("expected tool args delta {index}/{fragment:?}, got {other:?}"),
    }
}

#[track_caller]
pub(crate) fn expect_turn_end(part: &MessagePart, reason: TurnEndReason) {
    match part {
        MessagePart::TurnEndReason(got) if *got == reason => {}
        other => panic!("expected turn end {reason:?}, got {other:?}"),
    }
}
