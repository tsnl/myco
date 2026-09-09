//! SSE parsing, shared by both streaming backends.

/// Splits a server-sent-events byte stream into complete `data:` payloads.
///
/// Buffers **bytes**, not text: network chunks split anywhere, including the
/// middle of a multi-byte UTF-8 sequence, so converting per-chunk would turn
/// the boundary character into U+FFFD on both sides — silently corrupting
/// streamed text *and* tool-input JSON. Lines are only decoded once complete.
#[derive(Default)]
pub(crate) struct SseParser {
    buffer: Vec<u8>,
    pending_data_lines: Vec<String>,
}

impl SseParser {
    /// Push a chunk of bytes and return complete `data:` payloads (one per SSE event).
    pub(crate) fn push(&mut self, chunk: &[u8]) -> Vec<String> {
        self.buffer.extend_from_slice(chunk);

        let mut events = Vec::new();
        while let Some(newline_at) = self.buffer.iter().position(|&b| b == b'\n') {
            let mut line: Vec<u8> = self.buffer.drain(..=newline_at).collect();
            line.pop(); // the '\n'
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            let line = String::from_utf8_lossy(&line);

            if line.is_empty() {
                if !self.pending_data_lines.is_empty() {
                    events.push(self.pending_data_lines.join("\n"));
                    self.pending_data_lines.clear();
                }
                continue;
            }

            if let Some(data) = line.strip_prefix("data:") {
                let data = data.strip_prefix(' ').unwrap_or(data);
                if data.trim() == "[DONE]" {
                    continue;
                }
                self.pending_data_lines.push(data.to_string());
            }
            // Ignore event:/id:/retry:/comments — JSON `type` is authoritative.
        }

        events
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sse_parser_survives_chunk_splits_inside_multibyte_utf8() {
        let event = "data: {\"text\":\"héllo → 世界 🌍\"}\n\n";
        // 1-byte chunks force a split inside every multi-byte sequence.
        let mut parser = SseParser::default();
        let mut events = Vec::new();
        for byte in event.as_bytes() {
            events.extend(parser.push(std::slice::from_ref(byte)));
        }
        assert_eq!(events, vec!["{\"text\":\"héllo → 世界 🌍\"}".to_string()]);
    }

    #[test]
    fn sse_parser_joins_multiline_data_and_skips_done() {
        let mut parser = SseParser::default();
        let events = parser.push(b"data: a\ndata: b\n\ndata: [DONE]\n\nevent: x\ndata: c\r\n\r\n");
        assert_eq!(events, vec!["a\nb".to_string(), "c".to_string()]);
    }
}
