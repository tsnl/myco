//! Streaming Markdown renderer with fence-aware word wrap.
//!
//! Feed arbitrary UTF-8 chunks ([`MarkdownRenderer::feed`]); deltas may split
//! anywhere, including inside a delimiter run. Two invariants:
//!
//! - **Additive-only**: every content byte reaches the output in order.
//!   Styling only injects SGR sequences; wrapping only exchanges a run of
//!   breakable spaces for a newline (plus hanging indent). A stray delimiter
//!   can mis-style the tail of one paragraph, never corrupt content.
//! - **Disabled = identity**: styling off and wrap off ⇒ output is
//!   byte-identical to input (the non-TTY / [`Palette::plain`] guarantee).
//!
//! Internally the renderer is event-first: it produces a [`TuiEvent`] stream
//! in which content ([`TuiEvent::Text`], escape-free, wrap applied) and style
//! state ([`TuiEvent::Style`], semantic) are separate variants
//! ([`MarkdownRenderer::feed_events`] / [`MarkdownRenderer::finish_events`]).
//! The String API ([`MarkdownRenderer::feed`] / [`MarkdownRenderer::finish`])
//! is a facade that SGR-encodes that stream ([`crate::tui::encode_ansi`]),
//! gated by the palette's `enabled` flag — bytes are unchanged from the
//! pre-event renderer.
//!
//! Supported: `**` / `*` emphasis toggles (with a light flanking check),
//! `` ` `` inline code, ATX headers, fenced code blocks (never styled, never
//! wrapped), indented (4-space) lines verbatim, list hanging indent.
//! Out of scope — constructs that need non-linear layout: tables, setext
//! headers, reference links.

use unicode_width::UnicodeWidthChar;

use super::transcript::Palette;
use crate::tui::{Color, Style, TuiEvent, encode_ansi};

/// What the current physical line is, decided from its first characters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Line {
    /// Buffering the first characters to classify the line.
    Prefix,
    /// Ordinary inline text: styles + wrap apply.
    Body,
    /// Verbatim until end of line (indented code, tab-led lines).
    Raw,
}

#[derive(Debug)]
pub struct MarkdownRenderer {
    /// Whether the String facade ([`Self::feed`]) encodes `Style` events as
    /// SGR. The event path always carries them; encoders decide.
    styled: bool,
    wrap: Option<usize>,
    /// Style applied under all markdown styles (e.g. dim keeps thinking
    /// paragraphs dim). Default for normal text.
    base: Style,

    out: Vec<TuiEvent>,
    /// Last style emitted, to skip redundant events. `None` → none yet.
    last_style: Option<Style>,
    /// Last visible char pushed to `out` was a newline (or nothing yet).
    emitted_line_start: bool,

    bold: bool,
    italic: bool,
    code: bool,
    header: bool,

    line: Line,
    prefix: String,
    /// Inside a fenced block: (fence char, open-run length for close matching).
    fence: Option<(char, usize)>,
    /// Buffering a fence-block line start that may still be the closing fence.
    fence_line: Option<String>,
    /// Current line has non-whitespace content (blank line = paragraph break).
    line_has_content: bool,
    /// Last content char on this line, for the emphasis flanking check.
    prev_char: Option<char>,

    /// Pending delimiter run: (char, length). Survives chunk boundaries.
    run: Option<(char, usize)>,

    col: usize,
    word: String,
    /// Style changes queued at byte offsets inside the pending `word`, so
    /// style events ride the wrap decision with it and never dangle across a
    /// break (the event-form of splicing SGR into the word buffer).
    word_marks: Vec<(usize, Style)>,
    word_width: usize,
    spaces: usize,
    hang: usize,
    /// Word already reached the wrap width; stream it raw until a break.
    overflow: bool,
}

impl MarkdownRenderer {
    pub fn new(palette: Palette) -> Self {
        Self::with_base(palette, "")
    }

    /// Renderer whose resets return to `base` SGR attributes instead of plain
    /// (`"2"` for dim thinking paragraphs — the only non-empty base in use).
    /// The base is emitted up front.
    pub fn with_base(palette: Palette, base: &'static str) -> Self {
        debug_assert!(base.is_empty() || base == "2", "unsupported base {base:?}");
        let base = Style {
            dim: base == "2",
            ..Style::RESET
        };
        let mut r = Self {
            styled: palette.enabled,
            wrap: palette.wrap,
            base,
            out: Vec::new(),
            last_style: None,
            emitted_line_start: true,
            bold: false,
            italic: false,
            code: false,
            header: false,
            line: Line::Prefix,
            prefix: String::new(),
            fence: None,
            fence_line: None,
            line_has_content: false,
            prev_char: None,
            run: None,
            col: 0,
            word: String::new(),
            word_marks: Vec::new(),
            word_width: 0,
            spaces: 0,
            hang: 0,
            overflow: false,
        };
        if r.base != Style::RESET {
            r.out.push(TuiEvent::Style(r.base));
            r.last_style = Some(r.base);
        }
        r
    }

    /// String facade: SGR-encode the event stream (escapes only when the
    /// palette enabled styling).
    pub fn feed(&mut self, chunk: &str) -> String {
        let events = self.feed_events(chunk);
        encode_ansi(&events, self.styled)
    }

    /// Event path: content and style state as separate [`TuiEvent`] variants.
    /// `Text` events never contain escape bytes.
    pub fn feed_events(&mut self, chunk: &str) -> Vec<TuiEvent> {
        for c in chunk.chars() {
            self.push_char(c);
        }
        std::mem::take(&mut self.out)
    }

    /// Flush pending word/run and close any open styling (String facade).
    pub fn finish(&mut self) -> String {
        let events = self.finish_events();
        encode_ansi(&events, self.styled)
    }

    /// Event-path [`Self::finish`].
    pub fn finish_events(&mut self) -> Vec<TuiEvent> {
        match self.line {
            Line::Prefix if self.fence.is_none() => self.end_prefix(None),
            _ => {
                if let Some(buf) = self.fence_line.take() {
                    self.out_str(&buf);
                }
            }
        }
        self.resolve_run(None);
        self.flush_word();
        self.emit_spaces();
        if self.last_style.is_some_and(|s| s != Style::RESET) {
            self.out.push(TuiEvent::Style(Style::RESET));
            self.last_style = Some(Style::RESET);
        }
        std::mem::take(&mut self.out)
    }

    /// True when everything emitted so far ends at a line start.
    pub fn ends_at_line_start(&self) -> bool {
        self.emitted_line_start
    }

    // -- dispatch ----------------------------------------------------------

    fn push_char(&mut self, c: char) {
        if self.fence.is_some() {
            self.fence_char(c);
        } else {
            match self.line {
                Line::Prefix => self.prefix_char(c),
                Line::Body => self.inline_char(c),
                Line::Raw => self.raw_char(c),
            }
        }
        // Without wrap there is no need to hold words back; only delimiter
        // runs may buffer across chunks.
        if self.wrap.is_none() && self.run.is_none() && self.line == Line::Body {
            self.flush_word();
            self.emit_spaces();
        }
    }

    // -- line-start classification ----------------------------------------

    fn prefix_char(&mut self, c: char) {
        if c == '\n' {
            self.end_prefix(Some('\n'));
            return;
        }
        if prefix_still_open(&self.prefix, c) {
            self.prefix.push(c);
            return;
        }
        self.classify_prefix(c);
    }

    /// `c` is the first char that decides what this line is (not yet buffered).
    fn classify_prefix(&mut self, c: char) {
        let prefix = std::mem::take(&mut self.prefix);
        let (indent, stripped) = split_indent(&prefix);

        // 4-space (or tab) indent: verbatim line, no styles, no wrap.
        if stripped.is_empty() && (c == '\t' || (indent >= 3 && c == ' ')) {
            self.line = Line::Raw;
            self.out_str(&prefix);
            self.out_ch(c);
            return;
        }
        // Fence open: ``` / ~~~ (+ info string until end of line).
        if let Some((fc, n)) = fence_run(stripped) {
            self.line = Line::Body; // restored by the fence-close handler
            self.out_str(&prefix);
            self.fence = Some((fc, n));
            self.fence_line = None;
            self.fence_char(c);
            return;
        }
        // ATX header: #{1,6} followed by a space.
        if !stripped.is_empty()
            && stripped.len() <= 6
            && stripped.chars().all(|h| h == '#')
            && c == ' '
        {
            self.line = Line::Body;
            self.header = true;
            self.emit_style();
            self.replay_literal(&prefix);
            self.inline_char(c);
            return;
        }
        // Bullets: `- ` / `+ ` / `* ` / `1. ` / `1) ` — hanging indent for
        // wrapped continuations. `> ` passes through with no special layout.
        if c == ' ' && (is_bullet_marker(stripped) || stripped == ">") {
            if stripped != ">" {
                self.hang = display_width(&prefix) + 1;
            }
            self.line = Line::Body;
            self.replay_literal(&prefix);
            self.inline_char(c);
            return;
        }
        // Ordinary text: replay through the inline machine so a line-leading
        // `**bold` or `` `code` `` still styles.
        self.line = Line::Body;
        for pc in prefix.chars() {
            self.inline_char(pc);
        }
        self.inline_char(c);
    }

    /// Line ended (or stream finished) while still classifying.
    fn end_prefix(&mut self, newline: Option<char>) {
        let prefix = std::mem::take(&mut self.prefix);
        let (_, stripped) = split_indent(&prefix);
        if let Some((fc, n)) = fence_run(stripped) {
            self.out_str(&prefix);
            self.fence = Some((fc, n));
            self.fence_line = None;
            if newline.is_some() {
                self.out_ch('\n');
                self.fence_line = Some(String::new());
            }
            return;
        }
        self.line = Line::Body;
        for pc in prefix.chars() {
            self.inline_char(pc);
        }
        if let Some(nl) = newline {
            self.inline_char(nl);
        }
    }

    /// Replay classified structural markers (`- `, `# `, …) as plain content,
    /// bypassing delimiter detection so a `* ` bullet never opens emphasis.
    fn replay_literal(&mut self, prefix: &str) {
        for pc in prefix.chars() {
            if pc == ' ' {
                self.flush_word();
                self.spaces += 1;
            } else {
                self.add_content_char(pc);
            }
        }
    }

    // -- fenced blocks ------------------------------------------------------

    fn fence_char(&mut self, c: char) {
        let Some((fc, open_len)) = self.fence else {
            return;
        };
        let Some(buf) = self.fence_line.as_mut() else {
            // Mid-line (fence-open info string, or a flushed content line).
            self.out_ch(c);
            if c == '\n' {
                self.fence_line = Some(String::new());
            }
            return;
        };
        if c == '\n' {
            let line = std::mem::take(buf);
            self.fence_line = Some(String::new());
            let closes = fence_run(split_indent(&line).1.trim_end_matches(' '))
                .is_some_and(|(ch, n)| ch == fc && n >= open_len);
            self.out_str(&line);
            self.out_ch('\n');
            if closes {
                self.fence = None;
                self.fence_line = None;
                self.reset_line();
            }
            return;
        }
        buf.push(c);
        // Deviated from a plausible closing fence → verbatim content line.
        let (_, stripped) = split_indent(buf);
        let plausible = stripped.chars().all(|ch| ch == fc)
            || fence_run(stripped.trim_end_matches(' ')).is_some_and(|(ch, _)| ch == fc);
        if !plausible {
            let line = std::mem::take(buf);
            self.fence_line = None;
            self.out_str(&line);
        }
    }

    // -- raw (indented) lines ----------------------------------------------

    fn raw_char(&mut self, c: char) {
        self.out_ch(c);
        if c == '\n' {
            self.reset_line();
        }
    }

    // -- inline text --------------------------------------------------------

    fn inline_char(&mut self, c: char) {
        if let Some((rc, n)) = self.run {
            if c == rc {
                self.run = Some((rc, n + 1));
                return;
            }
            self.resolve_run(Some(c));
        }
        match c {
            '\n' => self.end_line(),
            ' ' => {
                self.flush_word();
                self.spaces += 1;
                self.overflow = false;
                self.prev_char = Some(' ');
            }
            '*' if !self.code => self.run = Some(('*', 1)),
            '`' => self.run = Some(('`', 1)),
            _ => self.add_content_char(c),
        }
    }

    fn end_line(&mut self) {
        self.flush_word();
        self.emit_spaces();
        if self.header {
            self.header = false;
            self.emit_style();
        }
        if !self.line_has_content && (self.bold || self.italic || self.code) {
            // Blank line = paragraph boundary: drop unclosed inline styles so a
            // stray delimiter's blast radius ends here.
            self.bold = false;
            self.italic = false;
            self.code = false;
            self.emit_style();
        }
        self.flush_word(); // style codes queued above ride out before the break
        self.out_ch('\n');
        self.reset_line();
    }

    fn reset_line(&mut self) {
        self.line = Line::Prefix;
        self.prefix.clear();
        self.line_has_content = false;
        self.prev_char = None;
        self.col = 0;
        self.hang = 0;
        self.overflow = false;
    }

    /// A delimiter run ended; `next` is the char after it (`None` at stream
    /// or line end). Emits the delimiters as content, then toggles styles —
    /// so delimiters stay visible and a misread is purely cosmetic.
    fn resolve_run(&mut self, next: Option<char>) {
        let Some((rc, n)) = self.run.take() else {
            return;
        };
        // Captured before the delimiters go out as content: the char in front
        // of the run, for the flanking check below.
        let left_flank = self.prev_char.is_some_and(|c| !c.is_whitespace());
        for _ in 0..n {
            self.add_content_char(rc);
        }
        match rc {
            '*' if n <= 3 => {
                let (bold, italic) = match n {
                    1 => (false, true),
                    2 => (true, false),
                    _ => (true, true),
                };
                // Flanking-lite: opening needs a following non-space
                // (`2 ** 3` stays literal); closing needs a preceding one.
                let opening = (bold && !self.bold) || (italic && !self.italic);
                let allowed = if opening {
                    next.is_some_and(|c| !c.is_whitespace())
                } else {
                    left_flank
                };
                if allowed {
                    if bold {
                        self.bold = !self.bold;
                    }
                    if italic {
                        self.italic = !self.italic;
                    }
                    self.emit_style();
                }
            }
            '`' if n == 1 => {
                self.code = !self.code;
                self.emit_style();
            }
            _ => {}
        }
    }

    fn add_content_char(&mut self, c: char) {
        if !c.is_whitespace() {
            self.line_has_content = true;
        }
        self.prev_char = Some(c);
        let w = UnicodeWidthChar::width(c).unwrap_or(0);
        if self.overflow {
            if !self.word.is_empty() || !self.word_marks.is_empty() {
                self.emit_word_raw();
            }
            self.out_ch(c);
            self.col += w;
            return;
        }
        self.word.push(c);
        self.word_width += w;
        if let Some(width) = self.wrap
            && self.word_width >= width
        {
            self.flush_word();
            self.overflow = true;
        }
    }

    // -- wrap machinery -----------------------------------------------------

    fn flush_word(&mut self) {
        if self.word.is_empty() && self.word_marks.is_empty() {
            return;
        }
        if let Some(width) = self.wrap
            && self.col > 0
            && self.spaces > 0
            && self.word_width > 0
            && self.col + self.spaces + self.word_width > width
        {
            // Break: the run of breakable spaces becomes the newline.
            self.spaces = 0;
            self.out_ch('\n');
            let hang = self.hang.min(width.saturating_sub(1));
            for _ in 0..hang {
                self.out_ch(' ');
            }
            self.col = hang;
        }
        self.emit_spaces();
        self.emit_word_raw();
    }

    /// Emit the pending word, interleaving queued style marks at their byte
    /// offsets (the wrap decision, if any, has already been made).
    fn emit_word_raw(&mut self) {
        let word = std::mem::take(&mut self.word);
        let marks = std::mem::take(&mut self.word_marks);
        let mut pos = 0;
        for (off, style) in marks {
            if off > pos {
                self.out_str(&word[pos..off]);
                pos = off;
            }
            self.out.push(TuiEvent::Style(style));
        }
        if pos < word.len() {
            self.out_str(&word[pos..]);
        }
        self.col += self.word_width;
        self.word_width = 0;
    }

    fn emit_spaces(&mut self) {
        for _ in 0..self.spaces {
            self.out_ch(' ');
        }
        self.col += self.spaces;
        self.spaces = 0;
    }

    // -- styling ------------------------------------------------------------

    /// The semantic style for the current toggle state, merged over `base`.
    /// SGR encoding ([`Style::sgr`]) reproduces the historical attribute
    /// order: dim, bold (header or `**`), italic, color.
    fn current_style(&self) -> Style {
        Style {
            dim: self.base.dim,
            bold: self.base.bold || self.header || self.bold,
            italic: self.base.italic || self.italic,
            color: if self.code {
                Some(Color::Cyan)
            } else {
                self.base.color
            },
        }
    }

    /// Queue a style event for the current state (zero display width). Rides
    /// inside the pending word so styles never dangle across a wrap break.
    fn emit_style(&mut self) {
        let style = self.current_style();
        if self.last_style == Some(style) {
            return;
        }
        self.last_style = Some(style);
        if self.overflow {
            self.out.push(TuiEvent::Style(style));
        } else {
            self.word_marks.push((self.word.len(), style));
        }
    }

    // -- output helpers (track visual line starts) --------------------------

    fn out_ch(&mut self, c: char) {
        self.emitted_line_start = c == '\n';
        if let Some(TuiEvent::Text(text)) = self.out.last_mut() {
            text.push(c);
        } else {
            self.out.push(TuiEvent::Text(c.to_string()));
        }
    }

    fn out_str(&mut self, s: &str) {
        if s.is_empty() {
            return;
        }
        self.emitted_line_start = s.ends_with('\n');
        if let Some(TuiEvent::Text(text)) = self.out.last_mut() {
            text.push_str(s);
        } else {
            self.out.push(TuiEvent::Text(s.to_string()));
        }
    }
}

/// Render a complete block (replay / non-streaming callers).
pub fn render_block(text: &str, palette: Palette) -> String {
    render_block_with_base(text, palette, "")
}

pub fn render_block_with_base(text: &str, palette: Palette, base: &'static str) -> String {
    let mut r = MarkdownRenderer::with_base(palette, base);
    let mut out = r.feed(text);
    out.push_str(&r.finish());
    out
}

// -- prefix classification helpers ------------------------------------------

/// Leading-space indent (max 3 counted) and the marker chars after it.
fn split_indent(prefix: &str) -> (usize, &str) {
    let indent = prefix.len() - prefix.trim_start_matches(' ').len();
    (indent, &prefix[indent..])
}

/// `Some((fence char, run length))` when `s` is a ``` / ~~~ run of ≥ 3.
fn fence_run(s: &str) -> Option<(char, usize)> {
    let c = s.chars().next()?;
    if (c == '`' || c == '~') && s.len() >= 3 && s.chars().all(|ch| ch == c) {
        Some((c, s.len()))
    } else {
        None
    }
}

fn is_bullet_marker(s: &str) -> bool {
    matches!(s, "-" | "+" | "*")
        || (s.len() >= 2
            && s.ends_with(['.', ')'])
            && s[..s.len() - 1].chars().all(|c| c.is_ascii_digit()))
}

/// Could `prefix + c` still become a structural marker? While true, chars are
/// buffered; the first char that decides goes to [`MarkdownRenderer::classify_prefix`].
fn prefix_still_open(prefix: &str, c: char) -> bool {
    let (indent, stripped) = split_indent(prefix);
    if stripped.is_empty() {
        return match c {
            ' ' => indent < 3,
            '#' | '`' | '~' | '-' | '+' | '*' | '>' => true,
            _ => c.is_ascii_digit(),
        };
    }
    let first = stripped.chars().next().unwrap();
    match first {
        '#' => c == '#' && stripped.len() < 6 && stripped.chars().all(|h| h == '#'),
        '`' | '~' => c == first && stripped.chars().all(|h| h == first),
        d if d.is_ascii_digit() => {
            stripped.len() < 4
                && stripped.chars().all(|x| x.is_ascii_digit())
                && (c.is_ascii_digit() || c == '.' || c == ')')
        }
        _ => false,
    }
}

fn display_width(s: &str) -> usize {
    s.chars()
        .map(|c| UnicodeWidthChar::width(c).unwrap_or(0))
        .sum()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn plain() -> Palette {
        Palette::plain()
    }

    fn styled() -> Palette {
        Palette::colored(true)
    }

    fn wrapped(width: usize) -> Palette {
        Palette::plain().with_wrap(Some(width))
    }

    fn render(text: &str, palette: Palette) -> String {
        render_block(text, palette)
    }

    /// Feed one char at a time — every delimiter run crosses a chunk boundary.
    fn render_char_chunks(text: &str, palette: Palette) -> String {
        let mut r = MarkdownRenderer::new(palette);
        let mut out = String::new();
        for c in text.chars() {
            out.push_str(&r.feed(&c.to_string()));
        }
        out.push_str(&r.finish());
        out
    }

    #[test]
    fn disabled_renderer_is_byte_identical() {
        let inputs = [
            "plain text\n",
            "text with **bold** and *italic* and `code`\n",
            "# Header\n\nbody  with  double  spaces \n",
            "- bullet one\n- bullet two with a * star\n",
            "```rust\nfn main() { println!(\"**not bold**\"); }\n```\ntail\n",
            "    indented code * ` #\nnormal\n",
            "1. ordered\n12) also ordered\n1.5 not a bullet\n",
            "unterminated **bold and `code\n\nnext paragraph\n",
            "no trailing newline **",
            "CJK 宽度测试 and emoji 🚀 pass through\n",
            "~~~\ntilde fence\n~~~\n",
            "> quoted\n>> nested\n",
            "####### seven hashes\n#nospace\n",
        ];
        for input in inputs {
            assert_eq!(render(input, plain()), input, "single-feed: {input:?}");
            assert_eq!(
                render_char_chunks(input, plain()),
                input,
                "char-chunked: {input:?}"
            );
        }
    }

    #[test]
    fn bold_and_italic_toggle_around_delimiters() {
        assert_eq!(render("a **b** c", styled()), "a **\x1b[0;1mb**\x1b[0m c");
        assert_eq!(render("x *it* y", styled()), "x *\x1b[0;3mit*\x1b[0m y");
        // *** toggles both on and both off.
        let out = render("***both*** end", styled());
        assert!(out.contains("\x1b[0;1;3m"), "{out:?}");
        assert!(out.ends_with("\x1b[0m end"), "{out:?}");
    }

    #[test]
    fn flanking_keeps_spaced_stars_literal() {
        // `2 ** 3` and `a * b`: opening delimiter followed by a space stays text.
        assert_eq!(render("2 ** 3", styled()), "2 ** 3");
        assert_eq!(render("a * b * c", styled()), "a * b * c");
    }

    #[test]
    fn inline_code_styles_and_shields_emphasis() {
        assert_eq!(render("see `x`.", styled()), "see `\x1b[0;36mx`\x1b[0m.");
        // Stars inside a code span are literal.
        let out = render("`a * b * c`", styled());
        assert!(!out.contains("[0;3m"), "{out:?}");
    }

    #[test]
    fn atx_header_is_bold_for_the_line() {
        assert_eq!(
            render("# Title\nbody", styled()),
            "\x1b[0;1m# Title\x1b[0m\nbody"
        );
        // 7 hashes / no space: not a header.
        assert_eq!(render("####### x", styled()), "####### x");
        assert_eq!(render("#nospace", styled()), "#nospace");
    }

    #[test]
    fn unclosed_styles_reset_at_paragraph_boundary() {
        let out = render("**unclosed\n\nnext", styled());
        let reset_at = out.find("\x1b[0m").expect("reset emitted");
        let next_at = out.find("next").unwrap();
        assert!(reset_at < next_at, "{out:?}");
        assert!(!out[next_at..].contains('\x1b'), "{out:?}");
    }

    #[test]
    fn finish_closes_open_styles() {
        assert_eq!(render("**a", styled()), "**\x1b[0;1ma\x1b[0m");
        // No markdown → no escapes at all.
        assert_eq!(render("hello", styled()), "hello");
    }

    #[test]
    fn base_style_matches_palette_thinking_bytes() {
        // Must equal Palette::thinking()'s framing for plain content.
        let expected = styled().thinking("Thinking: pondering");
        assert_eq!(
            render_block_with_base("Thinking: pondering", styled(), "2"),
            expected
        );
        // Plain palette: identity.
        assert_eq!(
            render_block_with_base("Thinking: pondering", plain(), "2"),
            "Thinking: pondering"
        );
    }

    #[test]
    fn wrap_breaks_at_spaces() {
        assert_eq!(render("aaa bbb ccc ddd", wrapped(10)), "aaa bbb\nccc ddd");
        // Exact fit does not break.
        assert_eq!(render("aaaa bbbbb", wrapped(10)), "aaaa bbbbb");
    }

    #[test]
    fn wrap_swallows_break_spaces_but_preserves_others() {
        assert_eq!(render("aa  bb", wrapped(20)), "aa  bb");
        // The spaces at a break point are consumed by the newline.
        assert_eq!(render("aaaa   bbbb", wrapped(6)), "aaaa\nbbbb");
    }

    #[test]
    fn bullet_continuation_gets_hanging_indent() {
        assert_eq!(
            render("- aaaa bbbb cccc", wrapped(12)),
            "- aaaa bbbb\n  cccc"
        );
        assert_eq!(
            render("12. aaa bbb ccc", wrapped(12)),
            "12. aaa bbb\n    ccc"
        );
    }

    #[test]
    fn hang_resets_on_source_newline() {
        assert_eq!(
            render("- aaaa bbbb cccc\nplain dddd eeee ffff", wrapped(12)),
            "- aaaa bbbb\n  cccc\nplain dddd\neeee ffff"
        );
    }

    #[test]
    fn oversized_word_streams_without_midword_break() {
        assert_eq!(render("abcdefghij", wrapped(5)), "abcdefghij");
        // A long word after text breaks before, not inside, the word.
        assert_eq!(render("xx abcdefghij yy", wrapped(5)), "xx\nabcdefghij\nyy");
    }

    #[test]
    fn fenced_code_never_wraps_or_styles() {
        let block =
            "```rust\nlet x = very_long_line_that_exceeds_any_width(); // **not bold**\n```\n";
        assert_eq!(render(block, wrapped(10)), block);
        assert_eq!(render(block, styled()), block);
        // Prose resumes wrapping after the closing fence.
        let text = format!("{block}aaa bbb ccc ddd");
        assert_eq!(
            render(&text, wrapped(10)),
            format!("{block}aaa bbb\nccc ddd")
        );
    }

    #[test]
    fn fence_close_requires_matching_run() {
        let block = "````\n``` still inside\n````\nafter\n";
        assert_eq!(render(block, plain()), block);
        let out = render(block, styled());
        assert!(!out.contains('\x1b'), "inside fence stays plain: {out:?}");
    }

    #[test]
    fn indented_lines_pass_verbatim() {
        let text = "    let x = a * b; // no *emphasis*, no wrap aaaaaa bbbbbb\nback";
        let out = render(text, wrapped(10));
        assert!(out.starts_with("    let x = a * b; // no *emphasis*, no wrap aaaaaa bbbbbb\n"));
    }

    #[test]
    fn cjk_widths_count_double() {
        // Each ideograph is width 2: "你好" = 4 cols.
        assert_eq!(render("你好 世界 又见", wrapped(6)), "你好\n世界\n又见");
        assert_eq!(render("你好 世界", wrapped(9)), "你好 世界");
    }

    #[test]
    fn chunked_and_single_feed_agree_when_styled_and_wrapped() {
        let palette = Palette::colored(true).with_wrap(Some(14));
        let inputs = [
            "Some **bold words** wrap across a few lines here\n",
            "# Header line that wraps\n\n- bullet with `code span` inside and more text\n",
            "```\nfenced content stays put\n```\ntrailing prose after the fence block\n",
        ];
        for input in inputs {
            let mut r = MarkdownRenderer::new(palette);
            let mut chunked = String::new();
            for c in input.chars() {
                chunked.push_str(&r.feed(&c.to_string()));
            }
            chunked.push_str(&r.finish());
            assert_eq!(chunked, render_block(input, palette), "{input:?}");
        }
    }

    #[test]
    fn stripping_sgr_recovers_content() {
        // Additive-only: dropping escapes and normalizing whitespace recovers
        // the input exactly.
        let input = "# Hi\n\nSome **bold** and `code` in a paragraph that wraps around\n- item one two three\n";
        let palette = Palette::colored(true).with_wrap(Some(16));
        let out = render_block(input, palette);
        let stripped = strip_sgr(&out);
        assert_eq!(normalize_ws(&stripped), normalize_ws(input));
    }

    #[test]
    fn ends_at_line_start_tracks_emitted_output() {
        let mut r = MarkdownRenderer::new(plain());
        assert!(r.ends_at_line_start());
        r.feed("abc");
        assert!(!r.ends_at_line_start());
        r.feed("\n");
        assert!(r.ends_at_line_start());
        // Pending (unemitted) word does not count as emitted output.
        let mut r = MarkdownRenderer::new(wrapped(10));
        r.feed("line\npend");
        assert!(r.ends_at_line_start());
        r.finish();
        assert!(!r.ends_at_line_start());
    }

    #[test]
    fn event_stream_separates_style_from_content() {
        let mut r = MarkdownRenderer::new(styled());
        let mut events = r.feed_events("a **b** c");
        events.extend(r.finish_events());
        // Content never carries escape bytes; joined Text is the full input
        // (additive-only: delimiters stay visible).
        let mut content = String::new();
        for e in &events {
            if let TuiEvent::Text(text) = e {
                assert!(!text.contains('\x1b'), "escape in Text: {text:?}");
                content.push_str(text);
            }
        }
        assert_eq!(content, "a **b** c");
        // Style truth is semantic: bold on, then reset.
        let styles: Vec<Style> = events
            .iter()
            .filter_map(|e| match e {
                TuiEvent::Style(s) => Some(*s),
                _ => None,
            })
            .collect();
        assert_eq!(
            styles,
            vec![
                Style {
                    bold: true,
                    ..Style::RESET
                },
                Style::RESET
            ]
        );
    }

    #[test]
    fn event_and_string_paths_agree_bytewise() {
        let palette = Palette::colored(true).with_wrap(Some(14));
        let inputs = [
            "Some **bold words** wrap across a few lines here\n",
            "# Header line that wraps\n\n- bullet with `code span` inside and more text\n",
            "```\nfenced content stays put\n```\ntrailing prose after the fence block\n",
        ];
        for input in inputs {
            // Char-chunked event stream, SGR-encoded, equals the String path.
            let mut r = MarkdownRenderer::new(palette);
            let mut events = Vec::new();
            for c in input.chars() {
                events.extend(r.feed_events(&c.to_string()));
            }
            events.extend(r.finish_events());
            assert_eq!(
                encode_ansi(&events, true),
                render_block(input, palette),
                "{input:?}"
            );
        }
    }

    fn strip_sgr(s: &str) -> String {
        let mut out = String::new();
        let mut chars = s.chars().peekable();
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

    fn normalize_ws(s: &str) -> String {
        s.split_whitespace().collect::<Vec<_>>().join(" ")
    }
}
