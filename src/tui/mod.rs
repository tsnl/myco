//! Semantic presentation events for the terminal front-end — the **TUI
//! stream**, and its single producer, the interactive CLI's `Ui`.
//!
//! Everything the interactive CLI shows is a flat stream of [`TuiEvent`]s:
//! content bytes ([`TuiEvent::Text`], escape-free, wrap decisions already
//! applied), semantic style state ([`TuiEvent::Style`], no ANSI), and
//! hyperlink spans ([`TuiEvent::Link`]). Sinks are dumb encoders
//! ([`StdoutTuiSink`] → SGR/OSC 8, [`ConsoleTuiSink`] → `Text` bytes into the
//! `{id}.console` mirror), which makes the mirror's escape-free invariant
//! *structural*: `encode_plain(events)` equals `strip_sgr(encode_ansi(events))`
//! by construction — there is nothing to strip.
//!
//! [`TuiProducer`] owns both sinks and is the sole writer of user-visible
//! output: it implements [`EventSink`] for root-agent deltas (nested-agent
//! events are ignored) and exposes chrome methods to the REPL loop. Two
//! deliberate asymmetries: the submitted input line goes to the mirror only
//! (the line editor already echoed it), and history replay goes to the
//! terminal only (the mirror already holds that content). Cursor repaints are
//! redraws of content already in the stream — direct terminal writes, never
//! events — which is exactly why the mirror never sees them. Saved-history
//! replay ([`history_events`]) is built on this module's
//! section-state helpers, so live output and replay share one layout policy.

use std::sync::{Arc, Mutex};

pub mod markdown;
mod tool_box;
pub mod transcript;

pub use markdown::{MarkdownRenderer, render_block, render_block_with_base};
pub use transcript::{
    Palette, SECTION_RULE, attachment_note, banner_open_events, banner_rule,
    compacted_banner_events, format_tokens, history_events, section_rule, usage_line,
    user_header_line, user_rule, write_error_section, write_warning_section,
};

use crate::agent::{AgentEvent, EventSink, TraceContext};
use crate::generative_model::{Content, GenerateError, Message, TokenUsage, ToolResult, ToolUse};
use crate::session::ConsoleLog;

// ---------------------------------------------------------------------------
// Events
// ---------------------------------------------------------------------------

/// Terminal color roles used by the CLI (encoded as SGR 31/32/33/34/36).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Color {
    Red,
    Green,
    Yellow,
    Blue,
    Cyan,
}

/// Semantic display attributes for subsequent [`TuiEvent::Text`]. Full state,
/// not a delta — the SGR encoding re-emits the complete style (`\x1b[0;…m`)
/// on every change so an interrupted stream can't leak styling.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Style {
    pub dim: bool,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub color: Option<Color>,
}

impl Style {
    /// Everything off — encodes as the SGR reset.
    pub const RESET: Style = Style {
        dim: false,
        bold: false,
        italic: false,
        underline: false,
        color: None,
    };
    /// USER rule + header: bold cyan.
    pub const USER: Style = Style {
        bold: true,
        color: Some(Color::Cyan),
        ..Style::RESET
    };
    /// ASSISTANT rule + header: bold green.
    pub const ASSISTANT: Style = Style {
        bold: true,
        color: Some(Color::Green),
        ..Style::RESET
    };
    /// ERROR rule + header: bold red.
    pub const ERROR: Style = Style {
        bold: true,
        color: Some(Color::Red),
        ..Style::RESET
    };
    /// WARNING rule + header / tool names: bold yellow.
    pub const WARNING: Style = Style {
        bold: true,
        color: Some(Color::Yellow),
        ..Style::RESET
    };
    /// Banner family — startup/COMPACTED banners and the MYCO section: bold,
    /// uncolored (distinct from the section palette).
    pub const BANNER: Style = Style {
        bold: true,
        ..Style::RESET
    };
    /// Thinking paragraphs: dim.
    pub const THINKING: Style = Style {
        dim: true,
        ..Style::RESET
    };

    /// `self` decorated as hyperlink text: underlined blue, the browser-style
    /// link affordance. The other attributes pass through, so a link inside
    /// bold or dim text keeps its weight; only the color is overridden.
    pub fn linked(self) -> Style {
        Style {
            underline: true,
            color: Some(Color::Blue),
            ..self
        }
    }

    /// Encode as SGR: attribute order dim(2), bold(1), italic(3),
    /// underline(4), color — with the `0;` prefix that clears any style an
    /// interrupted stream left open. [`Style::RESET`] encodes as plain
    /// `\x1b[0m`.
    pub fn sgr(&self) -> String {
        let mut attrs: Vec<&str> = Vec::new();
        if self.dim {
            attrs.push("2");
        }
        if self.bold {
            attrs.push("1");
        }
        if self.italic {
            attrs.push("3");
        }
        if self.underline {
            attrs.push("4");
        }
        if let Some(color) = self.color {
            attrs.push(match color {
                Color::Red => "31",
                Color::Green => "32",
                Color::Yellow => "33",
                Color::Blue => "34",
                Color::Cyan => "36",
            });
        }
        if attrs.is_empty() {
            "\x1b[0m".to_string()
        } else {
            format!("\x1b[0;{}m", attrs.join(";"))
        }
    }
}

/// One presentation event. The invariants that make sinks trivial:
///
/// - `Text` holds exactly the bytes a plain terminal would show — wrap
///   decisions applied, **never** any escape byte;
/// - `Style` carries semantics, not bytes — each sink chooses its encoding
///   (SGR, nothing, …);
/// - `Link` opens (`Some(url)`) or closes (`None`) a hyperlink over the
///   following `Text`; like `Style` it is presentation, not content (a
///   terminal sink emits OSC 8, a plain sink emits nothing).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TuiEvent {
    Style(Style),
    Link(Option<String>),
    Text(String),
}

// ---------------------------------------------------------------------------
// Sinks + encoders
// ---------------------------------------------------------------------------

/// Subscriber to the TUI stream. Batches follow producer flush boundaries
/// (one chrome block, one streamed delta), so a sink can write each batch
/// contiguously.
pub trait TuiSink: Send + Sync {
    fn emit(&self, events: &[TuiEvent]);
}

/// Encode for a terminal: `Text` verbatim, `Style` as SGR and `Link` as OSC 8
/// when `styled` (the `--color` decision). When not styled a link degrades to
/// its plain visible text (the `Text` events pass through).
pub fn encode_ansi(events: &[TuiEvent], styled: bool) -> String {
    let mut out = String::new();
    for event in events {
        match event {
            TuiEvent::Text(text) => out.push_str(text),
            TuiEvent::Style(style) => {
                if styled {
                    out.push_str(&style.sgr());
                }
            }
            TuiEvent::Link(target) => {
                if styled {
                    // OSC 8 hyperlink: `ESC ] 8 ; ; <uri> ST`; close is the
                    // same with an empty uri. ST is `ESC \`.
                    out.push_str("\x1b]8;;");
                    if let Some(url) = target {
                        out.push_str(url);
                    }
                    out.push_str("\x1b\\");
                }
            }
        }
    }
    out
}

/// Encode as plain text: `Text` only. By construction this equals the ANSI
/// encoding with escapes stripped — no stripper needed.
pub fn encode_plain(events: &[TuiEvent]) -> String {
    let mut out = String::new();
    for event in events {
        if let TuiEvent::Text(text) = event {
            out.push_str(text);
        }
    }
    out
}

/// True when `encode_ansi(events, styled)` would end with a newline — the
/// "does this block still need a line close?" decision, made without
/// building the string. A trailing `Style`/`Link` encodes as escape bytes
/// when styled, so it breaks the newline just like it does on the terminal.
pub(crate) fn encoded_ends_with_newline(events: &[TuiEvent], styled: bool) -> bool {
    for event in events.iter().rev() {
        match event {
            TuiEvent::Text(text) if !text.is_empty() => return text.ends_with('\n'),
            TuiEvent::Text(_) => {}
            TuiEvent::Style(_) | TuiEvent::Link(_) => {
                if styled {
                    return false;
                }
            }
        }
    }
    false
}

/// Terminal subscriber: SGR-encodes to stdout.
pub struct StdoutTuiSink {
    /// The resolved `--color` decision ([`crate::config::Config::colors_enabled`]).
    pub colors: bool,
}

impl TuiSink for StdoutTuiSink {
    fn emit(&self, events: &[TuiEvent]) {
        use std::io::Write;
        let mut stdout = std::io::stdout().lock();
        let _ = stdout.write_all(encode_ansi(events, self.colors).as_bytes());
        let _ = stdout.flush();
    }
}

/// Console-mirror subscriber: plain-encodes into the per-session
/// `{id}.console` file via [`ConsoleLog`]. Since [`encode_plain`] never
/// produces escapes, the mirror file is escape-free by construction.
pub struct ConsoleTuiSink {
    log: ConsoleLog,
}

impl ConsoleTuiSink {
    pub fn new(log: ConsoleLog) -> Self {
        Self { log }
    }
}

impl TuiSink for ConsoleTuiSink {
    fn emit(&self, events: &[TuiEvent]) {
        self.log.append(&encode_plain(events));
    }
}

// ---------------------------------------------------------------------------
// Shared section/paragraph layout
// ---------------------------------------------------------------------------

/// Rule/header line in a chrome color: style on, text, reset, newline.
pub(crate) fn styled_line(events: &mut Vec<TuiEvent>, style: Style, text: &str) {
    events.push(TuiEvent::Style(style));
    events.push(TuiEvent::Text(text.to_string()));
    events.push(TuiEvent::Style(Style::RESET));
    events.push(TuiEvent::Text("\n".into()));
}

fn tool_outcome_line(tool: &ToolUse, result: &ToolResult) -> Option<String> {
    let status = result.status.as_deref().or_else(|| {
        result.is_error.then(|| {
            result
                .content
                .iter()
                .find_map(|part| match part {
                    Content::Text { text } if !text.trim().is_empty() => Some(text.as_str()),
                    _ => None,
                })
                .unwrap_or("tool failed")
        })
    })?;
    let mut identity = tool.name.clone();
    for field in ["host", "action", "session_id", "command", "path"] {
        if let Some(value) = tool.input.get(field).and_then(|v| v.as_str()) {
            identity.push(' ');
            identity.extend(value.escape_debug().take(60));
        }
    }
    let status: String = status
        .lines()
        .next()
        .unwrap_or(status)
        .escape_debug()
        .take(240)
        .collect();
    let failure = if result.is_error { "failed: " } else { "" };
    Some(format!("↳ {identity}: {failure}{status}"))
}

/// Headed section open: blank line, thin rule, header, blank line. The one
/// layout shared by ASSISTANT (live + replay), ERROR, and WARNING sections.
pub(crate) fn section_open_events(
    events: &mut Vec<TuiEvent>,
    style: Style,
    header: &str,
    wrap: Option<usize>,
) {
    events.push(TuiEvent::Text("\n".into()));
    styled_line(events, style, &section_rule(wrap));
    styled_line(events, style, header);
    events.push(TuiEvent::Text("\n".into()));
}

/// Section/paragraph layout state shared by the live producer and history
/// replay: ASSISTANT opens once per agent turn, paragraphs (text, thinking,
/// tools) are blank-line separated inside it.
#[derive(Debug, Clone, Copy)]
pub(crate) struct SectionState {
    /// The stream sits at a visual line start.
    pub at_line_start: bool,
    /// The ASSISTANT header is already open for this agent turn.
    pub assistant_open: bool,
    /// A finished paragraph wants a blank line before the next one.
    pub need_blank: bool,
    tool_box: Option<tool_box::ToolBox>,
    pub turn_time: Option<chrono::DateTime<chrono::Utc>>,
}

impl SectionState {
    pub fn new() -> Self {
        Self {
            at_line_start: true,
            assistant_open: false,
            need_blank: false,
            tool_box: None,
            turn_time: None,
        }
    }

    /// Close a partial line if one is open.
    pub fn ensure_line_start(&mut self, events: &mut Vec<TuiEvent>) {
        if !self.at_line_start {
            events.push(TuiEvent::Text("\n".into()));
            self.at_line_start = true;
        }
    }

    /// Open the ASSISTANT section once per agent turn (multi-step tool loops
    /// stay in one section).
    pub fn ensure_assistant(&mut self, events: &mut Vec<TuiEvent>, wrap: Option<usize>) {
        if self.assistant_open {
            return;
        }
        self.ensure_line_start(events);
        let header = transcript::turn_header("ASSISTANT", self.turn_time);
        section_open_events(events, Style::ASSISTANT, &header, wrap);
        self.at_line_start = true;
        self.assistant_open = true;
        self.need_blank = false;
    }

    /// Blank line before a subsequent paragraph inside ASSISTANT.
    pub fn separate_paragraph_if_needed(&mut self, events: &mut Vec<TuiEvent>) {
        self.finish_tools(events);
        if self.need_blank {
            self.ensure_line_start(events);
            events.push(TuiEvent::Text("\n".into()));
            self.at_line_start = true;
        }
    }

    pub fn start_tool(
        &mut self,
        events: &mut Vec<TuiEvent>,
        name: &str,
        input: &serde_json::Value,
        palette: Palette,
    ) {
        self.ensure_assistant(events, palette.wrap);
        let mut frame = match self.tool_box {
            Some(frame) => {
                frame.next(events, name);
                frame
            }
            None => {
                self.separate_paragraph_if_needed(events);
                tool_box::ToolBox::open(events, name, palette)
            }
        };
        frame.input(events, name, input);
        self.tool_box = Some(frame);
        self.at_line_start = true;
        self.need_blank = true;
    }

    pub fn tool_result(
        &mut self,
        events: &mut Vec<TuiEvent>,
        tool: &ToolUse,
        result: &ToolResult,
        palette: Palette,
    ) {
        let line = tool_outcome_line(tool, result)
            .unwrap_or_else(|| format!("↳ {}: output", tool.name.escape_debug()));
        if self.tool_box.is_none() {
            self.start_tool(events, &tool.name, &tool.input, palette);
        }
        let status = result.status.as_deref().unwrap_or("");
        let process_failed = tool.name == "bash"
            && (status.starts_with("signal ")
                || status
                    .strip_prefix("exit ")
                    .and_then(|code| code.parse::<i32>().ok())
                    .is_some_and(|code| code != 0));
        let style = if result.is_error || process_failed {
            Style::ERROR
        } else if status.contains("cancel") {
            Style::WARNING
        } else {
            Style::ASSISTANT
        };
        let frame = self.tool_box.as_mut().unwrap();
        frame.status(events, &line, style);
        for content in &result.content {
            match content {
                Content::Text { text } if !text.is_empty() => {
                    frame.text(events, text, "", Style::RESET)
                }
                Content::Image { .. } => frame.text(events, "[image output]", "", Style::THINKING),
                _ => {}
            }
        }
    }

    pub fn finish_tools(&mut self, events: &mut Vec<TuiEvent>) {
        if let Some(frame) = self.tool_box.take() {
            frame.close(events);
        }
    }
}

// ---------------------------------------------------------------------------
// Producer
// ---------------------------------------------------------------------------

/// State guarded by one lock; events are built under the lock and emitted to
/// the sinks after it is released (no sink IO while holding producer state).
struct ProducerState {
    wrap: Option<usize>,
    verbose: bool,
    section: SectionState,
    /// True while streaming answer text (no blank lines between text deltas).
    in_text_stream: bool,
    /// Streaming markdown/wrap renderer for the current answer-text paragraph.
    text_md: Option<MarkdownRenderer>,
    /// Live `Thinking: …` summary-line builder (UI only; history replay
    /// renders thinking from the stored message instead).
    thinking_line_open: bool,
    thinking_md: Option<MarkdownRenderer>,
}

/// The single producer of the TUI stream — the interactive CLI's `Ui`.
///
/// Owns the terminal sink and the console-mirror sink; translates domain
/// [`AgentEvent`]s and REPL chrome calls into [`TuiEvent`]s. Headed sections:
/// USER ([`Self::user_header`]), ASSISTANT (streamed via [`EventSink`]),
/// MYCO ([`Self::myco_section`]), ERROR ([`Self::error_section`]), WARNING
/// ([`Self::warning_section`]). Thinking summaries, tool invocations, and
/// answer text are paragraphs inside a single ASSISTANT section for the whole
/// agent turn (including multi-step tool loops). MYCO/ERROR/WARNING sections
/// are live-only and not replayed.
///
/// **Invariant: every content line sits under a banner or a headed section.**
/// The API is what enforces it — there is no free-line emitter. The two
/// non-section methods are scoped: [`Self::note`] appends a line *inside* the
/// currently open section, and [`Self::blank_line`] is layout (the gap before
/// the next USER rule), not content.
pub struct TuiProducer {
    terminal: Arc<dyn TuiSink>,
    mirror: Arc<dyn TuiSink>,
    /// The resolved `--color` decision: picks the markdown renderer mode
    /// (styled consumes delimiters; plain is byte-identity) — it must match
    /// the terminal sink's encoding.
    colors: bool,
    state: Mutex<ProducerState>,
}

impl TuiProducer {
    pub fn new(
        terminal: Arc<dyn TuiSink>,
        mirror: Arc<dyn TuiSink>,
        colors: bool,
        wrap: Option<usize>,
    ) -> Self {
        Self {
            terminal,
            mirror,
            colors,
            state: Mutex::new(ProducerState {
                wrap,
                verbose: false,
                section: SectionState::new(),
                in_text_stream: false,
                text_md: None,
                thinking_line_open: false,
                thinking_md: None,
            }),
        }
    }

    /// Update the wrap width after a terminal resize (an in-flight paragraph
    /// keeps the width its renderer was created with).
    pub fn set_wrap(&self, wrap: Option<usize>) {
        self.with_state(|st| st.wrap = wrap);
    }

    /// Toggle tool detail at the idle prompt; the caller redraws saved history.
    pub fn toggle_verbose(&self) -> bool {
        self.with_state(|st| {
            st.verbose = !st.verbose;
            st.verbose
        })
    }

    fn with_state<R>(&self, f: impl FnOnce(&mut ProducerState) -> R) -> R {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        f(&mut state)
    }

    fn palette(&self, st: &ProducerState) -> Palette {
        Palette::colored(self.colors)
            .with_wrap(st.wrap)
            .with_verbose(st.verbose)
    }

    fn broadcast(&self, events: Vec<TuiEvent>) {
        if events.is_empty() {
            return;
        }
        self.terminal.emit(&events);
        self.mirror.emit(&events);
    }

    // -- chrome (called by the REPL loop, not derived from AgentEvents) -----

    /// USER rule + `USER <used>/<max> (<pct>%)` header + optional usage line +
    /// one `●` line per still-running tool + blank line. Resets per-turn
    /// stream state.
    pub fn user_header(
        &self,
        used: Option<u64>,
        max: u64,
        usage: Option<TokenUsage>,
        running: &[String],
    ) {
        let events = self.with_state(|st| {
            let mut events = Vec::new();
            styled_line(&mut events, Style::USER, &user_rule(st.wrap));
            styled_line(&mut events, Style::USER, &user_header_line(used, max));
            if let Some(u) = usage {
                styled_line(&mut events, Style::USER, &usage_line(u));
            }
            for line in running {
                styled_line(&mut events, Style::USER, &format!("● {line}"));
            }
            events.push(TuiEvent::Text("\n".into()));
            st.section = SectionState::new();
            st.in_text_stream = false;
            st.text_md = None;
            st.thinking_line_open = false;
            st.thinking_md = None;
            events
        });
        self.broadcast(events);
    }

    /// The submitted input line, wrap-only (no markdown styling) — **mirror
    /// only**: the line editor already echoed it to the terminal, so the
    /// console needs the logical text but the terminal must not repeat it.
    pub fn submitted_input(&self, line: &str) {
        let rendered =
            self.with_state(|st| render_block(line, Palette::plain().with_wrap(st.wrap)));
        self.mirror
            .emit(&[TuiEvent::Text(rendered), TuiEvent::Text("\n".into())]);
    }

    /// Label the assistant turn with the same acceptance time saved in history.
    pub fn accepted_turn(&self, time: chrono::DateTime<chrono::Utc>) {
        let events = self.with_state(|st| {
            st.section.turn_time = Some(time);
            let mut events = Vec::new();
            st.section.ensure_assistant(&mut events, st.wrap);
            events
        });
        self.broadcast(events);
    }

    /// Replay saved history — **terminal only**: the mirror already holds this
    /// content from the run(s) that streamed it (`{id}.console` is opened for
    /// append). Used for `--resume`/`/resume` replay and the Ctrl-L / resize
    /// reprint.
    pub fn replay_history(&self, messages: &[Message]) {
        let palette = self.with_state(|st| self.palette(st));
        let events = history_events(messages, palette);
        if !events.is_empty() {
            self.terminal.emit(&events);
        }
    }

    pub fn replay_thread(&self, thread: &crate::session::Thread) {
        let palette = self.with_state(|st| self.palette(st));
        let events =
            transcript::history_events_at(&thread.messages, palette, &thread.user_turn_timestamps);
        self.terminal.emit(&events);
    }

    /// Startup banner: full-block rule, MYCO title, model/session lines, and
    /// the two hints worth surfacing before the first prompt.
    pub fn startup_banner(&self, model_key: &str, session_label: &str) {
        let events = self.with_state(|st| {
            let mut events = Vec::new();
            styled_line(&mut events, Style::BANNER, &banner_rule(st.wrap));
            styled_line(&mut events, Style::BANNER, "MYCO");
            events.push(TuiEvent::Text(format!(
                "\nModel: {model_key}\nSession: {session_label}\n\n\
                 /help for commands\n\nAlt-Enter or Ctrl-J for newline\n"
            )));
            st.section.at_line_start = true;
            events
        });
        self.broadcast(events);
    }

    /// COMPACTED banner: printed onto the screen `/compact` just cleared, in
    /// place of a replayed transcript — a compaction is a fresh start, and the
    /// conversation is still on disk in both sessions and the mirror.
    pub fn compacted_banner(&self, outcome: &crate::session::CompactOutcome) {
        let events = self.with_state(|st| {
            let events = compacted_banner_events(outcome, st.wrap);
            let turn_time = st.section.turn_time;
            st.section = SectionState::new();
            st.section.turn_time = turn_time;
            st.section.at_line_start = true;
            events
        });
        self.broadcast(events);
    }

    pub fn compacting_banner(&self, session: &str, thread: &str, automatic: bool) {
        self.headed_section(
            Style::BANNER,
            "COMPACTING",
            &format!(
                "Mode: {}\nSession: {session}\nThread: {thread}\nCtrl-C to cancel",
                if automatic { "automatic" } else { "manual" }
            ),
        );
    }

    /// Headed MYCO section (live-only): myco's own response to a meta-command
    /// (`/help`, `/hosts`, `/session`, …) — the banner family's voice as a
    /// mid-screen section, so command output is headed like every other block.
    pub fn myco_section(&self, body: &str) {
        self.headed_section(Style::BANNER, "MYCO", body);
    }

    /// Headed ERROR section (live-only): generate failures, not stored in
    /// history, so resume/Ctrl-L will not replay them.
    pub fn error_section(&self, message: &str) {
        self.headed_section(Style::ERROR, "ERROR", message);
    }

    /// Headed WARNING section (live-only): startup preflight problems.
    pub fn warning_section(&self, body: &str) {
        self.headed_section(Style::WARNING, "WARNING", body);
    }

    fn headed_section(&self, style: Style, header: &str, body: &str) {
        let events = self.with_state(|st| {
            let mut events = Vec::new();
            finish_thinking_line(st, &mut events);
            end_text_stream(st, &mut events, self.colors);
            st.section.finish_tools(&mut events);
            st.section.assistant_open = false;
            section_open_events(&mut events, style, header, st.wrap);
            let body = if body.ends_with('\n') {
                body.to_string()
            } else {
                format!("{body}\n")
            };
            events.push(TuiEvent::Text(body));
            st.section.at_line_start = true;
            events
        });
        self.broadcast(events);
    }

    /// Turn-cancelled notice (live-only).
    pub fn cancelled(&self) {
        self.with_state(|st| st.section.at_line_start = true);
        self.broadcast(vec![TuiEvent::Text("\n(cancelled)\n".into())]);
    }

    /// One-line notice *inside the currently open section* (newline
    /// appended): attachment notes and transient progress under a USER
    /// header. Everything free-standing goes through a banner or a headed
    /// section instead — that is what keeps the transcript fully headed.
    /// `text` is content, not markup: it must not contain escape bytes.
    pub fn note(&self, text: &str) {
        self.emit_text(&format!("{text}\n"));
    }

    /// Blank line to terminal + mirror — layout, not content: the gap that
    /// closes a finished block before the next USER rule.
    pub fn blank_line(&self) {
        self.emit_text("\n");
    }

    /// Plain text verbatim to terminal + mirror (escape-free content only).
    fn emit_text(&self, text: &str) {
        if text.is_empty() {
            return;
        }
        self.with_state(|st| st.section.at_line_start = text.ends_with('\n'));
        self.broadcast(vec![TuiEvent::Text(text.to_string())]);
    }

    // -- AgentEvent translation (root agent; nested workers are filtered) ---

    fn thinking_delta(&self, text: &str) {
        if text.is_empty() {
            return;
        }
        let events = self.with_state(|st| {
            let mut events = Vec::new();
            // Always show thinking summaries inside ASSISTANT as `Thinking: …`.
            st.section.ensure_assistant(&mut events, st.wrap);
            if !st.thinking_line_open {
                // End answer-text stream so thinking is its own paragraph.
                end_text_stream(st, &mut events, self.colors);
                st.section.separate_paragraph_if_needed(&mut events);
                st.section.ensure_line_start(&mut events);
                st.thinking_line_open = true;
                // Dim base stays open across deltas; finish_thinking_line resets.
                let palette = self.palette(st);
                let md = st
                    .thinking_md
                    .insert(MarkdownRenderer::with_base(palette, "2"));
                events.extend(md.feed_events("Thinking: "));
                events.extend(md.feed_events(text));
            } else if let Some(md) = st.thinking_md.as_mut() {
                events.extend(md.feed_events(text));
            }
            st.section.at_line_start = st
                .thinking_md
                .as_ref()
                .is_some_and(|r| r.ends_at_line_start());
            events
        });
        self.broadcast(events);
    }

    fn text_delta(&self, text: &str) {
        if text.is_empty() {
            return;
        }
        let events = self.with_state(|st| {
            let mut events = Vec::new();
            finish_thinking_line(st, &mut events);
            st.section.ensure_assistant(&mut events, st.wrap);
            // Blank-separate only when starting a new text paragraph after
            // thinking/tools — never between chunks of the same stream.
            if !st.in_text_stream {
                st.section.separate_paragraph_if_needed(&mut events);
            }
            st.in_text_stream = true;
            st.section.need_blank = false;
            let palette = self.palette(st);
            let md = st
                .text_md
                .get_or_insert_with(|| MarkdownRenderer::new(palette));
            events.extend(md.feed_events(text));
            st.section.at_line_start = md.ends_at_line_start();
            events
        });
        self.broadcast(events);
    }

    fn tool_started(&self, name: &str, input: &serde_json::Value) {
        let events = self.with_state(|st| {
            let mut events = Vec::new();
            // End any open text/thinking stream so the tool is its own paragraph.
            finish_thinking_line(st, &mut events);
            end_text_stream(st, &mut events, self.colors);
            let palette = self.palette(st);
            st.section.start_tool(&mut events, name, input, palette);
            st.section.at_line_start = true;
            st.in_text_stream = false;
            st.section.need_blank = true;
            events
        });
        self.broadcast(events);
    }

    fn tool_finished(&self, tool: &ToolUse, result: &ToolResult) {
        let events = self.with_state(|st| {
            let mut events = Vec::new();
            finish_thinking_line(st, &mut events);
            end_text_stream(st, &mut events, self.colors);
            let palette = self.palette(st);
            st.section.tool_result(&mut events, tool, result, palette);
            st.section.at_line_start = true;
            st.section.need_blank = true;
            events
        });
        self.broadcast(events);
    }

    fn retry(
        &self,
        cause: &GenerateError,
        attempt: u32,
        max_attempts: u32,
        delay: std::time::Duration,
    ) {
        self.flush_output();
        let body = format!(
            "Attempt {}/{} in {:.1}s\n{}",
            attempt + 1,
            max_attempts,
            delay.as_secs_f64(),
            cause.to_string().escape_debug(),
        );
        self.headed_section(Style::WARNING, "RETRY", &body);
    }

    pub fn flush_output(&self) {
        let events = self.with_state(|st| {
            let mut events = Vec::new();
            finish_thinking_line(st, &mut events);
            end_text_stream(st, &mut events, self.colors);
            st.section.finish_tools(&mut events);
            st.section.ensure_line_start(&mut events);
            // Close ASSISTANT for the next user turn (REPL prints USER next).
            st.section.assistant_open = false;
            st.section.need_blank = false;
            st.in_text_stream = false;
            events
        });
        self.broadcast(events);
    }
}

impl EventSink for TuiProducer {
    fn emit(&self, event: AgentEvent) {
        // Root agent only — hide nested worker noise (depth > 0, e.g. compact).
        match event {
            AgentEvent::Failure {
                failure,
                attempt,
                max_attempts,
                retry_in: Some(delay),
                context: TraceContext { depth: 0, .. },
            } => self.retry(&failure.cause, attempt, max_attempts, delay),
            AgentEvent::ThinkingDelta {
                text,
                context: TraceContext { depth: 0, .. },
            } => self.thinking_delta(&text),
            AgentEvent::TextDelta {
                text,
                context: TraceContext { depth: 0, .. },
            } => self.text_delta(&text),
            AgentEvent::ToolStarted {
                tool_use,
                context: TraceContext { depth: 0, .. },
            } => self.tool_started(&tool_use.name, &tool_use.input),
            AgentEvent::ToolFinished {
                tool_use,
                result,
                context: TraceContext { depth: 0, .. },
            } => self.tool_finished(&tool_use, &result),
            AgentEvent::TurnFinished {
                context: TraceContext { depth: 0, .. },
            } => self.flush_output(),
            _ => {}
        }
    }
}

/// Finish a live `Thinking: …` line: flush its renderer, close the dim style
/// it opened, and end the line. Thinking is a finished paragraph for spacing.
fn finish_thinking_line(st: &mut ProducerState, events: &mut Vec<TuiEvent>) {
    if !st.thinking_line_open {
        return;
    }
    st.thinking_line_open = false;
    st.section.at_line_start = true;
    st.in_text_stream = false;
    st.section.need_blank = true;
    if let Some(mut md) = st.thinking_md.take() {
        events.extend(md.finish_events());
    }
    events.push(TuiEvent::Text("\n".into()));
}

/// Close the current answer-text stream: flush its renderer, mark the
/// paragraph finished. The line-start decision is made on the *encoded* tail
/// (a trailing style reset keeps the line open), matching the terminal.
fn end_text_stream(st: &mut ProducerState, events: &mut Vec<TuiEvent>, colors: bool) {
    if !st.in_text_stream {
        return;
    }
    st.in_text_stream = false;
    st.section.need_blank = true;
    let tail = st
        .text_md
        .take()
        .map(|mut r| r.finish_events())
        .unwrap_or_default();
    // Runs once per paragraph close, not per delta, so encoding the handful
    // of tail events is cheap.
    let encoded = encode_ansi(&tail, colors);
    if !encoded.is_empty() {
        st.section.at_line_start = encoded.ends_with('\n');
        events.extend(tail);
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::generative_model::ToolUse;

    /// Capturing sink for assertions on the raw stream.
    #[derive(Default)]
    struct Capture(Mutex<Vec<TuiEvent>>);

    impl TuiSink for Capture {
        fn emit(&self, events: &[TuiEvent]) {
            self.0
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .extend(events.iter().cloned());
        }
    }

    impl Capture {
        fn events(&self) -> Vec<TuiEvent> {
            self.0.lock().unwrap_or_else(|e| e.into_inner()).clone()
        }
    }

    fn producer(wrap: Option<usize>) -> (TuiProducer, Arc<Capture>, Arc<Capture>) {
        let terminal = Arc::new(Capture::default());
        let mirror = Arc::new(Capture::default());
        let producer = TuiProducer::new(terminal.clone(), mirror.clone(), true, wrap);
        (producer, terminal, mirror)
    }

    #[test]
    fn compact_tool_boxes_show_the_first_five_lines_and_keep_the_outcome() {
        let (p, terminal, _) = producer(Some(48));
        let tool = ToolUse {
            name: "bash".into(),
            input: serde_json::json!({"host":"local", "action":"exec", "command": (1..=10).map(|i| format!("echo input-{i}")).collect::<Vec<_>>().join("\n")}),
        };
        p.emit(AgentEvent::ToolStarted {
            tool_use: tool.clone(),
            context: ctx(0),
        });
        p.emit(AgentEvent::ToolFinished {
            tool_use: tool,
            result: ToolResult::text("output-1\noutput-2\noutput-3\noutput-4\noutput-5\noutput-6")
                .with_status("exit 7"),
            context: ctx(0),
        });
        finish(&p);
        let output = encode_plain(&terminal.events());
        assert!(output.contains("│   echo input-5"), "{output}");
        assert!(!output.contains("│   echo input-6"), "{output}");
        assert!(!output.contains("output-6"), "{output}");
        assert!(output.contains("… /verbose"), "{output}");
        assert!(output.contains("exit 7"), "{output}");
    }

    #[test]
    fn tool_input_and_outcome_share_a_rounded_box() {
        let (p, terminal, mirror) = producer(Some(48));
        let tool = ToolUse {
            name: "bash".into(),
            input: serde_json::json!({"command":"exit 7"}),
        };
        p.emit(AgentEvent::ToolStarted {
            tool_use: tool.clone(),
            context: ctx(0),
        });
        assert!(encode_plain(&terminal.events()).contains("$ exit 7"));
        p.emit(AgentEvent::ToolFinished {
            tool_use: tool,
            result: ToolResult::text("hidden stdout").with_status("exit 7"),
            context: ctx(0),
        });
        finish(&p);
        let output = encode_plain(&terminal.events());
        assert_eq!(output, encode_plain(&mirror.events()));
        assert_eq!(output.matches('╭').count(), 1, "{output}");
        assert_eq!(output.matches('╰').count(), 1, "{output}");
        assert!(!output.contains("bash("), "{output}");
        let start = output.find('╭').unwrap();
        let end = output.find('╰').unwrap();
        assert!(output[start..end].contains("$ exit 7"));
        assert!(output[start..end].contains("exit 7: exit 7"));
        for line in output[start..].lines() {
            assert_eq!(unicode_width::UnicodeWidthStr::width(line), 48, "{line}");
        }
    }

    #[test]
    fn concurrent_tool_outcomes_and_cancellation_stay_inside_the_frame() {
        let (p, terminal, mirror) = producer(Some(40));
        let tools = ["sleep 30", "exit 7"].map(|command| ToolUse {
            name: "bash".into(),
            input: serde_json::json!({"command":command}),
        });
        for tool in &tools {
            p.emit(AgentEvent::ToolStarted {
                tool_use: tool.clone(),
                context: ctx(0),
            });
        }
        for (tool, status) in [
            (&tools[1], "exit 7"),
            (&tools[0], "cancel requested; effects unknown"),
        ] {
            p.emit(AgentEvent::ToolFinished {
                tool_use: tool.clone(),
                result: ToolResult::text("").with_status(status),
                context: ctx(0),
            });
        }
        finish(&p);
        let events = terminal.events();
        let output = encode_plain(&events);
        assert_eq!(events, mirror.events());
        assert_eq!(output.matches('╭').count(), 1);
        assert_eq!(output.matches('├').count(), 1);
        assert_eq!(output.matches('╰').count(), 1);
        assert!(output.find("exit 7: exit 7").unwrap() < output.find('╰').unwrap());
        assert!(output.find("effects unknown").unwrap() < output.find('╰').unwrap());
        assert!(encode_ansi(&events, true).contains("\x1b[0;1;31m↳ bash exit 7: exit 7"));
        assert!(!output.contains('\x1b'));
    }

    #[test]
    fn turn_banners_use_the_saved_acceptance_time_in_live_output_and_replay() {
        let (p, terminal, mirror) = producer(None);
        let time = "2026-09-21T22:00:00Z".parse().unwrap();
        p.user_header(Some(0), 100, None, &[]);
        p.accepted_turn(time);
        text(&p, "answer");
        finish(&p);
        let live = encode_plain(&terminal.events());
        assert_eq!(live, encode_plain(&mirror.events()));
        let replay = encode_plain(&transcript::history_events_at(
            &[
                crate::test_support::user("question"),
                crate::test_support::assistant("answer"),
            ],
            Palette::plain(),
            &[(0, time)].into(),
        ));
        for output in [live, replay] {
            assert!(
                output.contains("ASSISTANT · 2026-09-21T22:00:00Z\n"),
                "{output}"
            );
            assert!(!output.contains("Accepted:"));
        }
    }

    #[test]
    fn compaction_progress_has_its_own_system_section() {
        let (p, terminal, mirror) = producer(None);
        p.user_header(Some(90), 100, None, &[]);
        p.compacting_banner("session-id", "thread-id", true);
        p.note("compacting: 10s elapsed (Ctrl-C to cancel)");
        let output = encode_plain(&terminal.events());
        assert!(output.contains(&format!("{SECTION_RULE}\nCOMPACTING\n\nMode: automatic\nSession: session-id\nThread: thread-id\nCtrl-C to cancel\ncompacting: 10s")), "{output}");
        assert_eq!(terminal.events(), mirror.events());
    }

    #[test]
    fn factual_outcomes_remain_visible_with_truncated_content_and_replay() {
        let (p, terminal, mirror) = producer(None);
        let tool = ToolUse {
            name: "bash".into(),
            input: serde_json::json!({"host":"local", "action":"exec", "command":"exit 7"}),
        };
        let result = ToolResult::text("large tool stdout should stay hidden").with_status("exit 7");
        p.emit(AgentEvent::ToolStarted {
            tool_use: tool.clone(),
            context: ctx(0),
        });
        p.emit(AgentEvent::ToolFinished {
            tool_use: tool.clone(),
            result: result.clone(),
            context: ctx(0),
        });
        let live = encode_plain(&terminal.events());
        assert_eq!(live, encode_plain(&mirror.events()));
        let replay = encode_plain(&history_events(
            &[
                Message::AssistantMessage {
                    content: vec![],
                    tool_uses: vec![tool.clone()],
                    turn_end_reason: None,
                },
                Message::ToolResults {
                    tool_use_results: vec![result],
                },
            ],
            Palette::plain(),
        ));
        let line = "↳ bash local exec exit 7: exit 7";
        for output in [live, replay] {
            assert!(output.contains(line), "{output}");
            assert!(!output.contains("large tool stdout"));
        }
        let failure = ToolResult::err(format!("bad\x1b[31m{}\nsecond line", "x".repeat(1000)));
        let line = tool_outcome_line(&tool, &failure).unwrap();
        assert!(!line.contains('\x1b'));
        assert!(!line.contains("second line"));
        assert!(line.len() < 400);
        assert!(line.contains("failed:"));
        assert!(tool_outcome_line(&tool, &ToolResult::text("success")).is_none());
    }

    #[test]
    fn retry_notice_flushes_text_and_is_mirrored_before_the_next_answer() {
        let (p, terminal, mirror) = producer(None);
        text(&p, "before");
        for depth in [1, 0] {
            p.emit(AgentEvent::Failure {
                failure: crate::generative_model::GenerationFailure::transient(
                    GenerateError::ExecutionError("busy\x1b[31m".into()),
                    None,
                ),
                attempt: 1,
                max_attempts: 3,
                retry_in: Some(std::time::Duration::from_secs(1)),
                context: ctx(depth),
            });
        }
        text(&p, "after");
        finish(&p);
        let output = encode_plain(&terminal.events());
        assert_eq!(output, encode_plain(&mirror.events()));
        assert!(!output.contains('\x1b'));
        assert_eq!(output.matches("RETRY").count(), 1);
        assert_eq!(output.matches("ASSISTANT").count(), 2);
        let before = output.find("before").unwrap();
        let retry = output.find("Attempt 2/3 in 1.0s").unwrap();
        let after = output.find("after").unwrap();
        assert!(before < retry && retry < after, "{output}");
        assert!(output.contains("busy"));
    }

    fn ctx(depth: usize) -> TraceContext {
        TraceContext {
            depth,
            ..TraceContext::root()
        }
    }

    // Emit shorthands: one agent event at `depth` (`_at`) or at the root.

    fn text_at(p: &TuiProducer, depth: usize, text: &str) {
        p.emit(AgentEvent::TextDelta {
            text: text.into(),
            context: ctx(depth),
        });
    }

    fn text(p: &TuiProducer, text: &str) {
        text_at(p, 0, text);
    }

    fn think_at(p: &TuiProducer, depth: usize, text: &str) {
        p.emit(AgentEvent::ThinkingDelta {
            text: text.into(),
            context: ctx(depth),
        });
    }

    fn think(p: &TuiProducer, text: &str) {
        think_at(p, 0, text);
    }

    fn tool_at(p: &TuiProducer, depth: usize, name: &str, input: serde_json::Value) {
        p.emit(AgentEvent::ToolStarted {
            tool_use: ToolUse {
                name: name.into(),
                input,
            },
            context: ctx(depth),
        });
    }

    fn tool(p: &TuiProducer, name: &str, input: serde_json::Value) {
        tool_at(p, 0, name, input);
    }

    fn finish_at(p: &TuiProducer, depth: usize) {
        p.emit(AgentEvent::TurnFinished {
            context: ctx(depth),
        });
    }

    fn finish(p: &TuiProducer) {
        finish_at(p, 0);
    }

    fn strip_sgr(s: &str) -> String {
        let mut out = String::new();
        let mut chars = s.chars();
        while let Some(c) = chars.next() {
            if c == '\x1b' {
                for e in chars.by_ref() {
                    if e == 'm' {
                        break;
                    }
                }
            } else {
                out.push(c);
            }
        }
        out
    }

    #[test]
    fn style_sgr_bytes_are_pinned() {
        // The exact escapes the CLI has always emitted for its chrome roles.
        assert_eq!(Style::USER.sgr(), "\x1b[0;1;36m");
        assert_eq!(Style::ASSISTANT.sgr(), "\x1b[0;1;32m");
        assert_eq!(Style::ERROR.sgr(), "\x1b[0;1;31m");
        assert_eq!(Style::WARNING.sgr(), "\x1b[0;1;33m");
        assert_eq!(Style::BANNER.sgr(), "\x1b[0;1m");
        // Markdown styles match the renderer's attribute order.
        let bold_code = Style {
            bold: true,
            color: Some(Color::Cyan),
            ..Style::RESET
        };
        assert_eq!(bold_code.sgr(), "\x1b[0;1;36m");
        assert_eq!(Style::RESET.sgr(), "\x1b[0m");
        // Hyperlink decoration: underline + blue over the base attributes.
        assert_eq!(Style::RESET.linked().sgr(), "\x1b[0;4;34m");
        assert_eq!(Style::THINKING.linked().sgr(), "\x1b[0;2;4;34m");
    }

    #[test]
    fn link_event_encodes_osc8_only_when_styled() {
        let events = vec![
            TuiEvent::Link(Some("https://ex.test/p".into())),
            TuiEvent::Text("docs".into()),
            TuiEvent::Link(None),
        ];
        // Styled: OSC 8 open (`ESC ] 8 ; ; uri ST`) around the text, then close.
        assert_eq!(
            encode_ansi(&events, true),
            "\x1b]8;;https://ex.test/p\x1b\\docs\x1b]8;;\x1b\\"
        );
        // Not styled: link degrades to its plain visible text.
        assert_eq!(encode_ansi(&events, false), "docs");
        // Plain encoding never emits the escape either.
        assert_eq!(encode_plain(&events), "docs");
    }

    #[test]
    fn user_header_matches_current_cli_bytes() {
        let (producer, terminal, _) = producer(Some(24));
        producer.user_header(
            Some(10),
            200,
            Some(TokenUsage {
                input_tokens: 10,
                output_tokens: 3,
                cached_input_tokens: 8,
            }),
            &["bash: sleep 99 (up 3s)".to_string()],
        );
        let events = terminal.events();

        let rule = "═".repeat(24);
        let expected = format!(
            "\x1b[0;1;36m{rule}\x1b[0m\n\
             \x1b[0;1;36mUSER 10/200 (5%)\x1b[0m\n\
             \x1b[0;1;36m⚙ last turn: input 10 (8 cached) · output 3\x1b[0m\n\
             \x1b[0;1;36m● bash: sleep 99 (up 3s)\x1b[0m\n\n"
        );
        assert_eq!(encode_ansi(&events, true), expected);
        // Colors off: same content, no escapes — the piped/`--color never` path.
        assert_eq!(encode_ansi(&events, false), strip_sgr(&expected));
    }

    #[test]
    fn startup_banner_matches_current_cli_bytes() {
        let (producer, terminal, _) = producer(None);
        producer.startup_banner("hy3-free", "993d14889c414aab81963843cccf8090 \"greeting\"");
        let plain = encode_plain(&terminal.events());
        // Head: the shared banner-open layout (rule, MYCO, blank line),
        // pinned differentially against the layout helper.
        let head = encode_plain(&banner_open_events("MYCO", None));
        let body = plain
            .strip_prefix(&head)
            .unwrap_or_else(|| panic!("banner must open with {head:?}: {plain:?}"));
        // Body lines: the CLI-chrome pin.
        assert_eq!(
            body,
            "Model: hy3-free\nSession: 993d14889c414aab81963843cccf8090 \"greeting\"\n\n\
             /help for commands\n\nAlt-Enter or Ctrl-J for newline\n"
        );
        // Styled: rule + MYCO are bold, body lines stay plain.
        let ansi = encode_ansi(&terminal.events(), true);
        assert!(ansi.contains("\x1b[0;1mMYCO\x1b[0m\n"));
        assert!(ansi.contains("\nModel: hy3-free\n"));
    }

    #[test]
    fn plain_encoding_is_structurally_stripped_ansi() {
        let (producer, terminal, _) = producer(Some(30));
        producer.user_header(Some(0), 100, None, &[]);
        text(
            &producer,
            "Some **bold** and `code` in a paragraph that wraps.",
        );
        finish(&producer);
        producer.error_section("boom");

        let events = terminal.events();
        // The invariant, structural: no Text event ever carries an escape…
        for event in &events {
            if let TuiEvent::Text(text) = event {
                assert!(!text.contains('\x1b'), "escape in Text: {text:?}");
            }
        }
        // …so plain == stripped ANSI with no stripper involved.
        assert_eq!(
            encode_plain(&events),
            strip_sgr(&encode_ansi(&events, true))
        );
        // And the stream carries real styling + chrome for the terminal.
        let ansi = encode_ansi(&events, true);
        assert!(ansi.contains("\x1b[0;1;36m"), "user chrome styled");
        assert!(ansi.contains("\x1b[0;1m"), "markdown bold styled");
        assert!(ansi.contains("ASSISTANT"));
        assert!(encode_plain(&events).contains("\nERROR\n\nboom\n"));
    }

    #[test]
    fn broadcast_delivers_identical_streams_to_terminal_and_mirror() {
        let (producer, terminal, mirror) = producer(None);
        producer.user_header(Some(1), 2, None, &[]);
        text(&producer, "hello");
        finish(&producer);
        assert_eq!(terminal.events(), mirror.events());
        assert!(!terminal.events().is_empty());
    }

    #[test]
    fn assistant_section_opens_once_per_turn() {
        let (producer, terminal, _) = producer(None);
        producer.user_header(Some(0), 1, None, &[]);
        text(&producer, "one");
        text(&producer, " two");
        finish(&producer);
        let plain = encode_plain(&terminal.events());
        assert_eq!(plain.matches("ASSISTANT · unknown\n").count(), 1);
        assert!(plain.contains("one two\n"));
        // Next user turn reopens the section.
        producer.user_header(Some(0), 1, None, &[]);
        text(&producer, "three");
        finish(&producer);
        let plain = encode_plain(&terminal.events());
        assert_eq!(plain.matches("ASSISTANT · unknown\n").count(), 2);
    }

    #[test]
    fn thinking_line_streams_dim_and_paragraphs_blank_separate() {
        let (producer, terminal, _) = producer(None);
        producer.user_header(Some(0), 1, None, &[]);
        think(&producer, "plan");
        think(&producer, " it");
        text(&producer, "done");
        finish(&producer);
        let plain = encode_plain(&terminal.events());
        // One ASSISTANT section: thinking line, blank line, answer text.
        assert!(
            plain.contains("ASSISTANT · unknown\n\nThinking: plan it\n\ndone\n"),
            "{plain:?}"
        );
        // The thinking line is dim on the terminal and closed before the answer.
        let ansi = encode_ansi(&terminal.events(), true);
        assert!(ansi.contains("\x1b[0;2mThinking: "), "{ansi:?}");
    }

    #[test]
    fn tool_paragraphs_blank_separate_inside_assistant() {
        let (producer, terminal, _) = producer(None);
        producer.user_header(Some(0), 1, None, &[]);
        text(&producer, "running now");
        tool(&producer, "bash", serde_json::json!({"command": "echo hi"}));
        text(&producer, "and after");
        finish(&producer);
        let plain = encode_plain(&terminal.events());
        assert!(
            plain.contains("running now\n\n╭─ bash ")
                && plain.contains("│ $ echo hi ")
                && plain.contains("╯\n\nand after\n"),
            "{plain:?}"
        );
        let ansi = encode_ansi(&terminal.events(), true);
        assert!(ansi.contains("\x1b[0;1;33m╭─ bash "), "{ansi:?}");
    }

    #[test]
    fn bash_wrapping_matches_live_replay_and_console() {
        let input = serde_json::json!({
            "action": "start",
            "session_id": "shell",
            "command": "bash --noprofile --norc",
            "stdin": "cargo test --locked --workspace --lib -- --nocapture\nprintf '%s\\n' '**done**'\n",
        });
        for verbose in [false, true] {
            for wrap in [None, Some(20), Some(40)] {
                let (producer, terminal, mirror) = producer(wrap);
                if verbose {
                    producer.toggle_verbose();
                }
                tool(&producer, "bash", input.clone());
                finish(&producer);
                let replay = history_events(
                    &[Message::AssistantMessage {
                        content: vec![],
                        tool_uses: vec![ToolUse {
                            name: "bash".into(),
                            input: input.clone(),
                        }],
                        turn_end_reason: None,
                    }],
                    Palette::colored(true).with_wrap(wrap).with_verbose(verbose),
                );
                assert_eq!(terminal.events(), mirror.events());
                assert_eq!(terminal.events(), replay);
                let plain = encode_plain(&replay);
                if verbose {
                    assert_eq!(plain.contains("↪ "), wrap.is_some());
                }
                assert_eq!(plain, strip_sgr(&encode_ansi(&replay, true)));
            }
        }
    }

    #[test]
    fn verbose_toggle_replays_full_recorded_output_and_applies_to_future_tools() {
        let (p, terminal, mirror) = producer(Some(48));
        let tool = ToolUse {
            name: "bash".into(),
            input: serde_json::json!({"command":"echo hi"}),
        };
        let result =
            ToolResult::text("output-1\noutput-2\noutput-3\noutput-4\noutput-5\noutput-6\n\x1b[2J")
                .with_status("exit 0");
        let messages = vec![
            Message::AssistantMessage {
                content: vec![],
                tool_uses: vec![tool.clone()],
                turn_end_reason: None,
            },
            Message::ToolResults {
                tool_use_results: vec![result.clone()],
            },
        ];
        p.replay_history(&messages);
        let compact = terminal.events();
        assert!(encode_plain(&compact).contains("output-4"));
        assert!(!encode_plain(&compact).contains("output-5"));
        assert!(p.toggle_verbose());
        p.replay_history(&messages);
        let full_events = terminal.events();
        let full = &full_events[compact.len()..];
        let output = encode_plain(full);
        assert!(output.contains("output-6"));
        assert!(output.contains("\\u{1b}[2J"));
        assert!(!output.contains('\x1b'));
        assert!(!output.contains("… /verbose"));
        assert!(!p.toggle_verbose());
        p.replay_history(&messages);
        assert_eq!(&terminal.events()[full_events.len()..], compact);
        assert!(mirror.events().is_empty());
        assert!(p.toggle_verbose());
        p.emit(AgentEvent::ToolStarted {
            tool_use: tool.clone(),
            context: ctx(0),
        });
        p.emit(AgentEvent::ToolFinished {
            tool_use: tool,
            result,
            context: ctx(0),
        });
        finish(&p);
        assert_eq!(mirror.events(), full);
    }

    #[test]
    fn nested_agent_events_are_ignored() {
        let (producer, terminal, mirror) = producer(None);
        text_at(&producer, 1, "worker noise");
        think_at(&producer, 1, "worker thought");
        tool_at(&producer, 1, "bash", serde_json::json!({}));
        finish_at(&producer, 1);
        assert!(terminal.events().is_empty());
        assert!(mirror.events().is_empty());
    }

    #[test]
    fn submitted_input_reaches_mirror_only_and_wraps() {
        let (producer, terminal, mirror) = producer(Some(10));
        producer.submitted_input("aaa bbb ccc ddd");
        assert!(terminal.events().is_empty());
        // Wrap-only, no markdown styling, one trailing newline.
        assert_eq!(encode_plain(&mirror.events()), "aaa bbb\nccc ddd\n");
        assert!(
            mirror
                .events()
                .iter()
                .all(|e| matches!(e, TuiEvent::Text(_)))
        );
    }

    #[test]
    fn replay_history_reaches_terminal_only() {
        let (producer, terminal, mirror) = producer(None);
        producer.replay_history(&[Message::UserMessage {
            content: vec![crate::generative_model::Content::Text {
                text: "hello".into(),
            }],
        }]);
        assert!(mirror.events().is_empty());
        let plain = encode_plain(&terminal.events());
        assert!(plain.contains("USER · unknown\n\nhello\n"), "{plain:?}");
    }

    #[test]
    fn myco_section_joins_the_section_family() {
        let (producer, terminal, mirror) = producer(None);
        producer.myco_section("effort=high");
        // Section layout (blank line, thin rule, header, blank line, body)…
        assert_eq!(
            encode_plain(&terminal.events()),
            format!("\n{SECTION_RULE}\nMYCO\n\neffort=high\n")
        );
        // …in the banner family's voice: bold-uncolored rule + header.
        let ansi = encode_ansi(&terminal.events(), true);
        assert!(ansi.contains(&format!("\x1b[0;1m{SECTION_RULE}\x1b[0m\n")));
        assert!(ansi.contains("\x1b[0;1mMYCO\x1b[0m\n"));
        assert_eq!(terminal.events(), mirror.events());
    }

    #[test]
    fn error_section_matches_write_error_section_bytes() {
        let (producer, terminal, _) = producer(None);
        producer.error_section("context length exceeded");
        // Same bytes as `write_error_section` (plain palette).
        let mut expected = Vec::new();
        write_error_section(&mut expected, "context length exceeded", Palette::plain()).unwrap();
        assert_eq!(
            encode_plain(&terminal.events()),
            String::from_utf8(expected).unwrap()
        );
    }
}
