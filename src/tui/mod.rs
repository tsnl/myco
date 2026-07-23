//! Semantic presentation events for the terminal front-end — the **TUI
//! stream**, and its single producer, the interactive CLI's `Ui`.
//!
//! Everything the interactive CLI shows is expressed as a flat stream of
//! [`TuiEvent`]s: content bytes ([`TuiEvent::Text`], escape-free, wrap
//! decisions already applied), style state changes ([`TuiEvent::Style`],
//! semantic attributes — no ANSI), and hyperlink spans ([`TuiEvent::Link`]).
//! Subscribers ([`TuiSink`]) are dumb encoders:
//!
//! - [`StdoutTuiSink`] encodes `Style` as SGR ([`Style::sgr`]) and `Link` as
//!   OSC 8, and writes to the terminal;
//! - [`ConsoleTuiSink`] ignores `Style`/`Link` entirely and appends the `Text`
//!   bytes to the per-session `{id}.console` mirror.
//!
//! This makes the mirror's escape-free invariant *structural*:
//! `encode_plain(events)` equals `strip_sgr(encode_ansi(events))` by
//! construction, because styling and content are different event variants —
//! there is nothing to strip.
//!
//! Events come from one producer ([`TuiProducer`]), which owns both sinks and
//! is the sole writer of user-visible output. It has two inputs, because
//! chrome does not originate in the agent:
//!
//! - it implements [`EventSink`], translating root-agent [`AgentEvent`]s
//!   (text/thinking/tool deltas; nested-agent events are ignored) through the
//!   streaming markdown renderer's event path
//!   ([`MarkdownRenderer::feed_events`]);
//! - the REPL loop calls its chrome methods directly ([`Self::user_header`],
//!   [`Self::error_section`], [`Self::line`], …).
//!
//! Two deliberate asymmetries between the sinks: the submitted input line
//! ([`Self::submitted_input`]) goes to the mirror only (the line editor
//! already echoed it to the terminal), and history replay
//! ([`Self::replay_history`]) goes to the terminal only (the mirror already
//! holds that content from when it streamed live). Cursor repaints (input
//! re-echo, resize/Ctrl-L clears) are not events at all: they are redraws of
//! content already in the stream, so they remain direct terminal writes —
//! which is exactly why the console mirror never sees them.
//!
//! The saved-history replay ([`crate::session::history_events`]) is built on
//! this module's section/paragraph helpers ([`SectionState`]), so live output
//! and replay share one layout policy.

use std::sync::{Arc, Mutex};

use crate::generative_model::{Message, TokenUsage};
use crate::session::{
    AgentEvent, ConsoleLog, EventSink, MarkdownRenderer, Palette, TOOL_DISPLAY_STRING_MAX,
    TraceContext, banner_rule, history_events, render_block, section_rule, truncate_json_strings,
    usage_line, user_header_line, user_rule,
};

// ---------------------------------------------------------------------------
// Events
// ---------------------------------------------------------------------------

/// Terminal color roles used by the CLI (encoded as SGR 31/32/33/36).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Color {
    Red,
    Green,
    Yellow,
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
    pub color: Option<Color>,
}

impl Style {
    /// Everything off — encodes as the SGR reset.
    pub const RESET: Style = Style {
        dim: false,
        bold: false,
        italic: false,
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
    /// Startup banner: bold, uncolored (distinct from the section palette).
    pub const BANNER: Style = Style {
        bold: true,
        ..Style::RESET
    };
    /// Thinking paragraphs: dim.
    pub const THINKING: Style = Style {
        dim: true,
        ..Style::RESET
    };

    /// Encode as SGR: attribute order dim(2), bold(1), italic(3), color —
    /// with the `0;` prefix that clears any style an interrupted stream left
    /// open. [`Style::RESET`] encodes as plain `\x1b[0m`.
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
        if let Some(color) = self.color {
            attrs.push(match color {
                Color::Red => "31",
                Color::Green => "32",
                Color::Yellow => "33",
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

/// True when `encode_ansi(events, styled)` would be empty.
pub(crate) fn encoded_is_empty(events: &[TuiEvent], styled: bool) -> bool {
    events.iter().all(|event| match event {
        TuiEvent::Text(text) => text.is_empty(),
        TuiEvent::Style(_) | TuiEvent::Link(_) => !styled,
    })
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

/// `name(<pretty json>)` with only the name styled — the tool paragraph
/// inside ASSISTANT, shared by the live producer and history replay.
pub(crate) fn tool_invocation_events(
    events: &mut Vec<TuiEvent>,
    name: &str,
    input: &serde_json::Value,
) {
    events.push(TuiEvent::Style(Style::WARNING));
    events.push(TuiEvent::Text(name.to_string()));
    events.push(TuiEvent::Style(Style::RESET));
    let display = truncate_json_strings(input, TOOL_DISPLAY_STRING_MAX);
    let body = match &display {
        serde_json::Value::Object(_) | serde_json::Value::Array(_) => {
            serde_json::to_string_pretty(&display).unwrap_or_else(|_| display.to_string())
        }
        other => other.to_string(),
    };
    events.push(TuiEvent::Text(format!("({body})\n")));
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
}

impl SectionState {
    pub fn new() -> Self {
        Self {
            at_line_start: true,
            assistant_open: false,
            need_blank: false,
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
        section_open_events(events, Style::ASSISTANT, "ASSISTANT", wrap);
        self.at_line_start = true;
        self.assistant_open = true;
        self.need_blank = false;
    }

    /// Blank line before a subsequent paragraph inside ASSISTANT.
    pub fn separate_paragraph_if_needed(&mut self, events: &mut Vec<TuiEvent>) {
        if self.need_blank {
            self.ensure_line_start(events);
            events.push(TuiEvent::Text("\n".into()));
            self.at_line_start = true;
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
/// ERROR ([`Self::error_section`]), WARNING ([`Self::warning_section`]).
/// Thinking summaries, tool invocations, and answer text are paragraphs
/// inside a single ASSISTANT section for the whole agent turn (including
/// multi-step tool loops). ERROR/WARNING sections are live-only and not
/// replayed.
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

    fn with_state<R>(&self, f: impl FnOnce(&mut ProducerState) -> R) -> R {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        f(&mut state)
    }

    fn palette(&self, wrap: Option<usize>) -> Palette {
        Palette::colored(self.colors).with_wrap(wrap)
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

    /// Replay saved history — **terminal only**: the mirror already holds this
    /// content from the run(s) that streamed it (`{id}.console` is opened for
    /// append). Used for `--resume`/`/resume` replay and the Ctrl-L / resize
    /// reprint.
    pub fn replay_history(&self, messages: &[Message]) {
        let wrap = self.with_state(|st| st.wrap);
        let events = history_events(messages, self.palette(wrap));
        if !events.is_empty() {
            self.terminal.emit(&events);
        }
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

    /// One plain line (newline appended) to terminal + mirror. `text` is
    /// content, not markup: it must not contain escape bytes.
    pub fn line(&self, text: &str) {
        self.text(&format!("{text}\n"));
    }

    /// Plain text verbatim to terminal + mirror (escape-free content only).
    pub fn text(&self, text: &str) {
        if text.is_empty() {
            return;
        }
        self.with_state(|st| st.section.at_line_start = text.ends_with('\n'));
        self.broadcast(vec![TuiEvent::Text(text.to_string())]);
    }

    /// Blank line to terminal + mirror (turn gaps, chrome separators).
    pub fn blank_line(&self) {
        self.line("");
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
                let md = st
                    .thinking_md
                    .insert(MarkdownRenderer::with_base(self.palette(st.wrap), "2"));
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
            let palette = self.palette(st.wrap);
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
            st.section.ensure_assistant(&mut events, st.wrap);
            st.section.separate_paragraph_if_needed(&mut events);
            tool_invocation_events(&mut events, name, input);
            st.section.at_line_start = true;
            st.in_text_stream = false;
            st.section.need_blank = true;
            events
        });
        self.broadcast(events);
    }

    fn turn_finished(&self) {
        let events = self.with_state(|st| {
            let mut events = Vec::new();
            finish_thinking_line(st, &mut events);
            end_text_stream(st, &mut events, self.colors);
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
            AgentEvent::TurnFinished {
                context: TraceContext { depth: 0, .. },
                ..
            } => self.turn_finished(),
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
    if !encoded_is_empty(&tail, colors) {
        st.section.at_line_start = encoded_ends_with_newline(&tail, colors);
        events.extend(tail);
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::generative_model::{ToolUse, TurnEndReason};

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

    fn ctx(depth: usize) -> TraceContext {
        TraceContext {
            depth,
            ..TraceContext::root()
        }
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
        assert_eq!(Style::THINKING.sgr(), "\x1b[0;2m");
        // Markdown styles match the renderer's attribute order.
        let bold_code = Style {
            bold: true,
            color: Some(Color::Cyan),
            ..Style::RESET
        };
        assert_eq!(bold_code.sgr(), "\x1b[0;1;36m");
        assert_eq!(Style::RESET.sgr(), "\x1b[0m");
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
        let expected = format!(
            "{rule}\nMYCO\n\nModel: hy3-free\n\
             Session: 993d14889c414aab81963843cccf8090 \"greeting\"\n\n\
             /help for commands\n\nAlt-Enter or Ctrl-J for newline\n",
            rule = banner_rule(None)
        );
        assert_eq!(encode_plain(&terminal.events()), expected);
        // Styled: rule + MYCO are bold, body lines stay plain.
        let ansi = encode_ansi(&terminal.events(), true);
        assert!(ansi.contains("\x1b[0;1mMYCO\x1b[0m\n"));
        assert!(ansi.contains("\nModel: hy3-free\n"));
    }

    #[test]
    fn plain_encoding_is_structurally_stripped_ansi() {
        let (producer, terminal, _) = producer(Some(30));
        producer.user_header(Some(0), 100, None, &[]);
        producer.emit(AgentEvent::TextDelta {
            text: "Some **bold** and `code` in a paragraph that wraps.".into(),
            context: ctx(0),
        });
        producer.emit(AgentEvent::TurnFinished {
            reason: TurnEndReason::EndTurn,
            context: ctx(0),
        });
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
        producer.emit(AgentEvent::TextDelta {
            text: "hello".into(),
            context: ctx(0),
        });
        producer.emit(AgentEvent::TurnFinished {
            reason: TurnEndReason::EndTurn,
            context: ctx(0),
        });
        assert_eq!(terminal.events(), mirror.events());
        assert!(!terminal.events().is_empty());
    }

    #[test]
    fn assistant_section_opens_once_per_turn() {
        let (producer, terminal, _) = producer(None);
        producer.user_header(Some(0), 1, None, &[]);
        producer.emit(AgentEvent::TextDelta {
            text: "one".into(),
            context: ctx(0),
        });
        producer.emit(AgentEvent::TextDelta {
            text: " two".into(),
            context: ctx(0),
        });
        producer.emit(AgentEvent::TurnFinished {
            reason: TurnEndReason::EndTurn,
            context: ctx(0),
        });
        let plain = encode_plain(&terminal.events());
        assert_eq!(plain.matches("ASSISTANT\n").count(), 1);
        assert!(plain.contains("one two\n"));
        // Next user turn reopens the section.
        producer.user_header(Some(0), 1, None, &[]);
        producer.emit(AgentEvent::TextDelta {
            text: "three".into(),
            context: ctx(0),
        });
        producer.emit(AgentEvent::TurnFinished {
            reason: TurnEndReason::EndTurn,
            context: ctx(0),
        });
        let plain = encode_plain(&terminal.events());
        assert_eq!(plain.matches("ASSISTANT\n").count(), 2);
    }

    #[test]
    fn thinking_line_streams_dim_and_paragraphs_blank_separate() {
        let (producer, terminal, _) = producer(None);
        producer.user_header(Some(0), 1, None, &[]);
        producer.emit(AgentEvent::ThinkingDelta {
            text: "plan".into(),
            context: ctx(0),
        });
        producer.emit(AgentEvent::ThinkingDelta {
            text: " it".into(),
            context: ctx(0),
        });
        producer.emit(AgentEvent::TextDelta {
            text: "done".into(),
            context: ctx(0),
        });
        producer.emit(AgentEvent::TurnFinished {
            reason: TurnEndReason::EndTurn,
            context: ctx(0),
        });
        let plain = encode_plain(&terminal.events());
        // One ASSISTANT section: thinking line, blank line, answer text.
        assert!(
            plain.contains("ASSISTANT\n\nThinking: plan it\n\ndone\n"),
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
        producer.emit(AgentEvent::TextDelta {
            text: "running now".into(),
            context: ctx(0),
        });
        producer.emit(AgentEvent::ToolStarted {
            tool_use: ToolUse {
                id: "t1".into(),
                name: "bash".into(),
                input: serde_json::json!({"command": "echo hi"}),
            },
            context: ctx(0),
        });
        producer.emit(AgentEvent::TextDelta {
            text: "and after".into(),
            context: ctx(0),
        });
        producer.emit(AgentEvent::TurnFinished {
            reason: TurnEndReason::EndTurn,
            context: ctx(0),
        });
        let plain = encode_plain(&terminal.events());
        assert!(
            plain.contains("running now\n\nbash({\n  \"command\": \"echo hi\"\n})\n\nand after\n"),
            "{plain:?}"
        );
        // Only the tool name is styled (bold yellow), the JSON body is plain.
        let ansi = encode_ansi(&terminal.events(), true);
        assert!(ansi.contains("\x1b[0;1;33mbash\x1b[0m({"), "{ansi:?}");
    }

    #[test]
    fn nested_agent_events_are_ignored() {
        let (producer, terminal, mirror) = producer(None);
        producer.emit(AgentEvent::TextDelta {
            text: "worker noise".into(),
            context: ctx(1),
        });
        producer.emit(AgentEvent::ThinkingDelta {
            text: "worker thought".into(),
            context: ctx(1),
        });
        producer.emit(AgentEvent::ToolStarted {
            tool_use: ToolUse {
                id: "t1".into(),
                name: "bash".into(),
                input: serde_json::json!({}),
            },
            context: ctx(1),
        });
        producer.emit(AgentEvent::TurnFinished {
            reason: TurnEndReason::EndTurn,
            context: ctx(1),
        });
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
        assert!(plain.contains("USER\n\nhello\n"), "{plain:?}");
    }

    #[test]
    fn error_section_matches_write_error_section_bytes() {
        let (producer, terminal, _) = producer(None);
        producer.error_section("context length exceeded");
        // Same bytes as `crate::session::write_error_section` (plain palette).
        let mut expected = Vec::new();
        crate::session::write_error_section(
            &mut expected,
            "context length exceeded",
            Palette::plain(),
        )
        .unwrap();
        assert_eq!(
            encode_plain(&terminal.events()),
            String::from_utf8(expected).unwrap()
        );
    }
}
