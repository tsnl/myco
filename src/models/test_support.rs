//! In-crate test helpers. These duplicate a slice of `myco-test-support`
//! deliberately: using that crate here would link a second build of
//! `myco-models` into our own tests (the dev-dependency-cycle type-identity
//! trap), so message-shaped helpers live in-crate.

#![allow(dead_code)]

use crate::models::{
    Content, ContentDelta, Message, MessagePart, ToolResult, ToolUse, ToolUseDelta, ToolUseStart,
    TurnEndReason,
};

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
