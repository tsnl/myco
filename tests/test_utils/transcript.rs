//! Human-readable rendering of an agent conversation history (transcript).

use myco::generative_model::{Content, Message, ToolResult, ToolUse};

/// Render the full agent history as a stable, inspectable transcript.
pub fn format_transcript(history: &[Message]) -> String {
    let mut out = String::new();
    for (i, msg) in history.iter().enumerate() {
        out.push_str(&format!("===== message[{i}] {} =====\n", message_kind(msg)));
        match msg {
            Message::UserMessage { content } => {
                for c in content {
                    out.push_str(&format_content(c));
                }
            }
            Message::AssistantMessage {
                content,
                tool_uses,
                turn_end_reason,
            } => {
                for c in content {
                    out.push_str(&format_content(c));
                }
                for tu in tool_uses {
                    out.push_str(&format_tool_use(tu));
                }
                if let Some(reason) = turn_end_reason {
                    out.push_str(&format!("turn_end_reason: {reason:?}\n"));
                }
            }
            Message::ToolResults { tool_use_results } => {
                for tr in tool_use_results {
                    out.push_str(&format_tool_result(tr));
                }
            }
        }
        out.push('\n');
    }
    out
}

fn message_kind(msg: &Message) -> &'static str {
    match msg {
        Message::UserMessage { .. } => "user",
        Message::AssistantMessage { .. } => "assistant",
        Message::ToolResults { .. } => "tool_results",
    }
}

fn format_content(c: &Content) -> String {
    match c {
        Content::Text { text } => format!("text: {text}\n"),
        Content::Image { .. } => "image: <omitted>\n".into(),
        Content::Thinking { text, redacted, .. } => {
            if *redacted {
                "thinking: <redacted>\n".into()
            } else {
                format!("thinking: {text}\n")
            }
        }
    }
}

fn format_tool_use(tu: &ToolUse) -> String {
    format!(
        "tool_use id={} name={} input={}\n",
        tu.id, tu.name, tu.input
    )
}

fn format_tool_result(tr: &ToolResult) -> String {
    let mut s = format!("tool_result id={} is_error={}\n", tr.id, tr.is_error);
    for c in &tr.content {
        for line in format_content(c).lines() {
            s.push_str(&format!("  {line}\n"));
        }
    }
    s
}
