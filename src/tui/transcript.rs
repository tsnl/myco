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
    styled_line,
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

/// ANSI styling and wrap width for transcript rendering. Disabled styling +
/// no wrap → byte-identical plain output, so files, logs, and piped stdout
/// never carry escape codes. The CLI resolves color at startup via
/// [`crate::config::Config::colors_enabled`] and recomputes width from
/// [`crate::config::WrapMode`] as the terminal resizes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Palette {
    pub enabled: bool,
    /// Word-wrap prose (and size rules) to this column width; `None` = off.
    pub wrap: Option<usize>,
    /// Expand tool inputs and recorded output instead of showing a short preview.
    pub verbose: bool,
}

impl Palette {
    /// No styling: session files, non-TTY output.
    pub const fn plain() -> Self {
        Self {
            enabled: false,
            wrap: None,
            verbose: false,
        }
    }

    pub const fn colored(enabled: bool) -> Self {
        Self {
            enabled,
            wrap: None,
            verbose: false,
        }
    }

    pub const fn with_wrap(self, wrap: Option<usize>) -> Self {
        Self { wrap, ..self }
    }

    pub const fn with_verbose(self, verbose: bool) -> Self {
        Self { verbose, ..self }
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
        "Session: {}\nThread: {}\nFrom thread: {}\nKept: {} {}\nSummary: {}\n",
        outcome.session_id,
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
/// the live producer streams, built from stored messages.
///
/// Only USER / ASSISTANT headers. Thinking summaries and tools are paragraphs
/// inside ASSISTANT. Thinking is stored in session history for resume, but
/// backends strip it when composing API requests. Live ERROR sections are not
/// part of history and are not replayed. Storage keeps assistant content and
/// tool_uses as separate lists (interleaving is lost), so replay renders
/// content paragraphs first, then tool paragraphs, per message.
pub fn history_events(messages: &[Message], palette: Palette) -> Vec<TuiEvent> {
    history_events_at(messages, palette, &std::collections::BTreeMap::new())
}

pub fn turn_header(role: &str, time: Option<chrono::DateTime<chrono::Utc>>) -> String {
    format!(
        "{role} · {}",
        time.map(|time| time.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
            .unwrap_or_else(|| "unknown".into())
    )
}

pub fn history_events_at(
    messages: &[Message],
    palette: Palette,
    times: &std::collections::BTreeMap<usize, chrono::DateTime<chrono::Utc>>,
) -> Vec<TuiEvent> {
    let mut st = SectionState::new();
    let mut events = Vec::new();
    for (index, msg) in messages.iter().enumerate() {
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
                st.finish_tools(&mut events);
                let time = times.get(&index).copied();
                styled_line(&mut events, Style::USER, &user_rule(palette.wrap));
                styled_line(&mut events, Style::USER, &turn_header("USER", time));
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
                st.turn_time = time;
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
                    st.start_tool(&mut events, &tu.name, &tu.input, palette);
                    st.at_line_start = true;
                    st.need_blank = true;
                }
            }
            Message::ToolResults { tool_use_results } => {
                if let Some(Message::AssistantMessage { tool_uses, .. }) =
                    index.checked_sub(1).and_then(|i| messages.get(i))
                {
                    for (tool, result) in tool_uses.iter().zip(tool_use_results) {
                        st.tool_result(&mut events, tool, result, palette);
                    }
                }
            }
        }
    }
    st.finish_tools(&mut events);
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
        encode_ansi(&tool_events(name, input, palette), palette.enabled)
    }

    fn tool_events(name: &str, input: &serde_json::Value, palette: Palette) -> Vec<TuiEvent> {
        let mut events = Vec::new();
        let mut frame = super::super::tool_box::ToolBox::open(&mut events, name, palette);
        frame.input(&mut events, input);
        frame.close(&mut events);
        events
    }

    fn box_body(text: &str) -> String {
        text.lines()
            .filter_map(|line| line.strip_prefix("│ ")?.strip_suffix(" │"))
            .map(str::trim_end)
            .collect::<Vec<_>>()
            .join("\n")
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
            session_id: "session-id".into(),
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
             Session: session-id\n\
             Thread: 1c0ffee0dead0beef0000000000000aa\n\
             From thread: 993d14889c414aab81963843cccf8090\n\
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
        assert!(rendered.contains("USER · unknown\n\nhello\n"));
        assert!(!rendered.contains("> hello"));
        // Tools live inside ASSISTANT (no TOOL header). One ASSISTANT open per turn.
        assert!(rendered.contains(&format!(
            "{SECTION_RULE}\nASSISTANT · unknown\n\nhi there\n"
        )));
        assert!(!rendered.contains("TOOL\n"));
        assert!(!rendered.contains("RESPONSE\n"));
        // Tool commands are ASSISTANT paragraphs, separated from text.
        assert!(rendered.contains("hi there\n\n╭─ bash "));
        assert!(rendered.contains("\"command\": \"echo hi\""));
        // Blank line before section rule/header.
        assert!(rendered.contains("\n\n────────────────────────────────"));
        // Tool results silent (no tool-use id leaks).
        assert!(!rendered.contains("t1"));
        // Multi-step assistant messages (tool loop) stay in one ASSISTANT section.
        assert!(rendered.contains("done\n"));
        assert_eq!(rendered.matches("ASSISTANT · unknown\n").count(), 1);
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
            "{SECTION_RULE}\nASSISTANT · unknown\n\nThinking: step a\nstep b\n"
        )));
        assert!(rendered.contains("Thinking: step a\nstep b\n\nanswer\n"));
        // Tools are paragraphs inside ASSISTANT, blank-separated.
        assert!(rendered.contains("answer\n\n╭─ bash "));
        assert!(rendered.contains("\"command\": \"echo 1\""));
        assert!(rendered.contains("├─ bash "));
        assert!(rendered.contains("\"command\": \"echo 2\""));
        assert_eq!(rendered.matches("ASSISTANT · unknown\n").count(), 1);
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
    fn tool_argument_layout_and_styles_do_not_depend_on_the_tool_name() {
        let input = json!({
            "command": "printf '%s\\n' '路径'\necho done",
            "stdin": "first\nsecond\n",
            "host": "remote",
            "options": {"timeout_ms": 1000},
        });
        for verbose in [false, true] {
            for width in [8, 20, 72] {
                let palette = Palette::colored(true)
                    .with_wrap(Some(width))
                    .with_verbose(verbose);
                let render = |name| {
                    tool_events(name, &input, palette)
                        .into_iter()
                        .map(|event| match event {
                            TuiEvent::Text(text) if text.starts_with('╭') => {
                                TuiEvent::Text("tool heading".into())
                            }
                            event => event,
                        })
                        .collect::<Vec<_>>()
                };
                let expected = render("custom");
                for name in ["bash", "editor", "view_image", "session_meta"] {
                    assert_eq!(
                        render(name),
                        expected,
                        "{name}, width {width}, verbose {verbose}"
                    );
                }
            }
        }
    }

    #[test]
    fn verbose_arguments_preserve_scripts_stdin_and_options() {
        let command = format!(
            "cd '/a very long path/{}' && cat <<'EOF'\nhello \"world\"\nEOF",
            "x".repeat(100)
        );
        let input =
            json!({"command":command, "stdin":"echo done\n", "host":"devbox", "timeout_ms":5000});
        let original = input.clone();
        let rendered = render_tool_invocation(
            "bash",
            &input,
            Palette::plain().with_wrap(Some(240)).with_verbose(true),
        );
        assert_eq!(
            box_body(&rendered),
            serde_json::to_string_pretty(&input).unwrap()
        );
        assert_eq!(input, original);
    }

    #[test]
    fn tool_arguments_wrap_with_distinct_display_continuations() {
        let command = "cargo test --locked --workspace --lib\necho done";
        let input = json!({"command": command});
        let rendered = render_tool_invocation(
            "bash",
            &input,
            Palette::plain().with_wrap(Some(30)).with_verbose(true),
        );
        assert!(rendered.contains("│ ↪ "), "{rendered}");
        assert!(rendered.contains("\\necho done"), "{rendered}");
        for line in rendered.lines() {
            assert_eq!(unicode_width::UnicodeWidthStr::width(line), 30);
        }
        assert_eq!(input["command"], command);
    }

    #[test]
    fn tool_wrapping_preserves_scripts_and_long_unicode_arguments() {
        let command = format!(
            "python3 - <<'PY'\n\tprint(\"{}\")\n\nPY\n\n",
            "路径e\u{301}".repeat(30)
        );
        for width in [8, 20, 80] {
            let events = tool_events(
                "bash",
                &json!({"command": command}),
                Palette::plain().with_wrap(Some(width)).with_verbose(true),
            );
            let rendered = super::super::encode_plain(&events);
            for line in rendered.lines() {
                assert_eq!(
                    unicode_width::UnicodeWidthStr::width(line),
                    width,
                    "{line:?}"
                );
            }
            let mut restored = String::new();
            let mut rows = Vec::new();
            let mut row = None;
            for event in &events {
                match event {
                    TuiEvent::Text(text) if text == "│ " => row = Some(String::new()),
                    TuiEvent::Style(style) if *style == Style::WARNING => {
                        if let Some(row) = row.take() {
                            rows.push(row);
                        }
                    }
                    TuiEvent::Text(text) => {
                        if let Some(row) = &mut row {
                            row.push_str(text);
                        }
                    }
                    _ => {}
                }
            }
            for (index, line) in rows.iter().enumerate() {
                let continuation = line.starts_with("↪ ");
                let content = line.strip_prefix("↪ ").unwrap_or(line);
                if index > 0 && !continuation {
                    restored.push('\n');
                }
                restored.push_str(content);
            }
            assert_eq!(
                serde_json::from_str::<serde_json::Value>(&restored).unwrap(),
                json!({"command": command})
            );
        }
    }

    #[test]
    fn tool_input_controls_are_visible_without_terminal_escapes() {
        let rendered = render_tool_invocation(
            "bash",
            &json!({"command": "echo \u{1b}[2J", "stdin": "hello\rworld\u{7}"}),
            Palette::plain(),
        );
        assert_eq!(
            box_body(&rendered),
            "{\n  \"command\": \"echo \\u001b[2J\",\n  \"stdin\": \"hello\\rworld\\u0007\"\n}"
        );
    }

    #[test]
    fn tool_invocation_pretty_prints_objects() {
        let rendered = render_tool_invocation(
            "bash",
            &json!({"action": "start", "session_id": "s", "timeout_ms": 1000}),
            Palette::plain(),
        );
        assert_eq!(
            box_body(&rendered),
            "{\n  \"action\": \"start\",\n  \"session_id\": \"s\",\n  \"timeout_ms\": 1000\n}"
        );
        // Scalars stay compact.
        assert_eq!(
            box_body(&render_tool_invocation("x", &json!(42), Palette::plain())),
            "42"
        );
        assert_eq!(
            box_body(&render_tool_invocation("x", &json!("hi"), Palette::plain())),
            "\"hi\""
        );
    }

    #[test]
    fn verbose_tool_inputs_keep_long_json_strings() {
        let long = "a".repeat(180);
        let input = json!({"content": long, "path": "f.txt"});
        let rendered = render_tool_invocation(
            "write",
            &input,
            Palette::plain().with_wrap(Some(240)).with_verbose(true),
        );
        assert!(rendered.contains(&long));
        assert!(rendered.contains("f.txt"));
        assert!(!rendered.contains("… /verbose"));
        let compact = render_tool_invocation("write", &input, Palette::plain().with_wrap(Some(20)));
        assert!(compact.contains("… /verbose"));
        assert!(!compact.contains("f.txt"));
        assert_eq!(input["content"], long);
    }

    #[test]
    fn system_parts_never_render_or_create_human_turn_headers() {
        let hidden = Content::System {
            kind: "resume".into(),
            text: "runtime notice".into(),
            data: json!({"service":"private inventory"}),
        };
        let messages = [
            Message::UserMessage {
                content: vec![hidden.clone()],
            },
            Message::UserMessage {
                content: vec![
                    hidden.clone(),
                    Content::Text {
                        text: "human request".into(),
                    },
                ],
            },
            Message::UserMessage {
                content: vec![hidden],
            },
        ];
        let rendered = render_history(&messages, Palette::plain());
        assert!(rendered.contains("human request"));
        assert!(!rendered.contains("runtime notice"));
        assert!(!rendered.contains("private inventory"));
        assert_eq!(rendered.matches("USER · unknown\n").count(), 1);
        assert!(!rendered.contains("Accepted:"));
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
        assert!(rendered.contains("ASSISTANT · unknown\n"));
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
        assert!(rendered.contains("\x1b[0;1;36mUSER · unknown\x1b[0m\n"));
        assert!(rendered.contains(&format!("\x1b[0;1;36m{}\x1b[0m\n", user_rule(None))));
        assert!(rendered.contains("\x1b[0;1;32mASSISTANT · unknown\x1b[0m\n"));
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
    fn colored_tool_invocation_distinguishes_frame_keys_and_values() {
        let rendered = render_tool_invocation(
            "bash",
            &json!({"command": "echo hi"}),
            Palette::colored(true),
        );
        assert!(rendered.starts_with("\x1b[0;1;33m╭─ bash "));
        assert!(rendered.contains("\x1b[0;1;36m  \"command\":\x1b[0;2m \"echo hi\""));
    }

    #[test]
    fn wrapped_and_escaped_json_keys_keep_their_color_without_coloring_values() {
        let input = json!({"outer": {"te\"xt:": "not a \"key\": value"}});
        for width in [8, 20, 72] {
            let events = tool_events(
                "editor",
                &input,
                Palette::colored(true)
                    .with_wrap(Some(width))
                    .with_verbose(true),
            );
            let keys = events
                .windows(2)
                .filter_map(|pair| match pair {
                    [TuiEvent::Style(style), TuiEvent::Text(text)] if *style == Style::USER => {
                        Some(text.as_str())
                    }
                    _ => None,
                })
                .collect::<String>();
            assert_eq!(keys, "  \"outer\":    \"te\\\"xt:\":", "width {width}");
        }
    }

    #[test]
    fn colored_thinking_paragraph_is_dimmed() {
        let rendered = render_history(&[thinking_msg(&["pondering"])], Palette::colored(true));
        assert!(rendered.contains("\x1b[0;2mThinking: pondering\x1b[0m\n"));
    }
}
