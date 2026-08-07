//! Rendering the vt100 screen model to styled cell runs with the xterm-256
//! palette resolved to concrete colors and the cursor as its own run. Ported
//! from v2, minus the embedded plain-text copy — plain text is the `text`
//! verb's job now, so no representation carries another one inside it.

/// One run of same-styled characters on one row. Rows are addressed, so
/// renderers keep alignment without trailing blanks.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub(crate) struct ScreenRun {
    pub row: u16,
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fg: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bg: Option<String>,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub inverse: bool,
    pub cursor: bool,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub(crate) struct Screen {
    pub cols: u16,
    pub rows: u16,
    pub cursor_row: u16,
    pub cursor_col: u16,
    pub cursor_hidden: bool,
    /// DECCKM: arrow keys should send application sequences.
    pub application_cursor: bool,
    pub runs: Vec<ScreenRun>,
}

/// Runs merge when the styles match and neither is the cursor's own run.
fn same_style(a: &ScreenRun, b: &ScreenRun) -> bool {
    a.row == b.row
        && a.fg == b.fg
        && a.bg == b.bg
        && a.bold == b.bold
        && a.italic == b.italic
        && a.underline == b.underline
        && a.inverse == b.inverse
        && !a.cursor
        && !b.cursor
}

/// A trailing run a renderer needs nothing from.
fn unstyled_blank(r: &ScreenRun) -> bool {
    r.fg.is_none()
        && r.bg.is_none()
        && !r.bold
        && !r.italic
        && !r.underline
        && !r.inverse
        && !r.cursor
        && r.text.trim().is_empty()
}

/// The xterm-256 palette, resolved to `#rrggbb`; `Default` stays `None` so
/// the client's own theme colors apply.
fn color_hex(color: vt100::Color) -> Option<String> {
    let (r, g, b) = match color {
        vt100::Color::Default => return None,
        vt100::Color::Rgb(r, g, b) => (r, g, b),
        vt100::Color::Idx(i) => match i {
            // The 16 base colors, xterm defaults.
            0 => (0, 0, 0),
            1 => (0xcd, 0, 0),
            2 => (0, 0xcd, 0),
            3 => (0xcd, 0xcd, 0),
            4 => (0, 0, 0xee),
            5 => (0xcd, 0, 0xcd),
            6 => (0, 0xcd, 0xcd),
            7 => (0xe5, 0xe5, 0xe5),
            8 => (0x7f, 0x7f, 0x7f),
            9 => (0xff, 0, 0),
            10 => (0, 0xff, 0),
            11 => (0xff, 0xff, 0),
            12 => (0x5c, 0x5c, 0xff),
            13 => (0xff, 0, 0xff),
            14 => (0, 0xff, 0xff),
            15 => (0xff, 0xff, 0xff),
            // 6×6×6 color cube.
            16..=231 => {
                let n = i - 16;
                let level = |v: u8| if v == 0 { 0 } else { 55 + 40 * v };
                (level(n / 36), level((n / 6) % 6), level(n % 6))
            }
            // Grayscale ramp.
            232..=255 => {
                let v = 8 + 10 * (i - 232);
                (v, v, v)
            }
        },
    };
    Some(format!("#{r:02x}{g:02x}{b:02x}"))
}

pub(crate) fn render(screen: &vt100::Screen) -> Screen {
    let (rows, cols) = screen.size();
    let (cursor_row, cursor_col) = screen.cursor_position();
    let cursor_hidden = screen.hide_cursor();

    let mut runs: Vec<ScreenRun> = Vec::new();
    for row in 0..rows {
        let mut row_runs: Vec<ScreenRun> = Vec::new();
        for col in 0..cols {
            let Some(cell) = screen.cell(row, col) else {
                continue;
            };
            // A wide character's second column carries no glyph of its own.
            if cell.is_wide_continuation() {
                continue;
            }
            let contents = cell.contents();
            let run = ScreenRun {
                row,
                text: if contents.is_empty() {
                    " ".into()
                } else {
                    contents
                },
                fg: color_hex(cell.fgcolor()),
                bg: color_hex(cell.bgcolor()),
                bold: cell.bold(),
                italic: cell.italic(),
                underline: cell.underline(),
                inverse: cell.inverse(),
                cursor: !cursor_hidden && row == cursor_row && col == cursor_col,
            };
            match row_runs.last_mut() {
                Some(last) if same_style(last, &run) => last.text.push_str(&run.text),
                _ => row_runs.push(run),
            }
        }
        // Trailing unstyled blanks carry nothing a renderer needs.
        while row_runs.last().is_some_and(unstyled_blank) {
            row_runs.pop();
        }
        runs.append(&mut row_runs);
    }

    Screen {
        cols,
        rows,
        cursor_row,
        cursor_col,
        cursor_hidden,
        application_cursor: screen.application_cursor(),
        runs,
    }
}
