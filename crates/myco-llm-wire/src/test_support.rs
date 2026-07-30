//! Fixtures for testing against these types: conversation builders and
//! [`MessagePart`] stream assertions.
//!
//! Enable with the `test-util` feature, normally as a dev-dependency:
//!
//! ```toml
//! [dev-dependencies]
//! myco-myco-llm-wire = { version = "0.1", features = ["test-util"] }
//! ```

use crate::{
    Content, ContentDelta, Message, MessagePart, ToolResult, ToolUse, ToolUseDelta, ToolUseStart,
    TurnEndReason,
};

// ---------------------------------------------------------------------------
// Conversation fixtures
// ---------------------------------------------------------------------------

/// A user turn carrying one text block.
pub fn user(text: &str) -> Message {
    Message::UserMessage {
        content: vec![Content::Text { text: text.into() }],
    }
}

/// An assistant turn that ends cleanly with one text block.
pub fn assistant(text: &str) -> Message {
    Message::AssistantMessage {
        content: vec![Content::Text { text: text.into() }],
        tool_uses: vec![],
        turn_end_reason: Some(TurnEndReason::EndTurn),
    }
}

/// An assistant turn that stops to call one tool, with optional leading text.
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

/// The `(tool_use_id, text)` results answering a tool-calling turn.
pub fn tool_results(results: &[(&str, &str)]) -> Message {
    Message::ToolResults {
        tool_use_results: results
            .iter()
            .map(|(id, text)| ToolResult::text(*text).with_id(*id))
            .collect(),
    }
}

/// A thinking block as stored history holds it (plaintext, unsigned).
pub fn thinking(text: &str) -> Content {
    Content::Thinking {
        text: text.into(),
        signature: None,
        redacted: false,
    }
}

// ---------------------------------------------------------------------------
// MessagePart assertions (driver stream tests)
// ---------------------------------------------------------------------------

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
