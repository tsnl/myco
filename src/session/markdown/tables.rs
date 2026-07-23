//! GFM pipe-table layout for [`MarkdownRenderer`]: block capture and
//! confirmation, cell rendering and measurement, max-min fair-share
//! column allocation, and box drawing. Styled-only — plain mode never
//! captures, preserving the byte-identity guarantee.

use super::*;

/// One rendered physical line of a table cell: its presentation events and
/// visible display width.
type CellLine = (Vec<TuiEvent>, usize);
/// A table cell rendered into physical lines (a single line when unwrapped).
type CellLines = Vec<CellLine>;

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
pub(super) struct TableCapture {
    lines: Vec<String>,
    confirmed: bool,
    /// Whether the last captured line ended with a newline (false only for a
    /// final partial line at stream end), so replay reproduces the input.
    terminated: bool,
}

impl MarkdownRenderer {
    /// Accumulate a table-row line; a newline closes it and hands it to the
    /// block machine.
    pub(super) fn table_char(&mut self, c: char) {
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
    pub(super) fn push_table_line(&mut self, line: String, terminated: bool) {
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
    pub(super) fn flush_pending_table(&mut self) {
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
    /// columns are squeezed and their cells wrap into taller rows. Body rows
    /// are separated by horizontal rules, keeping multiline rows
    /// distinguishable. Only an unbreakable over-long word can still push the
    /// box past the wrap width. `lines` is header, delimiter, then rows.
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
            for (i, row) in body_lines.into_iter().enumerate() {
                if i > 0 {
                    self.table_border(&widths, '├', '┼', '┤');
                }
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
}

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

#[cfg(test)]
mod tests {
    use super::super::tests::{render, render_char_chunks, strip_escapes, styled, styled_wrap};
    use super::*;

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
             ├─────┼─────┤\n\
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
    fn wrapped_table_separates_every_body_row() {
        // Per-row rules keep a multiline (wrapped) row from blending into the
        // single-line row after it.
        let input = "| id | note |\n| -- | ---- |\n\
                     | 1 | alpha beta gamma |\n| 2 | ok |\n";
        let out = strip_escapes(&render(input, styled_wrap(20)));
        assert_eq!(
            out,
            "┌────┬─────────────┐\n\
             │ id │ note        │\n\
             ├────┼─────────────┤\n\
             │ 1  │ alpha beta  │\n\
             │    │ gamma       │\n\
             ├────┼─────────────┤\n\
             │ 2  │ ok          │\n\
             └────┴─────────────┘\n"
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
}
