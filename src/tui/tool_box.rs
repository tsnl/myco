//! Append-only tool groups: inputs are visible before dispatch completes, and
//! concurrent outcomes stay inside the same frame until prose resumes.

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::{Style, TOOL_DISPLAY_STRING_MAX, TuiEvent, styled_line, truncate_json_strings};

#[derive(Debug, Clone, Copy)]
pub(super) struct ToolBox {
    width: usize,
}

impl ToolBox {
    pub fn open(events: &mut Vec<TuiEvent>, name: &str, wrap: Option<usize>) -> Self {
        let frame = Self {
            width: wrap.unwrap_or(super::transcript::DEFAULT_RULE_WIDTH).max(8),
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

    pub fn input(&self, events: &mut Vec<TuiEvent>, name: &str, input: &serde_json::Value) {
        let command = (name == "bash")
            .then(|| input.get("command")?.as_str())
            .flatten();
        let stdin = (name == "bash")
            .then(|| input.get("stdin")?.as_str())
            .flatten();
        let mut display = truncate_json_strings(input, TOOL_DISPLAY_STRING_MAX);
        if command.is_some() {
            display.as_object_mut().unwrap().remove("command");
        }
        if stdin.is_some() {
            display.as_object_mut().unwrap().remove("stdin");
        }
        if !display.as_object().is_some_and(|fields| fields.is_empty()) {
            self.text(
                events,
                &serde_json::to_string_pretty(&display).unwrap(),
                "",
                Style::THINKING,
            );
        }
        if let Some(command) = command {
            self.text(events, command, "$ ", Style::USER);
        }
        if let Some(stdin) = stdin {
            self.text(events, "stdin:", "", Style::THINKING);
            self.text(events, stdin, "> ", Style::USER);
        }
    }

    pub fn text(&self, events: &mut Vec<TuiEvent>, text: &str, prefix: &str, style: Style) {
        let body = tool_text(text, prefix, self.width - 4);
        for line in body.lines() {
            events.push(TuiEvent::Style(Style::WARNING));
            events.push(TuiEvent::Text("│ ".into()));
            events.push(TuiEvent::Style(style));
            events.push(TuiEvent::Text(line.to_string()));
            events.push(TuiEvent::Style(Style::WARNING));
            events.push(TuiEvent::Text(format!(
                "{} │",
                " ".repeat((self.width - 4).saturating_sub(line.width()))
            )));
            events.push(TuiEvent::Style(Style::RESET));
            events.push(TuiEvent::Text("\n".into()));
        }
    }

    pub fn close(self, events: &mut Vec<TuiEvent>) {
        styled_line(
            events,
            Style::WARNING,
            &format!("╰{}╯", "─".repeat(self.width - 2)),
        );
    }
}

/// Wrap without parsing shell syntax or dropping whitespace. The arrow marks
/// display continuations, so they cannot be mistaken for source line breaks.
fn tool_text(text: &str, prefix: &str, width: usize) -> String {
    let mut visible = String::new();
    for ch in text.chars() {
        if ch.is_control() && ch != '\n' {
            visible.extend(ch.escape_default());
        } else {
            visible.push(ch);
        }
    }
    let mut out = String::new();
    for (index, line) in visible
        .strip_suffix('\n')
        .unwrap_or(&visible)
        .split('\n')
        .enumerate()
    {
        let indent = " ".repeat(prefix.len());
        let mut prefix = if index == 0 { prefix } else { &indent };
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
