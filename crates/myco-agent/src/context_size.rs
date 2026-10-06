//! Incremental prompt sizing, independent of provider tokenizers and wire adapters.

use myco_model::{Content, Message, TokenUsage};

/// Three UTF-8 bytes per token plus framing is a sizing heuristic, not a limit
/// guarantee. Images and provider-owned instructions/schema need separate limits.
#[derive(Clone, Debug, Default)]
pub(crate) struct ContextSize {
    estimated: u64,
    measured: Option<(u64, u64)>,
}

impl ContextSize {
    pub(crate) fn restored(history: &[Message], usage: Option<TokenUsage>) -> Self {
        let mut size = Self::default();
        for message in history {
            size.append(message);
        }
        // Saved usage does not identify its exact input prefix. Charging the
        // restored history as well can compact early, but cannot drop growth
        // from later responses that omitted usage information.
        size.restore_hint(usage.map(TokenUsage::context_tokens));
        size
    }

    pub(crate) fn restore_hint(&mut self, tokens: Option<u64>) {
        if self.measured.is_none() {
            self.measured = tokens.map(|tokens| (tokens, 0));
        }
    }

    pub(crate) fn observe_input(&mut self, usage: TokenUsage) {
        self.measured = Some((usage.context_tokens(), self.estimated));
    }

    pub(crate) fn tokens(&self) -> u64 {
        self.measured.map_or(self.estimated, |(tokens, prefix)| {
            tokens.saturating_add(self.estimated.saturating_sub(prefix))
        })
    }

    pub(crate) fn append(&mut self, message: &Message) {
        self.estimated = self.estimated.saturating_add(message_tokens(message));
    }

    pub(crate) fn append_part(&mut self, part: &Content) {
        self.estimated = self.estimated.saturating_add(part_tokens(part));
    }
}

fn message_tokens(message: &Message) -> u64 {
    let mut tokens = message
        .content()
        .fold(8_u64, |sum, part| sum.saturating_add(part_tokens(part)));
    match message {
        Message::AssistantMessage { tool_uses, .. } => {
            for call in tool_uses {
                let mut bytes = ByteCount(call.name.len() as u64);
                serde_json::to_writer(&mut bytes, &call.input).expect("count JSON tool arguments");
                tokens = tokens.saturating_add(8 + bytes.0.div_ceil(3));
            }
        }
        Message::ToolResults { tool_use_results } => {
            tokens = tokens.saturating_add((tool_use_results.len() as u64).saturating_mul(8));
        }
        _ => {}
    }
    tokens
}

fn part_tokens(part: &Content) -> u64 {
    match part {
        Content::Text { text } | Content::System { text, .. } => (text.len() as u64).div_ceil(3),
        // Thinking is not echoed. Image size rejection is handled by the
        // request-size recovery path, independently of this text estimate.
        Content::Thinking { .. } | Content::Image { .. } => 0,
    }
}

struct ByteCount(u64);

impl std::io::Write for ByteCount {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0 = self.0.saturating_add(bytes.len() as u64);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use myco_model::{ToolResult, ToolUse};
    use serde_json::json;

    fn text(value: &str) -> Message {
        Message::UserMessage {
            content: vec![Content::Text { text: value.into() }],
        }
    }

    #[test]
    fn measured_input_is_anchored_without_charging_cumulative_output_tokens() {
        let mut size = ContextSize::restored(&[text("old context")], None);
        size.observe_input(TokenUsage {
            input_tokens: 10_000,
            output_tokens: 90_000,
            cached_input_tokens: 9000,
        });
        size.append(&text(&"new".repeat(1000)));
        assert_eq!(size.tokens(), 11_008);
        size.append(&Message::ToolResults {
            tool_use_results: vec![ToolResult::text("new".repeat(2000))],
        });
        assert_eq!(size.tokens(), 13_024);
        size.observe_input(TokenUsage {
            input_tokens: 12_000,
            output_tokens: 100_000,
            cached_input_tokens: 9000,
        });
        assert_eq!(size.tokens(), 12_000);
    }

    #[test]
    fn a_live_measurement_replaces_the_heuristic_for_its_exact_input_prefix() {
        let mut size = ContextSize::restored(&[text(&"x".repeat(100_000))], None);
        assert!(size.tokens() > 30_000);
        size.observe_input(TokenUsage {
            input_tokens: 10_000,
            output_tokens: 500,
            cached_input_tokens: 0,
        });
        assert_eq!(size.tokens(), 10_000);
        size.append(&text(&"new".repeat(1000)));
        assert_eq!(size.tokens(), 11_008);
    }

    #[test]
    fn no_usage_still_counts_arguments_and_text_but_not_hidden_metadata_or_thinking() {
        let hidden = "hidden".repeat(100_000);
        let plain = Message::UserMessage {
            content: vec![
                Content::System {
                    kind: "runtime".into(),
                    text: "small".into(),
                    data: json!({"full_inventory":hidden}),
                },
                Content::Thinking {
                    text: hidden,
                    signature: None,
                    redacted: false,
                },
            ],
        };
        let mut size = ContextSize::restored(&[plain], None);
        assert_eq!(size.tokens(), 10);
        size.append(&Message::AssistantMessage {
            content: vec![],
            tool_uses: vec![ToolUse {
                name: "bash".into(),
                input: json!({"command":"界".repeat(9000)}),
            }],
            turn_end_reason: None,
        });
        assert!(size.tokens() >= 9000);
    }
}
