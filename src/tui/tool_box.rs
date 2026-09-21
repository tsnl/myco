//! Append-only tool groups: inputs are visible before dispatch completes, and
//! concurrent outcomes stay inside the same frame until prose resumes.

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::{Palette, Style, TuiEvent, styled_line};

const PREVIEW_LINES: usize = 5;

#[derive(Debug, Clone, Copy)]
pub(super) struct ToolBox {
    width: usize,
    remaining: Option<usize>,
    truncated: bool,
}

impl ToolBox {
    pub fn open(events: &mut Vec<TuiEvent>, name: &str, palette: Palette) -> Self {
        let frame = Self {
            width: palette
                .wrap
                .unwrap_or(super::transcript::DEFAULT_RULE_WIDTH)
                .max(8),
            remaining: (!palette.verbose).then_some(PREVIEW_LINES),
            truncated: false,
        };
        frame.heading(events, name, '╭', '╮');
        frame
    }

    pub fn next(&self, events: &mut Vec<TuiEvent>, name: &str) {
        self.heading(events, name, '├', '┤');
    }

    fn heading(&self, events: &mut Vec<TuiEvent>, name: &str, left: char, right: char) {
        let mut title = String::from("─ ");
        for ch in name.escape_debug() {
            if title.width() + unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0)
                > self.width - 4
            {
                break;
            }
            title.push(ch);
        }
        title.push(' ');
        styled_line(
            events,
            Style::WARNING,
            &format!(
                "{left}{title}{}{right}",
                "─".repeat(self.width - title.width() - 2)
            ),
        );
    }

    pub fn input(&mut self, events: &mut Vec<TuiEvent>, input: &serde_json::Value) {
        self.content(
            events,
            &serde_json::to_string_pretty(input).unwrap(),
            Style::THINKING,
            true,
        );
    }

    pub fn text(&mut self, events: &mut Vec<TuiEvent>, text: &str, style: Style) {
        self.content(events, text, style, false);
    }

    fn content(&mut self, events: &mut Vec<TuiEvent>, text: &str, style: Style, json_keys: bool) {
        if self.truncated {
            return;
        }
        for source in text.strip_suffix('\n').unwrap_or(text).split('\n') {
            let key_end = json_keys.then(|| json_key_end(source)).flatten();
            let mut consumed = 0;
            for (index, line) in tool_text(source, self.width - 4).lines().enumerate() {
                if self.remaining == Some(0) {
                    self.status(events, "… /verbose", Style::THINKING);
                    self.truncated = true;
                    return;
                }
                let prefix = if index == 0 { 0 } else { "↪ ".len() };
                let length = line.len() - prefix;
                let key = key_end.and_then(|end| {
                    let count = end.saturating_sub(consumed).min(length);
                    (count > 0).then_some((prefix, prefix + count))
                });
                self.line(events, line, style, key);
                consumed += length;
                if let Some(remaining) = &mut self.remaining {
                    *remaining -= 1;
                }
            }
        }
    }

    /// Factual outcomes remain visible even when the content preview is exhausted.
    pub fn status(&self, events: &mut Vec<TuiEvent>, text: &str, style: Style) {
        for line in tool_text(text, self.width - 4).lines() {
            self.line(events, line, style, None);
        }
    }

    fn line(
        &self,
        events: &mut Vec<TuiEvent>,
        line: &str,
        style: Style,
        key: Option<(usize, usize)>,
    ) {
        events.push(TuiEvent::Style(Style::WARNING));
        events.push(TuiEvent::Text("│ ".into()));
        events.push(TuiEvent::Style(style));
        if let Some((start, end)) = key {
            events.push(TuiEvent::Text(line[..start].into()));
            events.push(TuiEvent::Style(Style::USER));
            events.push(TuiEvent::Text(line[start..end].into()));
            events.push(TuiEvent::Style(style));
            events.push(TuiEvent::Text(line[end..].into()));
        } else {
            events.push(TuiEvent::Text(line.to_string()));
        }
        events.push(TuiEvent::Style(Style::WARNING));
        events.push(TuiEvent::Text(format!(
            "{} │",
            " ".repeat((self.width - 4).saturating_sub(line.width()))
        )));
        events.push(TuiEvent::Style(Style::RESET));
        events.push(TuiEvent::Text("\n".into()));
    }

    pub fn close(self, events: &mut Vec<TuiEvent>) {
        styled_line(
            events,
            Style::WARNING,
            &format!("╰{}╯", "─".repeat(self.width - 2)),
        );
    }
}

fn json_key_end(line: &str) -> Option<usize> {
    line.match_indices("\":").find_map(|(index, _)| {
        serde_json::from_str::<String>(&line[..index + 1])
            .ok()
            .map(|_| index + 2)
    })
}

/// Wrap without dropping whitespace. The arrow marks
/// display continuations, so they cannot be mistaken for source line breaks.
fn tool_text(text: &str, width: usize) -> String {
    let mut visible = String::new();
    for ch in text.chars() {
        if ch.is_control() && ch != '\n' {
            visible.extend(ch.escape_default());
        } else {
            visible.push(ch);
        }
    }
    let mut out = String::new();
    for line in visible.strip_suffix('\n').unwrap_or(&visible).split('\n') {
        let mut prefix = "";
        let mut remaining = line;
        loop {
            let mut end = 0;
            let mut boundary = 0;
            let mut column = prefix.chars().count();
            for (offset, ch) in remaining.char_indices() {
                let char_width = ch.width().unwrap_or(0);
                if column + char_width > width && end > 0 {
                    break;
                }
                column += char_width;
                end = offset + ch.len_utf8();
                if ch == ' ' || ch == '\t' {
                    boundary = end;
                }
            }
            if end < remaining.len() && boundary > 0 {
                end = boundary;
            }
            out.push_str(prefix);
            out.push_str(&remaining[..end]);
            out.push('\n');
            remaining = &remaining[end..];
            if remaining.is_empty() {
                break;
            }
            prefix = "↪ ";
        }
    }
    out
}
