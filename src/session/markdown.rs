//! Streaming Markdown renderer with fence-aware word wrap.
//!
//! Feed arbitrary UTF-8 chunks ([`MarkdownRenderer::feed`]); deltas may split
//! anywhere, including inside a delimiter run or a `[text](url)` link. Two
//! invariants:
//!
//! - **Disabled = identity**: styling off ⇒ output is byte-identical to input
//!   (the non-TTY / [`Palette::plain`] guarantee). No delimiter is dropped, so
//!   piped stdout, session files, and the user's echoed input stay verbatim.
//! - **Styled = presentation**: with styling on, the *formatting* delimiters
//!   are consumed into presentation rather than printed — `*` / `` ` ``
//!   emphasis runs turn into SGR, a `[text](url)` link becomes an OSC 8
//!   hyperlink over its visible text, and a bare `http(s)://…` URL becomes one
//!   over itself — while every *content* byte still reaches the output in
//!   order. ATX headers are the exception: their `#` markers stay
//!   visible (the line just styles bold). A stray delimiter can mis-style a
//!   span, never corrupt content.
//!
//! Internally the renderer is event-first: it produces a [`TuiEvent`] stream in
//! which content ([`TuiEvent::Text`], escape-free, wrap applied) and
//! presentation state ([`TuiEvent::Style`] / [`TuiEvent::Link`], semantic) are
//! separate variants ([`MarkdownRenderer::feed_events`] /
//! [`MarkdownRenderer::finish_events`]). The String API
//! ([`MarkdownRenderer::feed`] / [`MarkdownRenderer::finish`]) is a facade that
//! encodes that stream ([`crate::tui::encode_ansi`]), gated by the palette's
//! `enabled` flag.
//!
//! Supported: `**` / `*` emphasis toggles (with a light flanking check),
//! `` ` `` inline code, `[text](url)` links and bare `http(s)://` URLs (both
//! OSC 8 when styled), ATX headers, fenced code blocks (never styled or
//! wrapped), indented (4-space) lines verbatim, list hanging indent, and —
//! **styled only** — GFM pipe tables.
//!
//! Tables are the one construct that needs the whole block before *any* of it
//! can be emitted (column widths depend on the widest cell), which append-only
//! output can't revise after the fact. So a leading-`|` line opens a capture
//! that buffers rows until the block's terminator, then draws a box-drawing
//! table with display-width-aligned columns. It stays a hold-back stream: the
//! header row is buffered until the next line confirms a delimiter row
//! (`| --- | :-: |`); a candidate that never confirms replays verbatim as
//! prose. A table that fits the wrap width renders at its natural width; one
//! too wide reflows — columns are sized by max-min fair share and cell contents
//! wrap into taller rows so it stays inside the terminal (only an unbreakable
//! over-long word can still overflow). Because capture is gated on styling,
//! **plain mode passes tables through byte-identically**, keeping the identity
//! guarantee for files and pipes.
//!
//! Out of scope — constructs that need non-linear layout or lookaside beyond
//! the above: setext headers, reference links, `<…>` angle-bracket autolinks,
//! images.

use unicode_width::UnicodeWidthChar;

use super::transcript::Palette;
use crate::tui::{Color, Style, TuiEvent, encode_ansi};

/// One rendered physical line of a table cell: its presentation events and
/// visible display width.
type CellLine = (Vec<TuiEvent>, usize);
/// A table cell rendered into physical lines (a single line when unwrapped).
type CellLines = Vec<CellLine>;

/// What the current physical line is, decided from its first characters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Line {
    /// Buffering the first characters to classify the line.
    Prefix,
    /// Ordinary inline text: styles + wrap apply.
    Body,
    /// Verbatim until end of line (indented code, tab-led lines).
    Raw,
    /// A leading-`|` line being captured into a pending [`TableCapture`].
    Table,
}

/// Column alignment from a GFM delimiter cell (`:--`, `--:`, `:-:`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Align {
    None,
    Left,
    Center,
    Right,
}

/// A leading-`|` block captured while it may still be a GFM pipe table. Holds
/// the raw physical lines (no trailing newline); `confirmed` flips once line
/// index 1 is a valid delimiter row, at which point the block will render as a
/// box rather than replay as prose.
#[derive(Debug)]
struct TableCapture {
    lines: Vec<String>,
    confirmed: bool,
    /// Whether the last captured line ended with a newline (false only for a
    /// final partial line at stream end), so replay reproduces the input.
    terminated: bool,
}

/// A zero-width presentation mark queued inside the pending word so it rides
/// the wrap decision (the event-form of splicing an escape into the buffer).
#[derive(Debug)]
enum Mark {
    Style(Style),
    /// OSC 8 hyperlink open (`Some(url)`) / close (`None`).
    Link(Option<String>),
}

/// Buffering a possible `[text](url)` link (styling on only). Survives chunk
/// boundaries; if the syntax doesn't complete it aborts back to literal text.
#[derive(Debug)]
struct LinkParse {
    stage: LinkStage,
    text: String,
    url: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LinkStage {
    /// Between `[` and `]` — the visible text.
    Text,
    /// Saw `]`; a following `(` opens the URL, anything else is literal `[text]`.
    CloseText,
    /// Between `(` and `)` — the URL.
    Url,
}

impl LinkParse {
    fn new() -> Self {
        Self {
            stage: LinkStage::Text,
            text: String::new(),
            url: String::new(),
        }
    }
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
    /// A pending GFM pipe-table capture (styled only). `None` when not in or
    /// awaiting a table block.
    table: Option<TableCapture>,
    /// The current physical line's raw bytes while in [`Line::Table`].
    table_line: String,
    /// True while replaying a rejected capture as prose, so those leading-`|`
    /// lines don't re-trigger table detection.
    replaying_table: bool,
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
    /// Buffering a `[text](url)` link across chunk boundaries (styled only).
    link: Option<LinkParse>,

    col: usize,
    word: String,
    /// Presentation changes (style toggles, link open/close) queued at byte
    /// offsets inside the pending `word`, so they ride the wrap decision with
    /// it and never dangle across a break.
    word_marks: Vec<(usize, Mark)>,
    word_width: usize,
    spaces: usize,
    hang: usize,
    /// Word already reached the wrap width; stream it raw until a break.
    overflow: bool,
    /// Widest word emitted so far, hanging indent included — the narrowest
    /// wrap width this content could rewrap to without a mid-word overflow.
    /// Meaningful only when `wrap` is set (the no-wrap path flushes words in
    /// fragments); read by table cell measurement ([`render_cell`]).
    max_word_width: usize,
    /// The pending word contains a bare URL scheme at a valid boundary.
    /// Suppresses wrap overflow (and per-char flushing in no-wrap mode) so the
    /// whole URL stays buffered — a word streamed raw can't be bracketed in the
    /// link marks [`Self::autolink_word`] splices in.
    word_is_url: bool,
    /// The content char just before the pending `word` (`None` at a line
    /// start): the left context for the scheme-boundary check, since in no-wrap
    /// mode the char before a URL was already flushed out of `word`.
    word_start_prev: Option<char>,
    /// Suppress bare-URL detection while replaying a `[text](url)` link's
    /// visible text, so a URL used *as* link text doesn't nest a second link.
    suppress_autolink: bool,
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
            table: None,
            table_line: String::new(),
            replaying_table: false,
            fence: None,
            fence_line: None,
            line_has_content: false,
            prev_char: None,
            run: None,
            link: None,
            col: 0,
            word: String::new(),
            word_marks: Vec::new(),
            word_width: 0,
            spaces: 0,
            max_word_width: 0,
            hang: 0,
            overflow: false,
            word_is_url: false,
            word_start_prev: None,
            suppress_autolink: false,
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
        // Complete an in-progress table line (unterminated at stream end), then
        // resolve any pending capture (draw the box, or replay as prose).
        if self.line == Line::Table {
            let partial = std::mem::take(&mut self.table_line);
            self.push_table_line(partial, false);
            self.line = Line::Body;
        }
        self.flush_pending_table();

        match self.line {
            Line::Prefix if self.fence.is_none() => self.end_prefix(None),
            _ => {
                if let Some(buf) = self.fence_line.take() {
                    self.out_str(&buf);
                }
            }
        }
        if self.link.is_some() {
            self.abort_link();
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
                Line::Table => self.table_char(c),
            }
        }
        // Without wrap there is no need to hold words back; only delimiter
        // runs — and a bare URL still forming, which must stay whole to be
        // linked — may buffer across chunks.
        if self.wrap.is_none()
            && self.run.is_none()
            && self.line == Line::Body
            && !self.pending_url_word()
        {
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

        // GFM pipe table (styled only): a line whose first non-space char is
        // `|` opens or extends a capture. Suppressed during replay so a
        // rejected candidate can't loop. Plain mode skips this entirely, so
        // pipe rows stream as ordinary text (byte-identity preserved).
        if self.styled && !self.replaying_table && c == '|' && stripped.is_empty() {
            self.line = Line::Table;
            self.table_line.clear();
            self.table_line.push_str(&prefix);
            self.table_line.push('|');
            return;
        }
        // A non-pipe line ends any pending capture before it is classified.
        self.flush_pending_table();

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
        // A blank / marker-only line terminates a pending table capture.
        self.flush_pending_table();
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

    // -- tables -------------------------------------------------------------

    /// Accumulate a table-row line; a newline closes it and hands it to the
    /// block machine.
    fn table_char(&mut self, c: char) {
        if c == '\n' {
            let line = std::mem::take(&mut self.table_line);
            self.push_table_line(line, true);
        } else {
            self.table_line.push(c);
        }
    }

    /// A complete physical line (or an unterminated final line at stream end)
    /// joined the pending capture. The second line decides the block's fate:
    /// a valid delimiter row confirms a table, anything else replays as prose.
    fn push_table_line(&mut self, line: String, terminated: bool) {
        let check = match &mut self.table {
            None => {
                self.table = Some(TableCapture {
                    lines: vec![line],
                    confirmed: false,
                    terminated,
                });
                false
            }
            Some(t) => {
                t.lines.push(line);
                t.terminated = terminated;
                !t.confirmed && t.lines.len() >= 2
            }
        };
        if check {
            let t = self.table.as_mut().unwrap();
            let ncols = split_cells(&t.lines[0]).len();
            if is_delimiter_row(&t.lines[1], ncols) {
                t.confirmed = true;
            } else {
                // Not a table after all — replay the buffered lines verbatim.
                self.flush_pending_table();
                return;
            }
        }
        if terminated {
            self.reset_line();
        }
    }

    /// Resolve the pending capture: draw a confirmed table as a box; otherwise
    /// replay the raw lines as prose. No-op when nothing is pending.
    fn flush_pending_table(&mut self) {
        let Some(t) = self.table.take() else {
            return;
        };
        if t.confirmed {
            self.render_table(&t.lines);
        } else {
            self.replay_as_prose(&t.lines, t.terminated);
        }
    }

    /// Feed the buffered lines back through the line machine as ordinary text
    /// (table detection suppressed), reproducing the input newline for newline.
    fn replay_as_prose(&mut self, lines: &[String], terminated: bool) {
        self.replaying_table = true;
        let n = lines.len();
        for (i, line) in lines.iter().enumerate() {
            for c in line.chars() {
                self.push_char(c);
            }
            if i + 1 < n || terminated {
                self.push_char('\n');
            }
        }
        self.replaying_table = false;
    }

    /// Draw a confirmed table as a box. Columns are always sized by the max-min
    /// fair-share allocator ([`allocate_widths`]): with no wrap width, or when
    /// the table already fits, every column gets its natural width and each row
    /// is one line; when the table is too wide for the wrap column, the wide
    /// columns are squeezed and their cells wrap into taller rows. Only an
    /// unbreakable over-long word can still push the box past the wrap width.
    /// `lines` is header, delimiter, then rows.
    fn render_table(&mut self, lines: &[String]) {
        fn cell_text(raw: &[String], i: usize) -> &str {
            raw.get(i).map_or("", String::as_str)
        }
        let header = split_cells(&lines[0]);
        let ncols = header.len();
        let delim = split_cells(&lines[1]);
        let aligns: Vec<Align> = (0..ncols)
            .map(|i| {
                delim
                    .get(i)
                    .map(|c| cell_alignment(c))
                    .unwrap_or(Align::None)
            })
            .collect();
        let body: Vec<Vec<String>> = lines[2..].iter().map(|l| split_cells(l)).collect();

        // Render every cell once at unbounded width: a single line whose width
        // is the cell's natural width, plus the wrap machinery's own floor (the
        // widest unbreakable word). The line is reused verbatim below for any
        // cell that ends up not needing to wrap — the common case.
        let measure_row = |raw: &[String]| -> Vec<(CellLines, usize)> {
            (0..ncols)
                .map(|i| render_cell(cell_text(raw, i), usize::MAX))
                .collect()
        };
        let measured_header = measure_row(&header);
        let measured_body: Vec<_> = body.iter().map(|r| measure_row(r)).collect();
        let mut maxs = vec![0usize; ncols];
        let mut mins = vec![0usize; ncols];
        for row in std::iter::once(&measured_header).chain(measured_body.iter()) {
            for (i, (lines, floor)) in row.iter().enumerate() {
                maxs[i] = maxs[i].max(lines[0].1);
                mins[i] = mins[i].max(*floor);
            }
        }

        // Fair-share allocation to the available width, floored at 1 so a
        // column is never empty. With no wrap the budget is unbounded, so the
        // allocator returns the natural widths (no squeeze).
        let chrome = 3 * ncols + 1; // `ncols+1` verticals + one pad space each side
        let budget = self.wrap.map_or(usize::MAX, |w| w.saturating_sub(chrome));
        let widths: Vec<usize> = allocate_widths(&mins, &maxs, budget)
            .into_iter()
            .map(|w| w.max(1))
            .collect();

        // Re-render only the squeezed cells, wrapped to their columns; every
        // other cell keeps its measured single line. The allocator never goes
        // below a column's floor, and the floor comes from the wrap machinery
        // itself, so no rendered line can exceed its column.
        let wrap_row = |measured: Vec<(CellLines, usize)>, raw: &[String]| -> Vec<CellLines> {
            measured
                .into_iter()
                .enumerate()
                .map(|(i, (lines, _))| {
                    let lines = if lines[0].1 <= widths[i] {
                        lines
                    } else {
                        render_cell(cell_text(raw, i), widths[i]).0
                    };
                    debug_assert!(
                        lines.iter().all(|(_, w)| *w <= widths[i]),
                        "wrapped cell line exceeds its column"
                    );
                    lines
                })
                .collect()
        };
        let header_lines = wrap_row(measured_header, &header);
        let body_lines: Vec<Vec<CellLines>> = measured_body
            .into_iter()
            .zip(body.iter())
            .map(|(m, r)| wrap_row(m, r))
            .collect();

        self.frame_open();
        self.table_border(&widths, '┌', '┬', '┐');
        self.table_multiline_row(header_lines, &widths, &aligns);
        if !body_lines.is_empty() {
            self.table_border(&widths, '├', '┼', '┤');
            for row in body_lines {
                self.table_multiline_row(row, &widths, &aligns);
            }
        }
        self.table_border(&widths, '└', '┴', '┘');
        self.frame_close();
    }

    /// A horizontal border line: `left`/`mid`/`right` corner-or-junction runs
    /// over each column (width + one pad space each side).
    fn table_border(&mut self, widths: &[usize], left: char, mid: char, right: char) {
        self.out_ch(left);
        for (i, w) in widths.iter().enumerate() {
            for _ in 0..(w + 2) {
                self.out_ch('─');
            }
            self.out_ch(if i + 1 < widths.len() { mid } else { right });
        }
        self.out_ch('\n');
    }

    /// A logical row rendered as per-column lines: the row is as tall as its
    /// tallest cell (shorter cells pad with blank lines, top-aligned), and each
    /// physical line is `│`-separated cells padded to their column width per
    /// the column's alignment.
    fn table_multiline_row(
        &mut self,
        mut cols: Vec<CellLines>,
        widths: &[usize],
        aligns: &[Align],
    ) {
        let height = cols.iter().map(Vec::len).max().unwrap_or(1);
        for j in 0..height {
            self.out_ch('│');
            for (i, col) in cols.iter_mut().enumerate() {
                let (mut events, w) = match col.get_mut(j) {
                    Some(line) => std::mem::take(line),
                    None => (Vec::new(), 0),
                };
                let pad = widths[i].saturating_sub(w);
                let (left, right) = match aligns.get(i).copied().unwrap_or(Align::None) {
                    Align::Right => (pad, 0),
                    Align::Center => (pad / 2, pad - pad / 2),
                    Align::Left | Align::None => (0, pad),
                };
                self.out_ch(' ');
                for _ in 0..left {
                    self.out_ch(' ');
                }
                self.out.append(&mut events);
                for _ in 0..right {
                    self.out_ch(' ');
                }
                self.out_ch(' ');
                self.out_ch('│');
            }
            self.out_ch('\n');
        }
    }

    /// Return the terminal to `base` before drawing borders, so a style left
    /// open by preceding prose doesn't tint the box.
    fn frame_open(&mut self) {
        if self.last_style != Some(self.base) {
            self.out.push(TuiEvent::Style(self.base));
            self.last_style = Some(self.base);
        }
    }

    /// After the box, re-assert `base`: spliced cell events changed the real
    /// terminal state without updating `last_style`, so emit unconditionally to
    /// resync (a following delta / the finish reset then behaves correctly).
    fn frame_close(&mut self) {
        self.out.push(TuiEvent::Style(self.base));
        self.last_style = Some(self.base);
    }

    // -- inline text --------------------------------------------------------

    fn inline_char(&mut self, c: char) {
        if self.link.is_some() {
            self.link_char(c);
            return;
        }
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
            // A link's `[…](…)` syntax is presentation, folded into an OSC 8
            // hyperlink; with styling off it stays literal text (identity).
            '[' if self.styled && !self.code => self.link = Some(LinkParse::new()),
            _ => self.add_content_char(c),
        }
    }

    // -- links --------------------------------------------------------------

    /// Route a char into the in-progress `[text](url)` parse. Any syntax that
    /// doesn't complete a link aborts back to literal text; a source newline
    /// or a control char in the URL also aborts (links don't span lines).
    fn link_char(&mut self, c: char) {
        let stage = match &self.link {
            Some(link) => link.stage,
            None => return,
        };
        match stage {
            LinkStage::Text => match c {
                ']' => self.link.as_mut().unwrap().stage = LinkStage::CloseText,
                '\n' => {
                    self.abort_link();
                    self.inline_char(c);
                }
                _ => self.link.as_mut().unwrap().text.push(c),
            },
            LinkStage::CloseText => {
                if c == '(' {
                    self.link.as_mut().unwrap().stage = LinkStage::Url;
                } else {
                    // `[text]` not followed by `(`: literal, then reprocess `c`
                    // (it may open the next construct, e.g. another `[`).
                    self.abort_link();
                    self.inline_char(c);
                }
            }
            LinkStage::Url => {
                if c == ')' {
                    let link = self.link.take().unwrap();
                    self.finish_link(link.text, link.url);
                } else if c.is_control() {
                    // A newline or other control byte can't be part of a URL.
                    self.abort_link();
                    self.inline_char(c);
                } else {
                    self.link.as_mut().unwrap().url.push(c);
                }
            }
        }
    }

    /// A complete `[text](url)`: bracket the visible text in link marks so the
    /// text still wraps and the OSC 8 open/close ride the word buffer with it.
    fn finish_link(&mut self, text: String, url: String) {
        self.emit_link(Some(url));
        // The visible text is already inside this link; don't let a URL within
        // it open a second, nested one.
        self.suppress_autolink = true;
        for ch in text.chars() {
            self.literal_char(ch);
        }
        self.suppress_autolink = false;
        self.emit_link(None);
    }

    /// The link syntax didn't complete — replay the bytes consumed so far as
    /// literal text (spaces stay breakable), exactly as identity output would.
    fn abort_link(&mut self) {
        let Some(link) = self.link.take() else {
            return;
        };
        self.literal_char('[');
        for ch in link.text.chars() {
            self.literal_char(ch);
        }
        if matches!(link.stage, LinkStage::CloseText | LinkStage::Url) {
            self.literal_char(']');
        }
        if link.stage == LinkStage::Url {
            self.literal_char('(');
            for ch in link.url.chars() {
                self.literal_char(ch);
            }
        }
    }

    /// Emit one char as plain inline content (no delimiter/link detection):
    /// spaces stay breakable for wrap, everything else joins the word buffer.
    fn literal_char(&mut self, c: char) {
        if c == ' ' {
            self.flush_word();
            self.spaces += 1;
            self.overflow = false;
            self.prev_char = Some(' ');
        } else {
            self.add_content_char(c);
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
        self.word_is_url = false;
    }

    /// A delimiter run ended; `next` is the char after it (`None` at stream
    /// or line end). Delimiters stay visible and render inside the span they
    /// delimit: an opening run styles before its delimiters, a closing run
    /// after — so both ends of `**bold**` carry the same styling, and a
    /// misread is purely cosmetic.
    fn resolve_run(&mut self, next: Option<char>) {
        let Some((rc, n)) = self.run.take() else {
            return;
        };
        // The char in front of the run, for the flanking check below.
        let left_flank = self.prev_char.is_some_and(|c| !c.is_whitespace());
        let (bold, italic, code, opening) = match rc {
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
                    (bold, italic, false, opening)
                } else {
                    (false, false, false, false)
                }
            }
            '`' if n == 1 => (false, false, true, !self.code),
            _ => (false, false, false, false),
        };
        if opening {
            self.toggle_styles(bold, italic, code);
        }
        // A recognized delimiter is consumed into the SGR span it toggles when
        // styling is on; otherwise (and for a run that toggled nothing, e.g.
        // `2 ** 3`) it stays literal so output is byte-identical to input.
        let hide = self.styled && (bold || italic || code);
        if !hide {
            for _ in 0..n {
                self.add_content_char(rc);
            }
        }
        if !opening {
            self.toggle_styles(bold, italic, code);
        }
    }

    fn toggle_styles(&mut self, bold: bool, italic: bool, code: bool) {
        if !(bold || italic || code) {
            return;
        }
        if bold {
            self.bold = !self.bold;
        }
        if italic {
            self.italic = !self.italic;
        }
        if code {
            self.code = !self.code;
        }
        self.emit_style();
    }

    fn add_content_char(&mut self, c: char) {
        if self.word.is_empty() {
            // Left context for the scheme boundary check, captured before the
            // char below overwrites `prev_char`.
            self.word_start_prev = self.prev_char;
        }
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
        // Recognize a bare URL the moment its `://` lands (styling on, outside
        // code and link-text replay). A recognized URL keeps buffering past the
        // wrap width instead of overflowing into raw output — overflow streams
        // chars straight out, where the link marks could no longer wrap them.
        if self.styled
            && !self.code
            && !self.suppress_autolink
            && !self.word_is_url
            && c == '/'
            && self.word.ends_with("://")
            && self.word_starts_url()
        {
            self.word_is_url = true;
        }
        if let Some(width) = self.wrap
            && self.word_width >= width
            && !self.word_is_url
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
        self.autolink_word();
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

    /// Emit the pending word, interleaving queued presentation marks at their
    /// byte offsets (the wrap decision, if any, has already been made).
    fn emit_word_raw(&mut self) {
        // The machinery's own record of its widest word: after a break this
        // word would sit at `hang`, so `hang + width` is the narrowest line it
        // can ever occupy. This is what makes table column floors agree with
        // actual wrapping by construction.
        self.max_word_width = self.max_word_width.max(self.hang + self.word_width);
        let word = std::mem::take(&mut self.word);
        let marks = std::mem::take(&mut self.word_marks);
        let mut pos = 0;
        for (off, mark) in marks {
            if off > pos {
                self.out_str(&word[pos..off]);
                pos = off;
            }
            self.out.push(match mark {
                Mark::Style(style) => TuiEvent::Style(style),
                Mark::Link(target) => TuiEvent::Link(target),
            });
        }
        if pos < word.len() {
            self.out_str(&word[pos..]);
        }
        self.col += self.word_width;
        self.word_width = 0;
        self.word_is_url = false;
    }

    /// Splice OSC 8 link marks around a bare `http(s)://…` URL in the completed
    /// word so it hyperlinks like a `[text](url)` link would — the visible text
    /// is untouched, only zero-width [`Mark::Link`] open/close ride in at the
    /// URL's byte offsets. Styled-only: `word_is_url` is set solely when styling
    /// is on, so plain output keeps its byte-for-byte identity.
    fn autolink_word(&mut self) {
        if !self.word_is_url {
            return;
        }
        let Some((start, end)) = self.find_url() else {
            return;
        };
        let url = self.word[start..end].to_string();
        self.word_marks.push((start, Mark::Link(Some(url))));
        self.word_marks.push((end, Mark::Link(None)));
        // Keep marks in ascending byte order for the offset-splicing in
        // `emit_word_raw`; the sort is stable, so a co-located style mark
        // (emitted earlier) still precedes the link open.
        self.word_marks.sort_by_key(|(off, _)| *off);
    }

    /// Whether `word` carries an `http(s)://` scheme at a valid boundary — the
    /// word start (with a non-alphanumeric `word_start_prev`) or any position
    /// after a non-alphanumeric char. Cheap-triggered only when a `://` lands,
    /// so its per-char scan runs at most once per word.
    fn word_starts_url(&self) -> bool {
        let mut prev = self.word_start_prev;
        for (i, c) in self.word.char_indices() {
            if prev.is_none_or(|p| !p.is_alphanumeric()) && scheme_len_at(&self.word, i).is_some() {
                return true;
            }
            prev = Some(c);
        }
        false
    }

    /// Byte range `[start, end)` of the bare URL in the completed word (scheme
    /// at a boundary, host non-empty), or `None`. The left boundary uses
    /// `word_start_prev` for a scheme at offset 0, so a URL glued to a preceding
    /// letter (`foohttp://…`) is rejected the same in wrap and no-wrap modes.
    fn find_url(&self) -> Option<(usize, usize)> {
        let mut prev = self.word_start_prev;
        for (i, c) in self.word.char_indices() {
            if prev.is_none_or(|p| !p.is_alphanumeric())
                && let Some(len) = scheme_len_at(&self.word, i)
            {
                let end = url_end(&self.word, i);
                // Require at least one host char past the scheme.
                if end > i + len {
                    return Some((i, end));
                }
            }
            prev = Some(c);
        }
        None
    }

    /// No-wrap mode flushes a word per char; a bare URL must instead stay
    /// buffered until whole so [`Self::autolink_word`] can wrap it. True while
    /// the pending word is a URL, or an `http(s)://` scheme still forming at a
    /// valid boundary. Styled-only, and never during code spans or link-text
    /// replay — those never autolink, so they keep their per-char flushing.
    fn pending_url_word(&self) -> bool {
        if !self.styled || self.code || self.suppress_autolink {
            return false;
        }
        if self.word_is_url {
            return true;
        }
        if self.word_start_prev.is_some_and(|p| p.is_alphanumeric()) {
            return false;
        }
        let w: &str = &self.word;
        !w.is_empty() && (HTTP.starts_with(w) || HTTPS.starts_with(w))
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
            self.word_marks.push((self.word.len(), Mark::Style(style)));
        }
    }

    /// Queue a hyperlink open (`Some(url)`) / close (`None`), riding the word
    /// buffer like a style mark so it lands on the correct side of a wrap.
    fn emit_link(&mut self, target: Option<String>) {
        if self.overflow {
            self.out.push(TuiEvent::Link(target));
        } else {
            self.word_marks.push((self.word.len(), Mark::Link(target)));
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

// -- bare URL autolinking ---------------------------------------------------

const HTTP: &str = "http://";
const HTTPS: &str = "https://";

/// Scheme length in bytes if `word[i..]` opens with `http://` / `https://`.
fn scheme_len_at(word: &str, i: usize) -> Option<usize> {
    let rest = &word[i..];
    if rest.starts_with(HTTPS) {
        Some(HTTPS.len())
    } else if rest.starts_with(HTTP) {
        Some(HTTP.len())
    } else {
        None
    }
}

/// End byte offset of the URL that begins at `start`, trimming trailing
/// punctuation more likely to be sentence punctuation than part of the link —
/// `.,;:!?"'<>` unconditionally, and a closing bracket only when unbalanced
/// within the URL (so the inner parens of `…/Foo_(bar)` stay, but a wrapping
/// `(…)` does not).
fn url_end(word: &str, start: usize) -> usize {
    let mut end = word.len();
    loop {
        let seg = &word[start..end];
        let Some(last) = seg.chars().next_back() else {
            break;
        };
        let strip = match last {
            '.' | ',' | ';' | ':' | '!' | '?' | '"' | '\'' | '<' | '>' => true,
            ')' => seg.matches(')').count() > seg.matches('(').count(),
            ']' => seg.matches(']').count() > seg.matches('[').count(),
            '}' => seg.matches('}').count() > seg.matches('{').count(),
            _ => false,
        };
        if strip {
            end -= last.len_utf8();
        } else {
            break;
        }
    }
    end
}

// -- table helpers ----------------------------------------------------------

/// Split a GFM table row into trimmed cell texts. Optional outer pipes are
/// dropped and `\|` is an escaped literal pipe inside a cell.
fn split_cells(row: &str) -> Vec<String> {
    let mut s = row.trim();
    s = s.strip_prefix('|').unwrap_or(s);
    s = s.strip_suffix('|').unwrap_or(s);
    let mut cells = Vec::new();
    let mut cur = String::new();
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' if chars.peek() == Some(&'|') => {
                cur.push('|');
                chars.next();
            }
            '|' => {
                cells.push(cur.trim().to_string());
                cur = String::new();
            }
            _ => cur.push(c),
        }
    }
    cells.push(cur.trim().to_string());
    cells
}

/// A GFM delimiter cell: optional leading/trailing `:` around one-or-more `-`.
fn is_delimiter_cell(cell: &str) -> bool {
    let b = cell.trim().as_bytes();
    let mut i = 0;
    if i < b.len() && b[i] == b':' {
        i += 1;
    }
    let dash_start = i;
    while i < b.len() && b[i] == b'-' {
        i += 1;
    }
    if i == dash_start {
        return false; // needs at least one dash
    }
    if i < b.len() && b[i] == b':' {
        i += 1;
    }
    i == b.len()
}

/// Whether `row` is a delimiter row of exactly `ncols` delimiter cells — the
/// line that turns a candidate header into a confirmed table.
fn is_delimiter_row(row: &str, ncols: usize) -> bool {
    if ncols == 0 {
        return false;
    }
    let cells = split_cells(row);
    cells.len() == ncols && cells.iter().all(|c| is_delimiter_cell(c))
}

/// Column alignment encoded by a delimiter cell's colons.
fn cell_alignment(cell: &str) -> Align {
    let c = cell.trim();
    match (c.starts_with(':'), c.ends_with(':')) {
        (true, true) => Align::Center,
        (false, true) => Align::Right,
        (true, false) => Align::Left,
        (false, false) => Align::None,
    }
}

/// Render one cell through a fresh styled renderer wrapped to `width` (inline
/// markdown works inside cells), returning its physical lines and the wrap
/// machinery's own floor for the cell — its widest unbreakable word, hanging
/// indent included. Measuring (`width = usize::MAX`, always one line) and
/// wrapping share this single driver, so the floor can never disagree with how
/// lines actually break.
fn render_cell(cell: &str, width: usize) -> (CellLines, usize) {
    let mut r = MarkdownRenderer::new(Palette::colored(true).with_wrap(Some(width)));
    let mut events = r.feed_events(cell);
    events.extend(r.finish_events());
    let floor = r.max_word_width;
    (split_cell_lines(events), floor)
}

/// Split a rendered cell's event stream into physical lines as
/// `(events, visible width)`. A style still open at a wrap break is re-asserted
/// at the next line's start, so each line renders correctly in isolation
/// between the table borders.
fn split_cell_lines(events: Vec<TuiEvent>) -> CellLines {
    let mut lines: CellLines = Vec::new();
    let mut cur: Vec<TuiEvent> = Vec::new();
    let mut cur_w = 0;
    let mut active = Style::RESET;
    for ev in events {
        match ev {
            TuiEvent::Style(s) => {
                active = s;
                cur.push(TuiEvent::Style(s));
            }
            TuiEvent::Link(t) => cur.push(TuiEvent::Link(t)),
            TuiEvent::Text(t) => {
                let mut rest = t.as_str();
                while let Some(nl) = rest.find('\n') {
                    let seg = &rest[..nl];
                    if !seg.is_empty() {
                        cur_w += display_width(seg);
                        cur.push(TuiEvent::Text(seg.to_string()));
                    }
                    lines.push((std::mem::take(&mut cur), cur_w));
                    cur_w = 0;
                    if active != Style::RESET {
                        cur.push(TuiEvent::Style(active));
                    }
                    rest = &rest[nl + 1..];
                }
                if !rest.is_empty() {
                    cur_w += display_width(rest);
                    cur.push(TuiEvent::Text(rest.to_string()));
                }
            }
            TuiEvent::Begin(_) => {}
        }
    }
    lines.push((cur, cur_w));
    lines
}

/// Max-min fair-share (water-filling) column allocation for a `budget` of
/// content columns (borders/padding already subtracted). Each column ends up
/// `clamp(t, min_i, max_i)` for a common water level `t`:
///
/// - `Σ max ≤ budget`: everything fits → natural widths, no wrapping.
/// - `Σ min ≥ budget`: even minimums overflow → minimum widths (box overflows).
/// - otherwise: raise `t` until the total hits `budget`; columns below `t` keep
///   their width, wider ones cap at `t` and wrap.
fn allocate_widths(mins: &[usize], maxs: &[usize], budget: usize) -> Vec<usize> {
    let n = maxs.len();
    if maxs.iter().sum::<usize>() <= budget {
        return maxs.to_vec();
    }
    if mins.iter().sum::<usize>() >= budget {
        return mins.to_vec();
    }
    let clamp = |t: usize, i: usize| maxs[i].min(mins[i].max(t));
    let total = |t: usize| (0..n).map(|i| clamp(t, i)).sum::<usize>();
    // Largest water level `t` whose total still fits the budget.
    let (mut lo, mut hi) = (0, maxs.iter().copied().max().unwrap_or(0));
    while lo < hi {
        let mid = lo + (hi - lo).div_ceil(2);
        if total(mid) <= budget {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    let mut w: Vec<usize> = (0..n).map(|i| clamp(lo, i)).collect();
    let mut leftover = budget - w.iter().sum::<usize>();
    // The rounding remainder tops up columns sitting at the water line
    // (min ≤ t < max); there are always enough of them to absorb it.
    for i in 0..n {
        if leftover == 0 {
            break;
        }
        if w[i] < maxs[i] && mins[i] <= lo {
            w[i] += 1;
            leftover -= 1;
        }
    }
    w
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

    /// OSC 8 hyperlink open for `url`; [`OSC_CLOSE`] ends the run.
    fn osc_open(url: &str) -> String {
        format!("\x1b]8;;{url}\x1b\\")
    }
    const OSC_CLOSE: &str = "\x1b]8;;\x1b\\";

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
            "see [the docs](https://example.com/x) and [broken](\n",
            "bare https://example.com/x url, then (http://a.b/c). end\n",
            "| A | B |\n| - | - |\n| 1 | 22 |\n",
            "| not a table\nplain line\n",
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
    fn styled_hides_emphasis_delimiters() {
        // Styling on: the `*` delimiters are consumed into the SGR span; only
        // the emphasized content prints.
        assert_eq!(render("a **b** c", styled()), "a \x1b[0;1mb\x1b[0m c");
        assert_eq!(render("x *it* y", styled()), "x \x1b[0;3mit\x1b[0m y");
        // *** toggles both on and both off.
        assert_eq!(
            render("***both*** end", styled()),
            "\x1b[0;1;3mboth\x1b[0m end"
        );
    }

    #[test]
    fn flanking_keeps_spaced_stars_literal() {
        // `2 ** 3` and `a * b`: opening delimiter followed by a space stays text.
        assert_eq!(render("2 ** 3", styled()), "2 ** 3");
        assert_eq!(render("a * b * c", styled()), "a * b * c");
    }

    #[test]
    fn inline_code_styles_and_shields_emphasis() {
        // The backtick delimiters are consumed; the code content stays cyan.
        assert_eq!(render("see `x`.", styled()), "see \x1b[0;36mx\x1b[0m.");
        // Stars inside a code span are literal (never open emphasis).
        let out = render("`a * b * c`", styled());
        assert!(!out.contains("[0;3m"), "{out:?}");
        assert!(out.contains("a * b * c"), "{out:?}");
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
        // Unclosed `**`: the delimiter is still consumed, the tail styled, and
        // finish emits the reset.
        assert_eq!(render("**a", styled()), "\x1b[0;1ma\x1b[0m");
        // No markdown → no escapes at all.
        assert_eq!(render("hello", styled()), "hello");
    }

    #[test]
    fn styled_link_becomes_osc8_hyperlink() {
        let url = "https://example.com/x";
        assert_eq!(
            render("see [the docs](https://example.com/x) now", styled()),
            format!("see {}the docs{OSC_CLOSE} now", osc_open(url))
        );
        // A link abutting preceding text keeps that text outside the link.
        assert_eq!(
            render("pre[t](u)", styled()),
            format!("pre{}t{OSC_CLOSE}", osc_open("u"))
        );
    }

    #[test]
    fn plain_link_stays_literal() {
        // Styling off ⇒ identity: the whole `[text](url)` prints verbatim.
        let input = "see [the docs](https://example.com/x) now";
        assert_eq!(render(input, plain()), input);
        assert_eq!(render_char_chunks(input, plain()), input);
    }

    #[test]
    fn incomplete_link_falls_back_to_literal() {
        // `]` not followed by `(` → literal, trailing char reprocessed.
        assert_eq!(render("[just text] here", styled()), "[just text] here");
        // No closing paren before end of stream → literal.
        assert_eq!(render("[a](http", styled()), "[a](http");
        // Reference-style `[a][ref]` is not a URL link — passes through.
        assert_eq!(render("[a][ref] x", styled()), "[a][ref] x");
        // A newline inside the URL aborts the link.
        assert_eq!(render("[a](url\nrest", styled()), "[a](url\nrest");
    }

    #[test]
    fn link_split_across_chunks_matches_single_feed() {
        let input = "go [here](https://h.test/p) ok";
        assert_eq!(render(input, styled()), render_char_chunks(input, styled()));
        // And the single-feed form is the OSC 8 hyperlink.
        assert_eq!(
            render(input, styled()),
            format!("go {}here{OSC_CLOSE} ok", osc_open("https://h.test/p"))
        );
    }

    // -- bare URL autolinking -----------------------------------------------

    #[test]
    fn bare_url_becomes_osc8_hyperlink() {
        // The visible text is the URL itself; the OSC 8 target is the same URL.
        let url = "https://example.com/x";
        assert_eq!(
            render("see https://example.com/x now", styled()),
            format!("see {}{url}{OSC_CLOSE} now", osc_open(url))
        );
        // `http://` is detected the same as `https://`.
        let url = "http://a.test/p";
        assert_eq!(
            render("go http://a.test/p", styled()),
            format!("go {}{url}{OSC_CLOSE}", osc_open(url))
        );
    }

    #[test]
    fn bare_url_trailing_punctuation_stays_outside_link() {
        // Sentence punctuation after the URL is visible but not part of the link.
        let url = "https://example.com";
        assert_eq!(
            render("Visit https://example.com.", styled()),
            format!("Visit {}{url}{OSC_CLOSE}.", osc_open(url))
        );
        // A wrapping paren is excluded; balanced inner parens are kept.
        assert_eq!(
            render("(https://example.com)", styled()),
            format!("({}{url}{OSC_CLOSE})", osc_open(url))
        );
        let wiki = "https://en.wikipedia.org/wiki/Foo_(bar)";
        assert_eq!(
            render("see https://en.wikipedia.org/wiki/Foo_(bar).", styled()),
            format!("see {}{wiki}{OSC_CLOSE}.", osc_open(wiki))
        );
    }

    #[test]
    fn bare_url_not_linked_without_a_real_scheme() {
        // No scheme → literal; a scheme glued to a preceding word → literal.
        assert_eq!(
            render("visit example.com today", styled()),
            "visit example.com today"
        );
        assert_eq!(
            render("see www.example.com", styled()),
            "see www.example.com"
        );
        assert_eq!(
            render("nothttp://x.test here", styled()),
            "nothttp://x.test here"
        );
        // Scheme with an empty host is not a link.
        assert_eq!(
            render("bare https:// slash", styled()),
            "bare https:// slash"
        );
    }

    #[test]
    fn bare_url_in_code_span_is_not_linked() {
        // Inside a code span the URL is literal, monospace content.
        let out = render("run `curl https://example.com`", styled());
        assert!(
            !out.contains("\x1b]8;;"),
            "no hyperlink in code span: {out:?}"
        );
        assert!(out.contains("https://example.com"), "{out:?}");
    }

    #[test]
    fn plain_mode_bare_url_stays_literal() {
        // Styling off ⇒ identity: the URL prints verbatim, no OSC 8.
        let input = "see https://example.com/x, then done";
        assert_eq!(render(input, plain()), input);
        assert_eq!(render_char_chunks(input, plain()), input);
    }

    #[test]
    fn bare_url_split_across_chunks_matches_single_feed() {
        let input = "go https://h.test/a/b?c=d ok";
        assert_eq!(render(input, styled()), render_char_chunks(input, styled()));
        let url = "https://h.test/a/b?c=d";
        assert_eq!(
            render(input, styled()),
            format!("go {}{url}{OSC_CLOSE} ok", osc_open(url))
        );
    }

    #[test]
    fn bare_url_event_stream_carries_link_as_presentation() {
        // The live TUI path is the event path: the URL must ride Link events,
        // its visible Text stay escape-free and equal to the source.
        let mut r = MarkdownRenderer::new(styled());
        let mut events = r.feed_events("open https://h.test/p now");
        events.extend(r.finish_events());
        let mut content = String::new();
        for e in &events {
            if let TuiEvent::Text(text) = e {
                assert!(!text.contains('\x1b'), "escape in Text: {text:?}");
                content.push_str(text);
            }
        }
        assert_eq!(content, "open https://h.test/p now");
        let links: Vec<Option<String>> = events
            .iter()
            .filter_map(|e| match e {
                TuiEvent::Link(t) => Some(t.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(links, vec![Some("https://h.test/p".to_string()), None]);
    }

    #[test]
    fn url_as_markdown_link_text_is_not_double_linked() {
        // `[url](url)` must yield a single link whose target is the paren URL,
        // never a second link nested over the visible text.
        let out = render("[https://vis.test](https://tgt.test)", styled());
        assert_eq!(
            out,
            format!(
                "{}https://vis.test{OSC_CLOSE}",
                osc_open("https://tgt.test")
            )
        );
    }

    #[test]
    fn long_bare_url_wraps_whole_and_links() {
        // A URL longer than the wrap width breaks before it (no mid-URL split)
        // and is still linked in full, rather than overflowing raw and unlinked.
        let url = "https://example.com/very/long/path/segment";
        let out = render(
            &format!("see {url}"),
            Palette::colored(true).with_wrap(Some(12)),
        );
        assert_eq!(out, format!("see\n{}{url}{OSC_CLOSE}", osc_open(url)));
    }

    #[test]
    fn bare_url_in_table_cell_is_hyperlinked() {
        // Cells render through the same inline machine, so a bare URL inside
        // one hyperlinks like prose; the box pads by visible width.
        let url = "https://h.test/x";
        let input = "| link |\n| - |\n| https://h.test/x |\n";
        let out = render(input, styled());
        assert!(out.contains(&osc_open(url)), "cell URL linked: {out:?}");
        assert_eq!(
            strip_escapes(&out),
            "┌──────────────────┐\n\
             │ link             │\n\
             ├──────────────────┤\n\
             │ https://h.test/x │\n\
             └──────────────────┘\n"
        );
    }

    #[test]
    fn squeezed_cell_keeps_url_whole_and_linked() {
        // In a squeezed table the URL is the column's floor: it stays on one
        // physical line (never split mid-URL) and stays hyperlinked, while the
        // prose around it wraps into further lines.
        let url = "https://h.test/long/path";
        let input = format!("| k | v |\n| - | - |\n| a | see {url} today |\n");
        let out = render(&input, styled_wrap(34));
        assert!(out.contains(&osc_open(url)), "squeezed URL linked: {out:?}");
        let plain = strip_escapes(&out);
        assert!(
            plain.contains(&format!("│ {url} ")),
            "URL on one cell line: {plain:?}"
        );
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
    fn styled_output_strips_to_visible_text() {
        // Styling on: emphasis/code delimiters are consumed (headers keep `#`),
        // so stripping SGR + normalizing whitespace yields the visible text.
        let input = "# Hi\n\nSome **bold** and `code` in a paragraph that wraps around\n- item one two three\n";
        let visible =
            "# Hi\n\nSome bold and code in a paragraph that wraps around\n- item one two three\n";
        let palette = Palette::colored(true).with_wrap(Some(16));
        let stripped = strip_escapes(&render_block(input, palette));
        assert_eq!(normalize_ws(&stripped), normalize_ws(visible));
        // Plain palette recovers the input byte-for-byte (no delimiter dropped).
        assert_eq!(render_block(input, Palette::plain()), input);
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
        // Content never carries escape bytes; the delimiters are folded into
        // Style events, so joined Text is the visible span only.
        let mut content = String::new();
        for e in &events {
            if let TuiEvent::Text(text) = e {
                assert!(!text.contains('\x1b'), "escape in Text: {text:?}");
                content.push_str(text);
            }
        }
        assert_eq!(content, "a b c");
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
    fn event_stream_carries_link_as_presentation() {
        let mut r = MarkdownRenderer::new(styled());
        let mut events = r.feed_events("go [here](https://h.test) ok");
        events.extend(r.finish_events());
        // Visible content is escape-free and free of the link syntax.
        let mut content = String::new();
        for e in &events {
            if let TuiEvent::Text(text) = e {
                assert!(!text.contains('\x1b'), "escape in Text: {text:?}");
                content.push_str(text);
            }
        }
        assert_eq!(content, "go here ok");
        // The URL rides Link events, not Text: open then close.
        let links: Vec<Option<String>> = events
            .iter()
            .filter_map(|e| match e {
                TuiEvent::Link(t) => Some(t.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(links, vec![Some("https://h.test".to_string()), None]);
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

    // -- tables -------------------------------------------------------------

    #[test]
    fn styled_table_renders_aligned_box() {
        let input = "| A | B |\n| - | - |\n| 1 | 22 |\n";
        let out = strip_escapes(&render(input, styled()));
        assert_eq!(
            out,
            "┌───┬────┐\n\
             │ A │ B  │\n\
             ├───┼────┤\n\
             │ 1 │ 22 │\n\
             └───┴────┘\n"
        );
    }

    #[test]
    fn table_honors_column_alignment() {
        // `:--` left, `--:` right; column width comes from the widest cell.
        let input = "| L | R |\n| :-- | --: |\n| a | b |\n| ccc | ddd |\n";
        let out = strip_escapes(&render(input, styled()));
        assert_eq!(
            out,
            "┌─────┬─────┐\n\
             │ L   │   R │\n\
             ├─────┼─────┤\n\
             │ a   │   b │\n\
             │ ccc │ ddd │\n\
             └─────┴─────┘\n"
        );
    }

    #[test]
    fn table_cells_render_inline_markdown_by_visible_width() {
        // `**b**` / `` `c` `` style inside the cell and pad by visible width (1),
        // not the raw delimiter length.
        let input = "| **b** | `c` |\n| - | - |\n| x | y |\n";
        let out = render(input, styled());
        assert_eq!(
            strip_escapes(&out),
            "┌───┬───┐\n│ b │ c │\n├───┼───┤\n│ x │ y │\n└───┴───┘\n"
        );
        assert!(out.contains("\x1b[0;1mb\x1b[0m"), "bold cell: {out:?}");
        assert!(out.contains("\x1b[0;36mc\x1b[0m"), "code cell: {out:?}");
    }

    #[test]
    fn table_renders_without_trailing_newline() {
        // Confirmed table whose final row is unterminated at stream end.
        let input = "| H |\n| - |\n| x |";
        let out = strip_escapes(&render(input, styled()));
        assert_eq!(out, "┌───┐\n│ H │\n├───┤\n│ x │\n└───┘\n");
    }

    #[test]
    fn table_terminates_at_blank_line_then_prose_resumes() {
        let input = "| H |\n| - |\n| x |\n\nafter\n";
        let out = strip_escapes(&render(input, styled()));
        assert_eq!(out, "┌───┐\n│ H │\n├───┤\n│ x │\n└───┘\n\nafter\n");
    }

    #[test]
    fn header_only_table_omits_body_separator() {
        // A header + delimiter with no body rows draws top/header/bottom only.
        let input = "| H1 | H2 |\n| - | - |\n\ntail\n";
        let out = strip_escapes(&render(input, styled()));
        assert_eq!(out, "┌────┬────┐\n│ H1 │ H2 │\n└────┴────┘\n\ntail\n");
    }

    #[test]
    fn pipe_lines_without_a_delimiter_stay_prose() {
        // Two pipe rows but no delimiter row → not a table; replays verbatim.
        let input = "| a | b |\n| c | d |\n";
        assert_eq!(render(input, styled()), input);
        // A lone leading-pipe line followed by prose, likewise.
        let input = "| just a note\nplain text\n";
        assert_eq!(render(input, styled()), input);
    }

    #[test]
    fn unbreakable_cells_overflow_at_natural_width() {
        // Every cell is a single unbreakable word, so the column floors equal
        // the natural widths and wrap 10 is infeasible: the allocator falls
        // back to minimums and the box overflows at its natural 15 columns.
        let input = "| aaaa | bbbb |\n| - | - |\n| cccc | dddd |\n";
        let out = strip_escapes(&render(input, Palette::colored(true).with_wrap(Some(10))));
        assert_eq!(
            out,
            "┌──────┬──────┐\n\
             │ aaaa │ bbbb │\n\
             ├──────┼──────┤\n\
             │ cccc │ dddd │\n\
             └──────┴──────┘\n"
        );
    }

    #[test]
    fn table_chunked_matches_single_feed() {
        // Detection buffers whole lines regardless of chunk boundaries.
        let inputs = [
            "| H | Val |\n| :- | --: |\n| a | 1 |\n| bb | 22 |\n",
            "text before\n\n| x | y |\n| - | - |\n| 1 | 2 |\n\ntext after\n",
            "| **bold** | plain |\n| - | - |\n| 你好 | z |\n",
        ];
        for input in inputs {
            assert_eq!(
                render(input, styled()),
                render_char_chunks(input, styled()),
                "{input:?}"
            );
        }
    }

    #[test]
    fn cjk_cells_pad_by_display_width() {
        // "你好" is width 4, so its column is 4 wide and ascii cells pad to match.
        let input = "| 你好 | b |\n| - | - |\n| a | y |\n";
        let out = strip_escapes(&render(input, styled()));
        assert_eq!(
            out,
            "┌──────┬───┐\n│ 你好 │ b │\n├──────┼───┤\n│ a    │ y │\n└──────┴───┘\n"
        );
    }

    // -- table cell wrapping (over-wide tables) -----------------------------

    fn styled_wrap(w: usize) -> Palette {
        Palette::colored(true).with_wrap(Some(w))
    }

    #[test]
    fn wide_table_wraps_cells_to_fit() {
        // Natural width (25) exceeds wrap 20, so the wide column wraps and the
        // row grows to two physical lines; the box fits within 20 columns.
        let input = "| id | note |\n| -- | ---- |\n| 1 | alpha beta gamma |\n";
        let out = strip_escapes(&render(input, styled_wrap(20)));
        assert_eq!(
            out,
            "┌────┬─────────────┐\n\
             │ id │ note        │\n\
             ├────┼─────────────┤\n\
             │ 1  │ alpha beta  │\n\
             │    │ gamma       │\n\
             └────┴─────────────┘\n"
        );
    }

    #[test]
    fn wrapped_cells_keep_column_alignment() {
        // Right-aligned column stays right-aligned on every wrapped line.
        let input = "| a | b |\n| :-- | --: |\n| x | one two three four |\n";
        let out = strip_escapes(&render(input, styled_wrap(18)));
        assert_eq!(
            out,
            "┌───┬────────────┐\n\
             │ a │          b │\n\
             ├───┼────────────┤\n\
             │ x │    one two │\n\
             │   │ three four │\n\
             └───┴────────────┘\n"
        );
    }

    #[test]
    fn wrapped_table_is_stream_stable() {
        // Whole-line buffering means char-by-char feeding matches a single feed.
        let input = "| Feature | Description |\n| --- | --- |\n\
                     | Streaming | renders markdown incrementally as tokens arrive |\n\
                     | Tables | buffers the block then draws an aligned wrapped box |\n";
        for w in [30, 44, 60] {
            assert_eq!(
                render(input, styled_wrap(w)),
                render_char_chunks(input, styled_wrap(w)),
                "wrap={w}"
            );
        }
    }

    #[test]
    fn unbreakable_word_overflows_its_column() {
        // A token wider than any feasible column can't be broken, so its column
        // overflows the wrap width (box wider than 30) and the word stays whole.
        let url = "https://example.com/really/long/unbreakable/path";
        let input = format!("| k | v |\n| - | - |\n| link | {url} |\n");
        let out = strip_escapes(&render(&input, styled_wrap(30)));
        assert!(out.contains(url), "token kept whole: {out:?}");
        // The narrow column still wrapped tight rather than padding to the box.
        assert!(out.contains("│ k    │"), "{out:?}");
    }

    #[test]
    fn allocate_widths_cases() {
        // Fits: everyone gets their natural max.
        assert_eq!(allocate_widths(&[1, 1], &[3, 4], 20), vec![3, 4]);
        // Even minimums overflow: fall back to mins (box will overflow).
        assert_eq!(allocate_widths(&[10, 10], &[20, 20], 15), vec![10, 10]);
        // Water-filling: the wide column absorbs the squeeze, the narrow one is
        // untouched (it's below the water line).
        assert_eq!(allocate_widths(&[2, 5], &[2, 16], 13), vec![2, 11]);
        // Equal columns share evenly.
        assert_eq!(
            allocate_widths(&[1, 1, 1], &[10, 10, 10], 15),
            vec![5, 5, 5]
        );
        // The rounding remainder tops up the leading columns at the water line.
        assert_eq!(allocate_widths(&[1, 1], &[10, 10], 15), vec![8, 7]);
    }

    /// Visible text only: drop SGR (`ESC [ … m`) and OSC 8 hyperlink
    /// (`ESC ] … ST`) sequences.
    fn strip_escapes(s: &str) -> String {
        let mut out = String::new();
        let mut chars = s.chars().peekable();
        while let Some(c) = chars.next() {
            if c != '\x1b' {
                out.push(c);
                continue;
            }
            match chars.next() {
                Some('[') => {
                    for e in chars.by_ref() {
                        if e == 'm' {
                            break;
                        }
                    }
                }
                Some(']') => {
                    while let Some(e) = chars.next() {
                        if e == '\x07' || (e == '\x1b' && chars.next_if_eq(&'\\').is_some()) {
                            break;
                        }
                    }
                }
                _ => {}
            }
        }
        out
    }

    fn normalize_ws(s: &str) -> String {
        s.split_whitespace().collect::<Vec<_>>().join(" ")
    }
}
