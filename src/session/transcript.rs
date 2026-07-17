//! Sectioned transcript layout for session restore and CLI display.
//!
//! Headed sections in the UI: USER (double rule), ASSISTANT (thin rule), and
//! ERROR (thin rule). Thinking summaries and tool invocations are paragraphs
//! inside ASSISTANT. ERROR is used for live generate failures (not stored in
//! session history).

use std::io::Write;

use crate::generative_model::{Content, Message};

/// Double-line 72-col rule before each user turn (UTF-8 box drawing, no ANSI).
pub const USER_RULE: &str =
    "════════════════════════════════════════════════════════════════════════";

/// Thin 72-col rule before ASSISTANT / ERROR section headers (USER uses USER_RULE).
pub const SECTION_RULE: &str =
    "────────────────────────────────────────────────────────────────────────";

/// Max chars for string values inside pretty-printed tool inputs (display only).
pub const TOOL_DISPLAY_STRING_MAX: usize = 72;

/// Write an ASSISTANT section open: blank line, thin rule, header, blank line, then body.
pub fn write_assistant_open(out: &mut (impl Write + ?Sized)) -> std::io::Result<()> {
    writeln!(out)?;
    writeln!(out, "{SECTION_RULE}")?;
    writeln!(out, "ASSISTANT")?;
    writeln!(out)?;
    Ok(())
}

/// Write an ERROR section open: blank line, thin rule, header, blank line, then body.
pub fn write_error_open(out: &mut (impl Write + ?Sized)) -> std::io::Result<()> {
    writeln!(out)?;
    writeln!(out, "{SECTION_RULE}")?;
    writeln!(out, "ERROR")?;
    writeln!(out)?;
    Ok(())
}

/// Write `text` with exactly one trailing newline.
pub fn write_block(out: &mut impl Write, text: &str) -> std::io::Result<()> {
    out.write_all(text.as_bytes())?;
    if !text.ends_with('\n') {
        out.write_all(b"\n")?;
    }
    Ok(())
}

/// Write a full ERROR section with body text (trailing newline ensured).
pub fn write_error_section(out: &mut impl Write, text: &str) -> std::io::Result<()> {
    write_error_open(out)?;
    write_block(out, text)
}

pub fn ensure_assistant(out: &mut impl Write, open: &mut bool) -> std::io::Result<()> {
    if !*open {
        write_assistant_open(out)?;
        *open = true;
    }
    Ok(())
}

/// Replay saved messages with the same section layout as the live REPL.
///
/// Only USER / ASSISTANT headers. Thinking summaries and tools are paragraphs
/// inside ASSISTANT. Thinking is stored in session history for resume, but
/// backends strip it when composing API requests. Live ERROR sections are not
/// part of history and are not replayed here.
pub fn write_session_history(out: &mut impl Write, messages: &[Message]) -> std::io::Result<()> {
    let mut assistant_open = false;
    // True when the ASSISTANT body already has a finished paragraph (text or tool).
    let mut need_blank = false;

    for msg in messages {
        match msg {
            Message::UserMessage { content } => {
                let text = content
                    .iter()
                    .filter_map(|c| match c {
                        Content::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("");
                if text.is_empty() {
                    continue;
                }
                writeln!(out, "{USER_RULE}")?;
                writeln!(out, "USER")?;
                writeln!(out)?;
                write_block(out, &text)?;
                // Next assistant turn opens a fresh ASSISTANT section.
                assistant_open = false;
                need_blank = false;
            }
            Message::AssistantMessage {
                content, tool_uses, ..
            } => {
                for c in content {
                    match c {
                        Content::Text { text } if !text.is_empty() => {
                            ensure_assistant(out, &mut assistant_open)?;
                            if need_blank {
                                writeln!(out)?;
                            }
                            write_block(out, text)?;
                            need_blank = true;
                        }
                        Content::Thinking { text, redacted, .. } => {
                            let body = if *redacted {
                                "[redacted]".to_string()
                            } else if text.is_empty() {
                                continue;
                            } else {
                                text.clone()
                            };
                            ensure_assistant(out, &mut assistant_open)?;
                            if need_blank {
                                writeln!(out)?;
                            }
                            // Same shape as the live sink: one `Thinking: …` paragraph.
                            write_block(out, &format!("Thinking: {body}"))?;
                            need_blank = true;
                        }
                        _ => {}
                    }
                }
                for tu in tool_uses {
                    ensure_assistant(out, &mut assistant_open)?;
                    if need_blank {
                        writeln!(out)?;
                    }
                    write!(out, "{}", format_tool_invocation(&tu.name, &tu.input))?;
                    need_blank = true;
                }
            }
            Message::ToolResults { .. } => {}
        }
    }
    Ok(())
}

pub fn print_session_history(messages: &[Message]) {
    let mut out = std::io::stdout();
    let _ = write_session_history(&mut out, messages);
    let _ = out.flush();
}

/// Truncate a display string to `max_chars` (including a trailing `…` when shortened).
pub fn truncate_display_string(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let trimmed: String = s.chars().take(max_chars.saturating_sub(1)).collect();
    format!("{trimmed}…")
}

/// Deep-copy JSON, replacing long string values with truncated versions for display.
pub fn truncate_json_strings(value: &serde_json::Value, max_chars: usize) -> serde_json::Value {
    match value {
        serde_json::Value::String(s) => {
            serde_json::Value::String(truncate_display_string(s, max_chars))
        }
        serde_json::Value::Array(items) => serde_json::Value::Array(
            items
                .iter()
                .map(|v| truncate_json_strings(v, max_chars))
                .collect(),
        ),
        serde_json::Value::Object(map) => {
            let mut out = serde_json::Map::with_capacity(map.len());
            for (k, v) in map {
                out.insert(k.clone(), truncate_json_strings(v, max_chars));
            }
            serde_json::Value::Object(out)
        }
        other => other.clone(),
    }
}

/// Render `name(<pretty json>)` for tool paragraphs inside ASSISTANT.
///
/// Long string values are truncated first, then objects/arrays are pretty-printed
/// with 2-space indent; scalars stay compact. Always ends with a trailing newline.
pub fn format_tool_invocation(name: &str, input: &serde_json::Value) -> String {
    let display = truncate_json_strings(input, TOOL_DISPLAY_STRING_MAX);
    match &display {
        serde_json::Value::Object(_) | serde_json::Value::Array(_) => {
            let pretty =
                serde_json::to_string_pretty(&display).unwrap_or_else(|_| display.to_string());
            format!("{name}({pretty})\n")
        }
        other => format!("{name}({other})\n"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::generative_model::{Content, Message, ToolResult, ToolUse, TurnEndReason};
    use serde_json::json;

    fn sample_messages() -> Vec<Message> {
        vec![
            Message::UserMessage {
                content: vec![Content::Text {
                    text: "hello".into(),
                }],
            },
            Message::AssistantMessage {
                content: vec![Content::Text {
                    text: "hi there".into(),
                }],
                tool_uses: vec![ToolUse {
                    id: "toolu_1".into(),
                    name: "bash".into(),
                    input: json!({"command": "echo hi"}),
                }],
                turn_end_reason: Some(TurnEndReason::ToolUse),
            },
            Message::ToolResults {
                tool_use_results: vec![ToolResult {
                    id: "toolu_1".into(),
                    content: vec![Content::Text {
                        text: "hi\n".into(),
                    }],
                    is_error: false,
                }],
            },
            Message::AssistantMessage {
                content: vec![Content::Text {
                    text: "done".into(),
                }],
                tool_uses: vec![],
                turn_end_reason: Some(TurnEndReason::EndTurn),
            },
            Message::AssistantMessage {
                content: vec![],
                tool_uses: vec![],
                turn_end_reason: Some(TurnEndReason::Other("Anthropic::PauseTurn".into())),
            },
        ]
    }

    #[test]
    fn write_session_history_section_layout() {
        let mut buf = Vec::new();
        write_session_history(&mut buf, &sample_messages()).unwrap();
        let rendered = String::from_utf8(buf).unwrap();

        assert!(rendered.contains(USER_RULE));
        assert!(rendered.contains("USER\n\nhello\n"));
        assert!(!rendered.contains("> hello"));
        // Tools live inside ASSISTANT (no TOOL header). One ASSISTANT open per turn.
        assert!(rendered.contains(&format!("{SECTION_RULE}\nASSISTANT\n\nhi there\n")));
        assert!(!rendered.contains("TOOL\n"));
        assert!(!rendered.contains("RESPONSE\n"));
        // Pretty-printed tool JSON as an ASSISTANT paragraph (blank line after text).
        assert!(rendered.contains("hi there\n\nbash({\n  \"command\": \"echo hi\"\n})\n"));
        // Blank line before section rule/header.
        assert!(rendered.contains("\n\n────────────────────────────────"));
        // Tool results silent.
        assert!(!rendered.contains("toolu_1"));
        // Multi-step assistant messages (tool loop) stay in one ASSISTANT section.
        assert!(rendered.contains("done\n"));
        assert_eq!(rendered.matches("ASSISTANT\n").count(), 1);
    }

    #[test]
    fn write_session_history_thinking_and_tools_in_assistant() {
        let messages = vec![
            Message::UserMessage {
                content: vec![Content::Text { text: "q".into() }],
            },
            Message::AssistantMessage {
                content: vec![
                    Content::Thinking {
                        text: "step a\nstep b".into(),
                        signature: None,
                        redacted: false,
                    },
                    Content::Text {
                        text: "answer".into(),
                    },
                ],
                tool_uses: vec![
                    ToolUse {
                        id: "1".into(),
                        name: "bash".into(),
                        input: json!({"command": "echo 1"}),
                    },
                    ToolUse {
                        id: "2".into(),
                        name: "bash".into(),
                        input: json!({"command": "echo 2"}),
                    },
                ],
                turn_end_reason: Some(TurnEndReason::ToolUse),
            },
        ];
        let mut buf = Vec::new();
        write_session_history(&mut buf, &messages).unwrap();
        let rendered = String::from_utf8(buf).unwrap();

        assert!(!rendered.contains("THINKING\n"));
        assert!(!rendered.contains("TOOL\n"));
        // Thinking replayed as an ASSISTANT paragraph (same prefix as live UI).
        assert!(rendered.contains(&format!(
            "{SECTION_RULE}\nASSISTANT\n\nThinking: step a\nstep b\n"
        )));
        assert!(rendered.contains("Thinking: step a\nstep b\n\nanswer\n"));
        // Tools are paragraphs inside ASSISTANT, blank-separated.
        assert!(rendered.contains("answer\n\nbash({\n  \"command\": \"echo 1\"\n})\n"));
        assert!(rendered.contains(")\n\nbash({\n  \"command\": \"echo 2\"\n})\n"));
        assert_eq!(rendered.matches("ASSISTANT\n").count(), 1);
        assert!(!rendered.contains("* "));
        assert!(!rendered.contains("+ Tool:"));
        assert!(!rendered.contains("[Tool]"));
    }

    #[test]
    fn write_error_section_layout() {
        let mut buf = Vec::new();
        write_error_section(&mut buf, "context length exceeded").unwrap();
        let rendered = String::from_utf8(buf).unwrap();
        assert!(rendered.contains(&format!(
            "{SECTION_RULE}\nERROR\n\ncontext length exceeded\n"
        )));
        // Leading blank line before the section rule.
        assert!(rendered.starts_with('\n'));
    }

    #[test]
    fn format_tool_invocation_pretty_prints_objects() {
        let rendered = format_tool_invocation(
            "bash",
            &json!({"action": "start", "session_id": "s", "timeout_ms": 1000}),
        );
        assert_eq!(
            rendered,
            "bash({\n  \"action\": \"start\",\n  \"session_id\": \"s\",\n  \"timeout_ms\": 1000\n})\n"
        );
        // Scalars stay compact.
        assert_eq!(format_tool_invocation("x", &json!(42)), "x(42)\n");
        assert_eq!(format_tool_invocation("x", &json!("hi")), "x(\"hi\")\n");
    }

    #[test]
    fn format_tool_invocation_truncates_long_strings() {
        let long = "a".repeat(TOOL_DISPLAY_STRING_MAX + 50);
        let rendered = format_tool_invocation(
            "write",
            &json!({
                "path": "f.txt",
                "content": long,
                "nested": { "blob": "b".repeat(TOOL_DISPLAY_STRING_MAX + 10) },
                "items": ["short", "c".repeat(TOOL_DISPLAY_STRING_MAX + 1)],
            }),
        );
        assert!(rendered.starts_with("write({"));
        assert!(rendered.contains("\"path\": \"f.txt\""));
        // Truncated values end with ellipsis inside the JSON string.
        assert!(rendered.contains('…'));
        // Full original length must not appear.
        assert!(!rendered.contains(&"a".repeat(TOOL_DISPLAY_STRING_MAX + 50)));
        // Short strings unchanged.
        assert!(rendered.contains("\"items\": [\n    \"short\","));
        // Scalar long string.
        let scalar =
            format_tool_invocation("echo", &json!("d".repeat(TOOL_DISPLAY_STRING_MAX + 5)));
        assert!(scalar.starts_with("echo(\""));
        assert!(scalar.contains('…'));
        assert!(!scalar.contains(&"d".repeat(TOOL_DISPLAY_STRING_MAX + 5)));
    }

    #[test]
    fn truncate_json_strings_leaves_short_values() {
        let v = json!({"n": 1, "s": "ok", "a": [true, null]});
        assert_eq!(truncate_json_strings(&v, 10), v);
    }

    #[test]
    fn thinking_blocks_are_replayed_from_history() {
        let messages = vec![Message::AssistantMessage {
            content: vec![
                Content::Thinking {
                    text: "secret-thought-aaa".into(),
                    signature: None,
                    redacted: false,
                },
                Content::Thinking {
                    text: "secret-thought-bbb".into(),
                    signature: None,
                    redacted: false,
                },
                Content::Text {
                    text: "done".into(),
                },
            ],
            tool_uses: vec![],
            turn_end_reason: Some(TurnEndReason::EndTurn),
        }];
        let mut buf = Vec::new();
        write_session_history(&mut buf, &messages).unwrap();
        let rendered = String::from_utf8(buf).unwrap();
        assert!(!rendered.contains("THINKING\n"));
        assert!(rendered.contains("Thinking: secret-thought-aaa\n"));
        assert!(rendered.contains("Thinking: secret-thought-bbb\n"));
        // Blank line between consecutive thinking paragraphs.
        assert!(
            rendered.contains("Thinking: secret-thought-aaa\n\nThinking: secret-thought-bbb\n")
        );
        assert!(rendered.contains("Thinking: secret-thought-bbb\n\ndone\n"));
        assert!(rendered.contains("ASSISTANT\n"));
    }
}
