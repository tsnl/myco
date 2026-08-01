//! Sectioned transcript layout for session restore and CLI display.
//!
//! Headed sections in the UI: USER (double rule), ASSISTANT (thin rule),
//! MYCO (thin rule, bold-uncolored), ERROR (thin rule), and WARNING (thin
//! rule). Thinking summaries and tool invocations are paragraphs inside
//! ASSISTANT. MYCO carries myco's own meta-command responses, ERROR live
//! generate/command failures, WARNING startup preflight problems; all three
//! are live-only (not stored in session history).
//!
//! Layout is built as [`TuiEvent`]s on the same section/paragraph helpers the
//! live producer uses ([`crate::tui::TuiProducer`]), so replay and live output
//! share one policy; the `Write`-based functions here are encoding facades
//! ([`crate::tui::encode_ansi`]) for buffered callers and tests.

use std::io::Write;

use super::markdown::{MarkdownRenderer, render_block};
use crate::generative_model::{Content, Message, TokenUsage};
use crate::tui::{
    SectionState, Style, TuiEvent, encode_ansi, encoded_ends_with_newline, section_open_events,
    styled_line, tool_invocation_events,
};

/// Thin 72-col rule before ASSISTANT / MYCO / ERROR / WARNING section headers
/// (USER uses [`user_rule`]).
pub const SECTION_RULE: &str =
    "────────────────────────────────────────────────────────────────────────";

/// Rule width when wrap is off (matches [`SECTION_RULE`]).
pub const DEFAULT_RULE_WIDTH: usize = 72;

/// Full-block startup-banner rule sized to the wrap width (default-width when
/// wrap is off) — the heaviest rule in the UI (banner `█` > user `═` > section
/// `─`), so launch stands out even uncolored. Block element rather than box
/// drawing: the box-drawing heavy line `━` is not reliably thicker than the
/// double `═` across terminal fonts.
pub fn banner_rule(wrap: Option<usize>) -> String {
    "█".repeat(wrap.unwrap_or(DEFAULT_RULE_WIDTH))
}

/// Double-line rule before each user turn (UTF-8 box drawing, no ANSI), sized
/// to the wrap width (default-width when wrap is off).
pub fn user_rule(wrap: Option<usize>) -> String {
    "═".repeat(wrap.unwrap_or(DEFAULT_RULE_WIDTH))
}

/// ASSISTANT / MYCO / ERROR / WARNING rule sized to the wrap width.
pub fn section_rule(wrap: Option<usize>) -> String {
    "─".repeat(wrap.unwrap_or(DEFAULT_RULE_WIDTH))
}

/// Compact token count for header chrome: `812`, `63.8k`, `200k`, `1.2M`.
pub fn format_tokens(n: u64) -> String {
    fn scale(n: u64, div: f64, suffix: &str) -> String {
        let v = n as f64 / div;
        let s = if v >= 100.0 {
            format!("{v:.0}")
        } else {
            format!("{v:.1}")
        };
        format!("{}{suffix}", s.strip_suffix(".0").unwrap_or(&s))
    }
    if n < 1_000 {
        n.to_string()
    } else if n < 1_000_000 {
        scale(n, 1_000.0, "k")
    } else {
        scale(n, 1_000_000.0, "M")
    }
}

/// `USER <used>/<max> (<pct>%)` header line. `used = None` (resumed session
/// predating usage tracking) renders `?` and omits the percentage.
pub fn user_header_line(used: Option<u64>, max: u64) -> String {
    match used {
        Some(used) => format!(
            "USER {}/{} ({}%)",
            format_tokens(used),
            format_tokens(max),
            used * 100 / max.max(1),
        ),
        None => format!("USER ?/{}", format_tokens(max)),
    }
}

/// `⚙`-prefixed usage line under the USER header, describing the turn that
/// just finished: input is the prompt of that turn's final request (≈ live
/// context), output is summed across the turn's requests. Zero cached counts
/// are elided. Pairs with the `●` running-tool lines printed below it.
pub fn usage_line(u: TokenUsage) -> String {
    let mut line = format!("⚙ last turn: input {}", format_tokens(u.input_tokens));
    if u.cached_input_tokens > 0 {
        line.push_str(&format!(
            " ({} cached)",
            format_tokens(u.cached_input_tokens)
        ));
    }
    line.push_str(&format!(" · output {}", format_tokens(u.output_tokens)));
    line
}

/// Max chars for string values inside pretty-printed tool inputs (display only).
pub const TOOL_DISPLAY_STRING_MAX: usize = 72;

/// ANSI styling and wrap width for transcript rendering. Disabled styling +
/// no wrap → byte-identical plain output, so files, logs, and piped stdout
/// never carry escape codes. The CLI resolves both once at startup
/// ([`crate::config::Config::colors_enabled`] /
/// [`crate::config::Config::wrap_width`]).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Palette {
    pub enabled: bool,
    /// Word-wrap prose (and size rules) to this column width; `None` = off.
    pub wrap: Option<usize>,
}

impl Palette {
    /// No styling: session files, non-TTY output.
    pub const fn plain() -> Self {
        Self {
            enabled: false,
            wrap: None,
        }
    }

    pub const fn colored(enabled: bool) -> Self {
        Self {
            enabled,
            wrap: None,
        }
    }

    pub const fn with_wrap(self, wrap: Option<usize>) -> Self {
        Self { wrap, ..self }
    }
}

/// Banner open events: full-block rule, bold title, blank line.
///
/// The banner counterpart of the section openers, shared by the startup
/// (`MYCO`) and compaction (`COMPACTED`) banners. Two deliberate differences
/// from a section: bold-uncolored rather than the USER/ASSISTANT palette, and
/// **no leading blank line** — a banner begins a screen instead of separating
/// itself from the block before it.
pub fn banner_open_events(title: &str, wrap: Option<usize>) -> Vec<TuiEvent> {
    let mut events = Vec::new();
    styled_line(&mut events, Style::BANNER, &banner_rule(wrap));
    styled_line(&mut events, Style::BANNER, title);
    events.push(TuiEvent::Text("\n".into()));
    events
}

/// The full COMPACTED banner: banner open, then the successor/predecessor
/// pair, tail size, and summary path.
///
/// Banner family rather than a USER/ASSISTANT section because a compaction *is*
/// a fresh start — the same open the startup banner uses, printed onto the
/// cleared screen.
pub fn compacted_banner_events(
    outcome: &crate::session::CompactOutcome,
    wrap: Option<usize>,
) -> Vec<TuiEvent> {
    let mut events = banner_open_events("COMPACTED", wrap);
    events.push(TuiEvent::Text(format!(
        "Session: {}\nFrom: {}\nKept: {} {}\nSummary: {}\n",
        outcome.successor_id,
        outcome.predecessor_id,
        outcome.tail_messages,
        if outcome.tail_messages == 1 {
            "message"
        } else {
            "messages"
        },
        outcome.summary_path.display(),
    )));
    events
}

/// Write a WARNING section with body text: blank line, thin rule, header,
/// blank line, then the body. Callers pass plain problem lines (startup
/// preflight); the palette styles the rule and header only.
pub fn write_warning_section(
    out: &mut (impl Write + ?Sized),
    text: &str,
    palette: Palette,
) -> std::io::Result<()> {
    write_section(out, Style::WARNING, "WARNING", text, palette)
}

/// Write a full ERROR section with body text (trailing newline ensured).
pub fn write_error_section(
    out: &mut (impl Write + ?Sized),
    text: &str,
    palette: Palette,
) -> std::io::Result<()> {
    write_section(out, Style::ERROR, "ERROR", text, palette)
}

fn write_section(
    out: &mut (impl Write + ?Sized),
    style: Style,
    header: &str,
    text: &str,
    palette: Palette,
) -> std::io::Result<()> {
    let mut events = Vec::new();
    section_open_events(&mut events, style, header, palette.wrap);
    let body = if text.ends_with('\n') {
        text.to_string()
    } else {
        format!("{text}\n")
    };
    events.push(TuiEvent::Text(body));
    out.write_all(encode_ansi(&events, palette.enabled).as_bytes())
}

/// `[N image(s) attached]` note for a user message carrying images; `None`
/// when it has none. Image bytes are never printed. The live echo and replay
/// both print this line directly under the wrapped user text, so the two
/// paths stay byte-identical (the `@path` mentions in the text carry the
/// filenames).
pub fn attachment_note(content: &[Content]) -> Option<String> {
    let images = content
        .iter()
        .filter(|c| matches!(c, Content::Image { .. }))
        .count();
    match images {
        0 => None,
        1 => Some("[1 image attached]".into()),
        n => Some(format!("[{n} images attached]")),
    }
}

/// Event form of the saved-history replay: the same section/paragraph layout
/// the live producer streams ([`SectionState`]), built from stored messages.
///
/// Only USER / ASSISTANT headers. Thinking summaries and tools are paragraphs
/// inside ASSISTANT. Thinking is stored in session history for resume, but
/// backends strip it when composing API requests. Live ERROR sections are not
/// part of history and are not replayed. Storage keeps assistant content and
/// tool_uses as separate lists (interleaving is lost), so replay renders
/// content paragraphs first, then tool paragraphs, per message.
pub fn history_events(messages: &[Message], palette: Palette) -> Vec<TuiEvent> {
    let mut st = SectionState::new();
    let mut events = Vec::new();
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
                let note = attachment_note(content);
                if text.is_empty() && note.is_none() {
                    continue;
                }
                styled_line(&mut events, Style::USER, &user_rule(palette.wrap));
                styled_line(&mut events, Style::USER, "USER");
                events.push(TuiEvent::Text("\n".into()));
                // Wrap-only (no markdown styling): the user's own words replay
                // as typed, at the transcript width — same as the live echo.
                if !text.is_empty() {
                    let rendered = render_block(&text, Palette::plain().with_wrap(palette.wrap));
                    let ends_nl = rendered.ends_with('\n');
                    events.push(TuiEvent::Text(rendered));
                    if !ends_nl {
                        events.push(TuiEvent::Text("\n".into()));
                    }
                }
                if let Some(note) = note {
                    events.push(TuiEvent::Text(format!("{note}\n")));
                }
                // Next assistant turn opens a fresh ASSISTANT section.
                st = SectionState::new();
            }
            Message::AssistantMessage {
                content, tool_uses, ..
            } => {
                for c in content {
                    match c {
                        Content::Text { text } if !text.is_empty() => {
                            let mut r = MarkdownRenderer::new(palette);
                            let mut body = r.feed_events(text);
                            body.extend(r.finish_events());
                            replay_paragraph(&mut st, &mut events, body, palette);
                        }
                        Content::Thinking { text, redacted, .. } => {
                            let body = if *redacted {
                                "[redacted]".to_string()
                            } else if text.is_empty() {
                                continue;
                            } else {
                                text.clone()
                            };
                            // Same shape as the live stream: one dim
                            // `Thinking: …` paragraph.
                            let mut r = MarkdownRenderer::with_base(palette, "2");
                            let mut body = r.feed_events(&format!("Thinking: {body}"));
                            body.extend(r.finish_events());
                            replay_paragraph(&mut st, &mut events, body, palette);
                        }
                        _ => {}
                    }
                }
                for tu in tool_uses {
                    st.ensure_assistant(&mut events, palette.wrap);
                    st.separate_paragraph_if_needed(&mut events);
                    tool_invocation_events(&mut events, &tu.name, &tu.input);
                    st.at_line_start = true;
                    st.need_blank = true;
                }
            }
            Message::ToolResults { .. } => {}
        }
    }
    events
}

/// One finished paragraph inside ASSISTANT: open the section if needed,
/// blank-separate, emit the pre-rendered body with exactly one trailing
/// newline (decided on the encoded tail, as the terminal sees it).
fn replay_paragraph(
    st: &mut SectionState,
    events: &mut Vec<TuiEvent>,
    body: Vec<TuiEvent>,
    palette: Palette,
) {
    st.ensure_assistant(events, palette.wrap);
    st.separate_paragraph_if_needed(events);
    let ends_nl = encoded_ends_with_newline(&body, palette.enabled);
    events.extend(body);
    if !ends_nl {
        events.push(TuiEvent::Text("\n".into()));
    }
    st.at_line_start = true;
    st.need_blank = true;
}

/// Truncate a display string to `max_chars` (including a trailing `…` when shortened).
fn truncate_display_string(s: &str, max_chars: usize) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::generative_model::{Content, Message, ToolUse, TurnEndReason};
    use crate::test_support::{assistant, thinking, thinking_msg, tool_loop, user};
    use serde_json::json;

    /// Replay bytes as a terminal sees them: events, ANSI-encoded.
    fn render_history(messages: &[Message], palette: Palette) -> String {
        encode_ansi(&history_events(messages, palette), palette.enabled)
    }

    fn render_compacted_banner(
        outcome: &crate::session::CompactOutcome,
        palette: Palette,
    ) -> String {
        encode_ansi(
            &compacted_banner_events(outcome, palette.wrap),
            palette.enabled,
        )
    }

    fn render_tool_invocation(name: &str, input: &serde_json::Value, palette: Palette) -> String {
        let mut events = Vec::new();
        tool_invocation_events(&mut events, name, input);
        encode_ansi(&events, palette.enabled)
    }

    #[test]
    fn format_tokens_scales_and_drops_trailing_zero() {
        assert_eq!(format_tokens(0), "0");
        assert_eq!(format_tokens(999), "999");
        assert_eq!(format_tokens(1_000), "1k");
        assert_eq!(format_tokens(8_192), "8.2k");
        assert_eq!(format_tokens(63_841), "63.8k");
        assert_eq!(format_tokens(200_000), "200k");
        assert_eq!(format_tokens(1_000_000), "1M");
        assert_eq!(format_tokens(1_234_567), "1.2M");
    }

    #[test]
    fn user_header_line_shows_percent_only_when_usage_known() {
        assert_eq!(
            user_header_line(Some(63_841), 200_000),
            "USER 63.8k/200k (31%)"
        );
        assert_eq!(user_header_line(Some(0), 200_000), "USER 0/200k (0%)");
        assert_eq!(user_header_line(None, 200_000), "USER ?/200k");
    }

    #[test]
    fn usage_line_elides_zero_cached_counts() {
        let full = TokenUsage {
            input_tokens: 63_841,
            output_tokens: 1_400,
            cached_input_tokens: 58_000,
        };
        assert_eq!(
            usage_line(full),
            "⚙ last turn: input 63.8k (58k cached) · output 1.4k"
        );
        let uncached = TokenUsage {
            input_tokens: 500,
            output_tokens: 42,
            cached_input_tokens: 0,
        };
        assert_eq!(usage_line(uncached), "⚙ last turn: input 500 · output 42");
    }

    fn sample_outcome(tail_messages: usize) -> crate::session::CompactOutcome {
        crate::session::CompactOutcome {
            predecessor_id: "993d14889c414aab81963843cccf8090".into(),
            successor_id: "1c0ffee0dead0beef0000000000000aa".into(),
            summary_path: std::path::PathBuf::from("/home/u/.myco/sessions/993d1488.summary.md"),
            tail_messages,
        }
    }

    #[test]
    fn banner_open_layout() {
        let rendered = encode_ansi(&banner_open_events("MYCO", None), false);
        // No leading blank line — unlike the section openers, a banner starts a screen.
        assert_eq!(rendered, format!("{}\nMYCO\n\n", banner_rule(None)));

        let rendered = encode_ansi(&banner_open_events("COMPACTED", None), true);
        assert!(rendered.contains("\x1b[0;1mCOMPACTED\x1b[0m\n"));
        assert!(rendered.contains(&format!("\x1b[0;1m{}\x1b[0m\n", banner_rule(None))));
    }

    #[test]
    fn compacted_banner_layout() {
        let rendered = render_compacted_banner(&sample_outcome(7), Palette::plain());
        let expected = format!(
            "{rule}\nCOMPACTED\n\n\
             Session: 1c0ffee0dead0beef0000000000000aa\n\
             From: 993d14889c414aab81963843cccf8090\n\
             Kept: 7 messages\n\
             Summary: /home/u/.myco/sessions/993d1488.summary.md\n",
            rule = banner_rule(None)
        );
        assert_eq!(rendered, expected);
    }

    #[test]
    fn compacted_banner_singular_tail() {
        assert!(
            render_compacted_banner(&sample_outcome(1), Palette::plain())
                .contains("Kept: 1 message\n"),
            "single kept message should not be pluralized"
        );
    }

    #[test]
    fn compacted_banner_rule_follows_wrap_width() {
        let palette = Palette::plain().with_wrap(Some(40));
        let text = render_compacted_banner(&sample_outcome(2), palette);
        assert!(
            text.starts_with(&format!("{}\nCOMPACTED\n", "█".repeat(40))),
            "{text}"
        );
    }

    #[test]
    fn session_history_section_layout() {
        let rendered = render_history(&tool_loop(), Palette::plain());

        assert!(rendered.contains(&user_rule(None)));
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
        // Tool results silent (no tool-use id leaks).
        assert!(!rendered.contains("t1"));
        // Multi-step assistant messages (tool loop) stay in one ASSISTANT section.
        assert!(rendered.contains("done\n"));
        assert_eq!(rendered.matches("ASSISTANT\n").count(), 1);
    }

    #[test]
    fn session_history_thinking_and_tools_in_assistant() {
        let messages = vec![
            user("q"),
            Message::AssistantMessage {
                content: vec![
                    thinking("step a\nstep b"),
                    Content::Text {
                        text: "answer".into(),
                    },
                ],
                tool_uses: vec![
                    ToolUse {
                        name: "bash".into(),
                        input: json!({"command": "echo 1"}),
                    },
                    ToolUse {
                        name: "bash".into(),
                        input: json!({"command": "echo 2"}),
                    },
                ],
                turn_end_reason: Some(TurnEndReason::ToolUse),
            },
        ];
        let rendered = render_history(&messages, Palette::plain());

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
    fn attachment_note_counts_and_pluralizes() {
        let text = Content::Text { text: "hi".into() };
        let image = Content::Image {
            source: "data:image/png;base64,AA".into(),
        };
        assert_eq!(attachment_note(std::slice::from_ref(&text)), None);
        assert_eq!(
            attachment_note(&[image.clone(), text.clone()]).as_deref(),
            Some("[1 image attached]")
        );
        assert_eq!(
            attachment_note(&[image.clone(), image, text]).as_deref(),
            Some("[2 images attached]")
        );
    }

    #[test]
    fn user_images_replay_as_count_placeholder() {
        let messages = vec![Message::UserMessage {
            content: vec![
                Content::Image {
                    source: "data:image/png;base64,AAAA".into(),
                },
                Content::Text {
                    text: "look at @shot.png".into(),
                },
            ],
        }];
        let rendered = render_history(&messages, Palette::plain());
        assert!(rendered.contains("look at @shot.png\n[1 image attached]\n"));
        // The base64 payload never hits the terminal.
        assert!(!rendered.contains("AAAA"));
    }

    #[test]
    fn wrapped_palette_wraps_prose_and_sizes_rules() {
        let palette = Palette::plain().with_wrap(Some(20));
        let messages = vec![
            user("user words that go past twenty columns"),
            assistant("one two three four five six seven"),
        ];
        let rendered = render_history(&messages, palette);
        assert!(rendered.contains(&"═".repeat(20)), "{rendered}");
        assert!(!rendered.contains(&"═".repeat(21)), "{rendered}");
        assert!(
            rendered.contains("one two three four\nfive six seven\n"),
            "{rendered}"
        );
        // User text is wrapped too (wrap-only, no styling).
        assert!(
            rendered.contains("user words that go\npast twenty columns\n"),
            "{rendered}"
        );
        // Rule fns default to the fixed 72-col width when wrap is off.
        assert_eq!(banner_rule(None).chars().count(), DEFAULT_RULE_WIDTH);
        assert_eq!(user_rule(None).chars().count(), DEFAULT_RULE_WIDTH);
        assert_eq!(section_rule(None), SECTION_RULE);
    }

    #[test]
    fn write_error_section_layout() {
        let mut buf = Vec::new();
        write_error_section(&mut buf, "context length exceeded", Palette::plain()).unwrap();
        let rendered = String::from_utf8(buf).unwrap();
        assert!(rendered.contains(&format!(
            "{SECTION_RULE}\nERROR\n\ncontext length exceeded\n"
        )));
        // Leading blank line before the section rule.
        assert!(rendered.starts_with('\n'));
    }

    /// One header per section, whatever the body: the startup preflight folds
    /// several unrelated problems into a single block.
    #[test]
    fn write_warning_section_layout() {
        let body = "missing executable tmux: no session browser\nssh-agent: agent down\n";
        let mut buf = Vec::new();
        write_warning_section(&mut buf, body, Palette::plain()).unwrap();
        let rendered = String::from_utf8(buf).unwrap();
        assert_eq!(rendered, format!("\n{SECTION_RULE}\nWARNING\n\n{body}"));
        assert_eq!(rendered.matches("WARNING").count(), 1, "{rendered}");

        let mut buf = Vec::new();
        write_warning_section(&mut buf, body, Palette::colored(true)).unwrap();
        let rendered = String::from_utf8(buf).unwrap();
        assert!(rendered.contains("\x1b[0;1;33mWARNING\x1b[0m\n"));
        assert!(rendered.contains(&format!("\x1b[0;1;33m{SECTION_RULE}\x1b[0m\n")));
        // Body lines stay plain — the palette styles the rule and header only.
        assert!(rendered.ends_with(body), "{rendered}");
    }

    /// A body that already ends in a newline does not gain a second one.
    #[test]
    fn section_body_newline_is_ensured_not_doubled() {
        let render = |text: &str| {
            let mut buf = Vec::new();
            write_warning_section(&mut buf, text, Palette::plain()).unwrap();
            String::from_utf8(buf).unwrap()
        };
        assert_eq!(render("one line"), render("one line\n"));
        assert!(render("one line").ends_with("one line\n"));
    }

    #[test]
    fn tool_invocation_pretty_prints_objects() {
        let rendered = render_tool_invocation(
            "bash",
            &json!({"action": "start", "session_id": "s", "timeout_ms": 1000}),
            Palette::plain(),
        );
        assert_eq!(
            rendered,
            "bash({\n  \"action\": \"start\",\n  \"session_id\": \"s\",\n  \"timeout_ms\": 1000\n})\n"
        );
        // Scalars stay compact.
        assert_eq!(
            render_tool_invocation("x", &json!(42), Palette::plain()),
            "x(42)\n"
        );
        assert_eq!(
            render_tool_invocation("x", &json!("hi"), Palette::plain()),
            "x(\"hi\")\n"
        );
    }

    #[test]
    fn tool_invocation_truncates_long_strings() {
        let long = "a".repeat(TOOL_DISPLAY_STRING_MAX + 50);
        let rendered = render_tool_invocation(
            "write",
            &json!({
                "path": "f.txt",
                "content": long,
                "nested": { "blob": "b".repeat(TOOL_DISPLAY_STRING_MAX + 10) },
                "items": ["short", "c".repeat(TOOL_DISPLAY_STRING_MAX + 1)],
            }),
            Palette::plain(),
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
        let scalar = render_tool_invocation(
            "echo",
            &json!("d".repeat(TOOL_DISPLAY_STRING_MAX + 5)),
            Palette::plain(),
        );
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
                thinking("secret-thought-aaa"),
                thinking("secret-thought-bbb"),
                Content::Text {
                    text: "done".into(),
                },
            ],
            tool_uses: vec![],
            turn_end_reason: Some(TurnEndReason::EndTurn),
        }];
        let rendered = render_history(&messages, Palette::plain());
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

    #[test]
    fn plain_palette_emits_no_ansi() {
        let mut buf = Vec::new();
        write_error_section(&mut buf, "boom", Palette::plain()).unwrap();
        let mut rendered = render_history(&tool_loop(), Palette::plain());
        rendered.push_str(std::str::from_utf8(&buf).unwrap());
        rendered.push_str(&render_compacted_banner(
            &sample_outcome(3),
            Palette::plain(),
        ));
        assert!(!rendered.contains('\x1b'));
    }

    #[test]
    fn colored_palette_styles_headers_but_not_bodies() {
        let palette = Palette::colored(true);
        let rendered = render_history(&tool_loop(), palette);

        // Headers and rules are wrapped in SGR sequences…
        assert!(rendered.contains("\x1b[0;1;36mUSER\x1b[0m\n"));
        assert!(rendered.contains(&format!("\x1b[0;1;36m{}\x1b[0m\n", user_rule(None))));
        assert!(rendered.contains("\x1b[0;1;32mASSISTANT\x1b[0m\n"));
        // …while message bodies stay plain.
        assert!(rendered.contains("\nhello\n"));
        assert!(rendered.contains("\nhi there\n"));

        let mut buf = Vec::new();
        write_error_section(&mut buf, "boom", palette).unwrap();
        let rendered = String::from_utf8(buf).unwrap();
        assert!(rendered.contains("\x1b[0;1;31mERROR\x1b[0m\n\nboom\n"));

        // COMPACTED joins the banner family: bold, uncolored rule + title,
        // plain detail lines.
        let rendered = render_compacted_banner(&sample_outcome(3), palette);
        assert!(rendered.contains(&format!("\x1b[0;1m{}\x1b[0m\n", banner_rule(None))));
        assert!(rendered.contains("\x1b[0;1mCOMPACTED\x1b[0m\n"));
        assert!(rendered.contains("\nKept: 3 messages\n"));
    }

    #[test]
    fn colored_tool_invocation_styles_only_the_name() {
        let rendered = render_tool_invocation(
            "bash",
            &json!({"command": "echo hi"}),
            Palette::colored(true),
        );
        assert!(rendered.starts_with("\x1b[0;1;33mbash\x1b[0m({"));
        assert!(rendered.contains("\"command\": \"echo hi\""));
        assert!(!rendered.contains("echo hi\x1b"));
    }

    #[test]
    fn colored_thinking_paragraph_is_dimmed() {
        let rendered = render_history(&[thinking_msg(&["pondering"])], Palette::colored(true));
        assert!(rendered.contains("\x1b[0;2mThinking: pondering\x1b[0m\n"));
    }
}
