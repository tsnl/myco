use crate::*;

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

pub(crate) fn assistant_tool(text: Option<&str>, name: &str, input: serde_json::Value) -> Message {
    Message::AssistantMessage {
        content: text
            .map(|t| vec![Content::Text { text: t.into() }])
            .unwrap_or_default(),
        tool_uses: vec![ToolUse {
            name: name.into(),
            input,
        }],
        turn_end_reason: Some(TurnEndReason::ToolUse),
    }
}

pub(crate) fn tool_results(results: &[&str]) -> Message {
    Message::ToolResults {
        tool_use_results: results.iter().map(|text| ToolResult::text(*text)).collect(),
    }
}

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
pub(crate) fn expect_tool_start(part: &MessagePart, index: usize, name: &str) {
    match part {
        MessagePart::ToolUseStart(ToolUseStart {
            index: i,
            name: got_name,
        }) if *i == index && got_name == name => {}
        other => panic!("expected tool start {index}/{name}, got {other:?}"),
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
