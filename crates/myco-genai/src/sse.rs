use crate::Error;

/// Decode complete SSE data events without assuming HTTP chunk boundaries,
/// UTF-8 boundaries, or a particular line ending. EOF never completes a frame.
#[derive(Default)]
pub(crate) struct Sse {
    bytes: Vec<u8>,
    data: Vec<String>,
    saw_line: bool,
}

impl Sse {
    pub fn push(&mut self, bytes: &[u8], eof: bool) -> Result<Vec<String>, Error> {
        self.bytes.extend_from_slice(bytes);
        let mut events = vec![];
        let mut start = 0;
        while let Some(offset) = self.bytes[start..]
            .iter()
            .position(|b| matches!(b, b'\r' | b'\n'))
        {
            let end = start + offset;
            if self.bytes[end] == b'\r' && end + 1 == self.bytes.len() && !eof {
                break;
            }
            let mut line = std::str::from_utf8(&self.bytes[start..end])
                .map_err(|e| Error::Protocol(format!("SSE is not UTF-8: {e}")))?;
            if !self.saw_line {
                line = line.strip_prefix('\u{feff}').unwrap_or(line);
                self.saw_line = true;
            }
            if line.is_empty() {
                if !self.data.is_empty() {
                    events.push(self.data.join("\n"));
                    self.data.clear();
                }
            } else {
                let (name, value) = line.split_once(':').unwrap_or((line, ""));
                if name == "data" {
                    self.data
                        .push(value.strip_prefix(' ').unwrap_or(value).into());
                }
            }
            start = end + 1;
            if self.bytes[end] == b'\r' && self.bytes.get(start) == Some(&b'\n') {
                start += 1;
            }
        }
        self.bytes.drain(..start);
        Ok(events)
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
