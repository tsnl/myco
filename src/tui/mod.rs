//! Semantic presentation events for the terminal front-end — the **TUI
//! stream** (design sketch).
//!
//! Everything the interactive CLI shows is expressed as a flat stream of
//! [`TuiEvent`]s: content bytes ([`TuiEvent::Text`], escape-free, wrap
//! decisions already applied), style state changes ([`TuiEvent::Style`],
//! semantic attributes — no ANSI), and region markers ([`TuiEvent::Begin`],
//! zero-byte layout intent). Subscribers ([`TuiSink`]) are dumb encoders:
//!
//! - [`StdoutTuiSink`] encodes `Style` as SGR ([`Style::sgr`]) and writes to
//!   the terminal — byte-identical to today's output.
//! - [`ConsoleTuiSink`] ignores `Style` entirely and appends the `Text` bytes
//!   to the per-session `{id}.console` mirror.
//!
//! This makes the additive-only invariant *structural* instead of empirical:
//! `encode_plain(events)` equals `strip_sgr(encode_ansi(events))` by
//! construction, because styling and content are different event variants —
//! there is nothing to strip ([`crate::session::ConsoleLog`]'s `AnsiStripper`
//! becomes dead code once the CLI migrates to this stream).
//!
//! Events come from one producer ([`TuiProducer`]), which fans out to all
//! sinks configured at construction. It has two inputs, because chrome does
//! not originate in the agent:
//!
//! - it implements [`EventSink`], translating domain [`AgentEvent`]s
//!   (text/tool deltas) through the streaming markdown renderer's event path
//!   ([`MarkdownRenderer::feed_events`]);
//! - the REPL loop calls its chrome methods directly ([`Self::user_header`],
//!   [`Self::submitted_input`], [`Self::error_section`], …).
//!
//! **Sketch scope.** The producer ports the `CliEventSink` text/tool state
//! machine in reduced form: the thinking-summary line builder, usage
//! accounting, and nested-agent labeling are elided (`_ => {}` below) — the
//! migration moves them here verbatim and deletes `CliEventSink`. The startup
//! banner / preflight WARNING chrome methods follow the same pattern as
//! [`Self::error_section`]. Cursor repaints (input re-echo, resize reflow,
//! Ctrl-L) are deliberately **not** events: they are redraws of content
//! already in the stream, so they remain terminal-sink-local operations —
//! which is exactly why the console mirror never sees them.

use std::sync::{Arc, Mutex};

use crate::session::{
    AgentEvent, ConsoleLog, EventSink, MarkdownRenderer, Palette, TOOL_DISPLAY_STRING_MAX,
    render_block, section_rule, truncate_json_strings, user_rule,
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
/// not a delta — mirrors the CLI's SGR discipline of re-emitting the complete
/// style (`\x1b[0;…m`) on every change so an interrupted stream can't leak
/// styling.
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
    /// USER rule + header (bold cyan) — matches [`Palette::user`].
    pub const USER: Style = Style {
        bold: true,
        color: Some(Color::Cyan),
        ..Style::RESET
    };
    /// ASSISTANT rule + header (bold green) — matches [`Palette::assistant`].
    pub const ASSISTANT: Style = Style {
        bold: true,
        color: Some(Color::Green),
        ..Style::RESET
    };
    /// ERROR rule + header (bold red) — matches [`Palette::error`].
    pub const ERROR: Style = Style {
        bold: true,
        color: Some(Color::Red),
        ..Style::RESET
    };
    /// WARNING rule + header / tool names (bold yellow) — matches
    /// [`Palette::warning`] / [`Palette::tool_name`].
    pub const WARNING: Style = Style {
        bold: true,
        color: Some(Color::Yellow),
        ..Style::RESET
    };
    /// Startup banner (bold, uncolored) — matches [`Palette::banner`].
    pub const BANNER: Style = Style {
        bold: true,
        ..Style::RESET
    };
    /// Thinking paragraphs (dim) — matches [`Palette::thinking`].
    pub const THINKING: Style = Style {
        dim: true,
        ..Style::RESET
    };

    /// Encode as SGR, byte-identical to the CLI's existing escapes: attribute
    /// order dim(2), bold(1), italic(3), color — with the `0;` prefix that
    /// clears any style an interrupted stream left open. [`Style::RESET`]
    /// encodes as plain `\x1b[0m`.
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

/// Logical regions of the transcript, for sinks that lay out rather than
/// append (a future pane-based TUI, structured export). Byte-oriented sinks
/// ignore these — the producer has already rendered the region's chrome as
/// `Text`/`Style` events.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Region {
    Banner,
    Warning,
    UserTurn,
    Input,
    Assistant,
    Error,
}

/// One presentation event. The invariants that make sinks trivial:
///
/// - `Text` holds exactly the bytes a plain terminal would show — wrap
///   decisions applied, **never** any escape byte;
/// - `Style` carries semantics, not bytes — each sink chooses its encoding
///   (SGR, nothing, HTML classes, …);
/// - `Begin` is zero-width intent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TuiEvent {
    Begin(Region),
    Style(Style),
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

/// Encode for a terminal: `Text` verbatim, `Style` as SGR when `styled`
/// (the `--color` decision), `Begin` dropped.
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
            TuiEvent::Begin(_) => {}
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
/// produces escapes, `ConsoleLog`'s ANSI stripper is a pass-through here (and
/// is deleted outright when the CLI's remaining byte-tap call sites migrate
/// to this stream).
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
// Producer
// ---------------------------------------------------------------------------

/// Fan-out state guarded by one lock; events are built under the lock and
/// broadcast after it is released (same discipline as `CliEventSink`:
/// no sink IO while holding producer state).
struct ProducerState {
    wrap: Option<usize>,
    at_line_start: bool,
    assistant_open: bool,
    need_blank: bool,
    in_text_stream: bool,
    text_md: Option<MarkdownRenderer>,
}

/// The single producer of the TUI stream: translates domain [`AgentEvent`]s
/// and REPL chrome calls into [`TuiEvent`]s, fanned out to every sink
/// configured at construction.
pub struct TuiProducer {
    sinks: Vec<Arc<dyn TuiSink>>,
    state: Mutex<ProducerState>,
}

impl TuiProducer {
    pub fn new(sinks: Vec<Arc<dyn TuiSink>>, wrap: Option<usize>) -> Self {
        Self {
            sinks,
            state: Mutex::new(ProducerState {
                wrap,
                at_line_start: true,
                assistant_open: false,
                need_blank: false,
                in_text_stream: false,
                text_md: None,
            }),
        }
    }

    /// Update the wrap width after a terminal resize (an in-flight paragraph
    /// keeps the width its renderer was created with, as today).
    pub fn set_wrap(&self, wrap: Option<usize>) {
        self.with_state(|st| st.wrap = wrap);
        self.broadcast(Vec::new());
    }

    fn with_state<R>(&self, f: impl FnOnce(&mut ProducerState) -> R) -> R {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        f(&mut state)
    }

    fn broadcast(&self, events: Vec<TuiEvent>) {
        if events.is_empty() {
            return;
        }
        for sink in &self.sinks {
            sink.emit(&events);
        }
    }

    // -- chrome (called by the REPL loop, not derived from AgentEvents) -----

    /// USER rule + `USER <used>/<max>` header + blank line, byte-compatible
    /// with the current `run_repl` chrome. Resets per-turn stream state.
    pub fn user_header(&self, used: u64, max: u64) {
        let events = self.with_state(|st| {
            let mut events = vec![TuiEvent::Begin(Region::UserTurn)];
            styled_line(&mut events, Style::USER, &user_rule(st.wrap));
            styled_line(&mut events, Style::USER, &format!("USER {used}/{max}"));
            events.push(TuiEvent::Text("\n".into()));
            st.assistant_open = false;
            st.need_blank = false;
            st.in_text_stream = false;
            st.text_md = None;
            note_emitted(st, &events);
            events
        });
        self.broadcast(events);
    }

    /// The submitted input line, wrap-only (no markdown styling) — the same
    /// rendering the input re-echo and history replay use.
    pub fn submitted_input(&self, line: &str) {
        let events = self.with_state(|st| {
            let rendered = render_block(line, Palette::plain().with_wrap(st.wrap));
            let mut events = vec![TuiEvent::Begin(Region::Input)];
            events.push(TuiEvent::Text(format!("{rendered}\n")));
            note_emitted(st, &events);
            events
        });
        self.broadcast(events);
    }

    /// Headed ERROR section (live-only), byte-compatible with
    /// [`crate::session::write_error_section`].
    pub fn error_section(&self, message: &str) {
        let events = self.with_state(|st| {
            let mut events = vec![TuiEvent::Begin(Region::Error)];
            if !st.at_line_start {
                events.push(TuiEvent::Text("\n".into()));
            }
            events.push(TuiEvent::Text("\n".into()));
            styled_line(&mut events, Style::ERROR, &section_rule(st.wrap));
            styled_line(&mut events, Style::ERROR, "ERROR");
            events.push(TuiEvent::Text("\n".into()));
            let body = if message.ends_with('\n') {
                message.to_string()
            } else {
                format!("{message}\n")
            };
            events.push(TuiEvent::Text(body));
            st.assistant_open = false;
            st.in_text_stream = false;
            st.text_md = None;
            note_emitted(st, &events);
            events
        });
        self.broadcast(events);
    }

    /// Turn-cancelled notice (live-only).
    pub fn cancelled(&self) {
        let events = self.with_state(|st| {
            let events = vec![TuiEvent::Text("\n(cancelled)\n".into())];
            st.in_text_stream = false;
            st.text_md = None;
            note_emitted(st, &events);
            events
        });
        self.broadcast(events);
    }

    // -- AgentEvent translation --------------------------------------------

    fn text_delta(&self, text: &str) {
        let events = self.with_state(|st| {
            let mut events = Vec::new();
            ensure_assistant(st, &mut events);
            if !st.in_text_stream {
                if st.need_blank {
                    events.push(TuiEvent::Text("\n".into()));
                    st.need_blank = false;
                }
                st.in_text_stream = true;
                // Event-path renderer: `enabled` only gates the String facade
                // (`feed`); `feed_events` always separates style from content.
                st.text_md = Some(MarkdownRenderer::new(
                    Palette::colored(true).with_wrap(st.wrap),
                ));
            }
            if let Some(md) = st.text_md.as_mut() {
                events.extend(md.feed_events(text));
            }
            note_emitted(st, &events);
            events
        });
        self.broadcast(events);
    }

    fn tool_started(&self, name: &str, input: &serde_json::Value) {
        let events = self.with_state(|st| {
            let mut events = Vec::new();
            ensure_assistant(st, &mut events);
            end_text_stream(st, &mut events);
            if st.need_blank {
                events.push(TuiEvent::Text("\n".into()));
            }
            // `name(<pretty json>)` with only the name styled — the event
            // form of [`crate::session::format_tool_invocation`].
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
            st.need_blank = true;
            note_emitted(st, &events);
            events
        });
        self.broadcast(events);
    }

    fn turn_finished(&self) {
        let events = self.with_state(|st| {
            let mut events = Vec::new();
            end_text_stream(st, &mut events);
            note_emitted(st, &events);
            events
        });
        self.broadcast(events);
    }
}

impl EventSink for TuiProducer {
    fn emit(&self, event: AgentEvent) {
        match event {
            AgentEvent::TextDelta { text, .. } => self.text_delta(&text),
            AgentEvent::ToolStarted { tool_use, .. } => {
                self.tool_started(&tool_use.name, &tool_use.input)
            }
            AgentEvent::TurnFinished { .. } => self.turn_finished(),
            // Sketch: thinking-summary lines, usage accounting, and nested
            // agent lifecycle are elided here; the migration ports the rest
            // of CliEventSink's state machine into these arms.
            _ => {}
        }
    }
}

/// Rule/header line in a chrome color: style on, text, reset, newline —
/// the event form of [`Palette::paint`]'s `\x1b[0;…m{text}\x1b[0m` framing.
fn styled_line(events: &mut Vec<TuiEvent>, style: Style, text: &str) {
    events.push(TuiEvent::Style(style));
    events.push(TuiEvent::Text(text.to_string()));
    events.push(TuiEvent::Style(Style::RESET));
    events.push(TuiEvent::Text("\n".into()));
}

/// Open the ASSISTANT section once per agent turn (blank line, thin rule,
/// header, blank line) — the event form of `CliEventSink::ensure_assistant`.
fn ensure_assistant(st: &mut ProducerState, events: &mut Vec<TuiEvent>) {
    if st.assistant_open {
        return;
    }
    events.push(TuiEvent::Begin(Region::Assistant));
    if !st.at_line_start {
        events.push(TuiEvent::Text("\n".into()));
        st.at_line_start = true;
    }
    events.push(TuiEvent::Text("\n".into()));
    styled_line(events, Style::ASSISTANT, &section_rule(st.wrap));
    styled_line(events, Style::ASSISTANT, "ASSISTANT");
    events.push(TuiEvent::Text("\n".into()));
    st.assistant_open = true;
    st.need_blank = false;
}

/// Flush the in-flight answer paragraph (renderer finish + line close).
fn end_text_stream(st: &mut ProducerState, events: &mut Vec<TuiEvent>) {
    if let Some(mut md) = st.text_md.take() {
        events.extend(md.finish_events());
        if !md.ends_at_line_start() {
            events.push(TuiEvent::Text("\n".into()));
        }
        st.need_blank = true;
    }
    st.in_text_stream = false;
}

/// Track whether the stream sits at a visual line start (`Style` events are
/// zero-width and never affect this).
fn note_emitted(st: &mut ProducerState, events: &[TuiEvent]) {
    for event in events {
        if let TuiEvent::Text(text) = event
            && !text.is_empty()
        {
            st.at_line_start = text.ends_with('\n');
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::TraceContext;

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
    fn style_sgr_matches_palette_bytes() {
        // Chrome styles must encode to the exact escapes Palette emits today.
        let palette = Palette::colored(true);
        assert_eq!(
            format!("{}x{}", Style::USER.sgr(), Style::RESET.sgr()),
            palette.user("x")
        );
        assert_eq!(
            format!("{}x{}", Style::ASSISTANT.sgr(), Style::RESET.sgr()),
            palette.assistant("x")
        );
        assert_eq!(
            format!("{}x{}", Style::ERROR.sgr(), Style::RESET.sgr()),
            palette.error("x")
        );
        assert_eq!(
            format!("{}x{}", Style::WARNING.sgr(), Style::RESET.sgr()),
            palette.warning("x")
        );
        assert_eq!(
            format!("{}x{}", Style::BANNER.sgr(), Style::RESET.sgr()),
            palette.banner("x")
        );
        assert_eq!(
            format!("{}x{}", Style::THINKING.sgr(), Style::RESET.sgr()),
            palette.thinking("x")
        );
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
    fn user_header_matches_current_cli_bytes() {
        let capture = Arc::new(Capture::default());
        let producer = TuiProducer::new(vec![capture.clone()], Some(24));
        producer.user_header(10, 200);
        let events = capture.events();

        let palette = Palette::colored(true).with_wrap(Some(24));
        let expected = format!(
            "{}\n{}\n\n",
            palette.user(&user_rule(palette.wrap)),
            palette.user("USER 10/200")
        );
        assert_eq!(encode_ansi(&events, true), expected);
        // Colors off: same content, no escapes — the piped/`--color never` path.
        assert_eq!(encode_ansi(&events, false), strip_sgr(&expected));
    }

    #[test]
    fn plain_encoding_is_structurally_stripped_ansi() {
        let capture = Arc::new(Capture::default());
        let producer = TuiProducer::new(vec![capture.clone()], Some(30));
        producer.user_header(0, 100);
        producer.submitted_input("please make it **bold** somewhere");
        producer.emit(AgentEvent::TextDelta {
            text: "Some **bold** and `code` in a paragraph that wraps.".into(),
            context: TraceContext::root(),
        });
        producer.emit(AgentEvent::TurnFinished {
            reason: crate::generative_model::TurnEndReason::EndTurn,
            context: TraceContext::root(),
        });
        producer.error_section("boom");

        let events = capture.events();
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
        assert!(ansi.contains("ERROR"));
        // The input echo stays unstyled (wrap-only) but wrapped.
        assert!(encode_plain(&events).contains("please make it **bold**\nsomewhere\n"));
    }

    #[test]
    fn fan_out_delivers_identical_streams_to_every_sink() {
        let a = Arc::new(Capture::default());
        let b = Arc::new(Capture::default());
        let producer = TuiProducer::new(vec![a.clone(), b.clone()], None);
        producer.user_header(1, 2);
        producer.emit(AgentEvent::TextDelta {
            text: "hello".into(),
            context: TraceContext::root(),
        });
        producer.turn_finished();
        assert_eq!(a.events(), b.events());
        assert!(!a.events().is_empty());
    }

    #[test]
    fn assistant_section_opens_once_per_turn() {
        let capture = Arc::new(Capture::default());
        let producer = TuiProducer::new(vec![capture.clone()], None);
        producer.user_header(0, 1);
        producer.emit(AgentEvent::TextDelta {
            text: "one".into(),
            context: TraceContext::root(),
        });
        producer.emit(AgentEvent::TextDelta {
            text: " two".into(),
            context: TraceContext::root(),
        });
        producer.turn_finished();
        let plain = encode_plain(&capture.events());
        assert_eq!(plain.matches("ASSISTANT\n").count(), 1);
        assert!(plain.contains("one two\n"));
        // Next user turn reopens the section.
        producer.user_header(0, 1);
        producer.emit(AgentEvent::TextDelta {
            text: "three".into(),
            context: TraceContext::root(),
        });
        producer.turn_finished();
        let plain = encode_plain(&capture.events());
        assert_eq!(plain.matches("ASSISTANT\n").count(), 2);
    }
}
