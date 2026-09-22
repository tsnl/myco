use crate::Error;

/// Decode complete SSE data events without assuming HTTP chunk boundaries,
/// UTF-8 boundaries, or a particular line ending. EOF never completes a frame.
#[derive(Default)]
pub(crate) struct Sse {
    bytes: Vec<u8>,
    fields: Fields,
}

impl Sse {
    pub fn push(&mut self, bytes: &[u8], eof: bool) -> Result<Vec<String>, Error> {
        self.bytes.extend_from_slice(bytes);
        let mut events = vec![];
        let mut start = 0;
        while let Some((end, next)) = line_bounds(&self.bytes, start, eof) {
            events.extend(self.fields.line(&self.bytes[start..end])?);
            start = next;
        }
        self.bytes.drain(..start);
        Ok(events)
    }
}

fn line_bounds(bytes: &[u8], start: usize, eof: bool) -> Option<(usize, usize)> {
    let offset = bytes[start..]
        .iter()
        .position(|b| matches!(b, b'\r' | b'\n'))?;
    let end = start + offset;
    if bytes[end] == b'\r' && end + 1 == bytes.len() && !eof {
        return None;
    }
    let crlf = bytes[end] == b'\r' && bytes.get(end + 1) == Some(&b'\n');
    Some((end, end + 1 + usize::from(crlf)))
}

#[derive(Default)]
struct Fields {
    data: Vec<String>,
    saw_line: bool,
}

impl Fields {
    fn line(&mut self, bytes: &[u8]) -> Result<Option<String>, Error> {
        let mut line = std::str::from_utf8(bytes)
            .map_err(|e| Error::Protocol(format!("SSE is not UTF-8: {e}")))?;
        if !self.saw_line {
            line = line.strip_prefix('\u{feff}').unwrap_or(line);
            self.saw_line = true;
        }
        Ok(self.field(line))
    }

    fn field(&mut self, line: &str) -> Option<String> {
        if line.is_empty() {
            return self.finish();
        }
        let (name, value) = line.split_once(':').unwrap_or((line, ""));
        if name == "data" {
            self.data
                .push(value.strip_prefix(' ').unwrap_or(value).into());
        }
        None
    }

    fn finish(&mut self) -> Option<String> {
        if self.data.is_empty() {
            return None;
        }
        let event = self.data.join("\n");
        self.data.clear();
        Some(event)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arbitrary_chunks_preserve_unicode_multiline_data_and_line_endings() {
        for newline in ["\n", "\r\n", "\r"] {
            let text = format!(
                "\u{feff}: heartbeat{newline}event: update{newline}data: 雪{newline}data: more{newline}{newline}"
            );
            for size in 1..=text.len() {
                let mut parser = Sse::default();
                let mut events = vec![];
                for chunk in text.as_bytes().chunks(size) {
                    events.extend(parser.push(chunk, false).unwrap());
                }
                events.extend(parser.push(&[], true).unwrap());
                assert_eq!(events, ["雪\nmore"]);
            }
        }
    }

    #[test]
    fn eof_does_not_turn_an_unterminated_event_into_a_complete_event() {
        let mut parser = Sse::default();
        assert!(parser.push(b"data: incomplete\n", true).unwrap().is_empty());
    }
}
