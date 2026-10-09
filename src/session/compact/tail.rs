//! Retain whole recent turns within a budget, before cloning their contents.

use crate::core::image_store::{ImageStore, is_reference};
use crate::generative_model::{Content, Message};

const MAX_TAIL_MESSAGES: usize = 64;
const MAX_TAIL_BYTES: usize = 64 * 1024;

//
// Complete turn selection
//

/// Select up to `user_turns` recent turns, bounded by 64 messages and 64 KiB.
/// A turn that exceeds either limit is represented by the compaction summary.
/// Image sidecars count at their resolved size; unknown-size images omit a turn.
pub fn select_tail(messages: &[Message], user_turns: usize, tool_body_max: usize) -> Vec<Message> {
    select_tail_indexed(messages, user_turns, tool_body_max)
        .into_iter()
        .map(|(_, message)| message)
        .collect()
}

pub(super) fn select_tail_indexed(
    messages: &[Message],
    user_turns: usize,
    tool_body_max: usize,
) -> Vec<(usize, Message)> {
    let end = complete_end(messages);
    let start = bounded_start(&messages[..end], user_turns);
    let mut tail: Vec<_> = messages[start..end]
        .iter()
        .cloned()
        .enumerate()
        .map(|(offset, message)| (start + offset, message))
        .collect();
    for (_, message) in &mut tail {
        trim_message(message, tool_body_max);
    }
    tail.retain(
        |(_, message)| !matches!(message, Message::UserMessage { content } if content.is_empty()),
    );
    tail
}

fn complete_end(messages: &[Message]) -> usize {
    let incomplete = matches!(messages.last(),
        Some(Message::AssistantMessage { tool_uses, .. }) if !tool_uses.is_empty());
    messages.len().saturating_sub(usize::from(incomplete))
}

fn bounded_start(messages: &[Message], user_turns: usize) -> usize {
    let mut start = messages.len();
    for (index, _) in messages
        .iter()
        .enumerate()
        .rev()
        .filter(|(_, message)| message.is_user_turn())
        .take(user_turns)
    {
        if !fits_budget(&messages[index..]) {
            break;
        }
        start = index;
    }
    start
}

//
// Bounded accounting
//

fn fits_budget(messages: &[Message]) -> bool {
    if messages.len() > MAX_TAIL_MESSAGES {
        return false;
    }
    let mut budget = ByteBudget(MAX_TAIL_BYTES);
    if serde_json::to_writer(&mut budget, messages).is_err() {
        return false;
    }
    messages
        .iter()
        .flat_map(Message::content)
        .all(|part| image_bytes(part).is_some_and(|bytes| budget.consume(bytes).is_ok()))
}

fn image_bytes(part: &Content) -> Option<usize> {
    let Content::Image { source } = part else {
        return Some(0);
    };
    if source.starts_with("http://") || source.starts_with("https://") {
        return None;
    }
    if !is_reference(source) {
        return Some(0); // Inline payloads were already counted by serialization.
    }
    let path = ImageStore::for_profile().ok()?.path(source).ok()?;
    let metadata = std::fs::metadata(path).ok()?;
    if !metadata.is_file() {
        return None;
    }
    let bytes = usize::try_from(metadata.len()).ok()?;
    // Charge the entire resolved data URL conservatively, without reading blobs.
    Some(bytes.div_ceil(3).saturating_mul(4).saturating_add(32))
}

struct ByteBudget(usize);

impl ByteBudget {
    fn consume(&mut self, bytes: usize) -> std::io::Result<()> {
        self.0 = self
            .0
            .checked_sub(bytes)
            .ok_or_else(|| std::io::Error::other("compaction tail exceeds byte budget"))?;
        Ok(())
    }
}

impl std::io::Write for ByteBudget {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.consume(bytes.len())?;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

//
// Body previews
//

fn trim_message(message: &mut Message, tool_body_max: usize) {
    match message {
        Message::ToolResults { tool_use_results } => {
            for result in tool_use_results {
                truncate_text(&mut result.content, tool_body_max);
            }
        }
        Message::UserMessage { content } => {
            content.retain(|part| {
                !matches!(part, Content::System { kind, .. } if kind == "session" || kind == "runtime")
            });
            truncate_text(content, tool_body_max.max(8_000));
        }
        Message::AssistantMessage { content, .. } => {
            truncate_text(content, tool_body_max.max(8_000));
        }
    }
}

fn truncate_text(content: &mut [Content], max_chars: usize) {
    for part in content {
        if let Content::Text { text } = part {
            truncate_chars(text, max_chars);
        }
    }
}

fn truncate_chars(text: &mut String, max_chars: usize) {
    const MARKER: &str = "\n...(truncated for compact tail)";
    if text.chars().count() <= max_chars {
        return;
    }
    // ASCII marker length bounds both character count and serialized bytes.
    let marker = &MARKER[..MARKER.len().min(max_chars)];
    let prefix: String = text
        .chars()
        .take(max_chars.saturating_sub(marker.len()))
        .collect();
    *text = format!("{prefix}{marker}");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{Session, compact_thread};
    use crate::test_support::{assistant, assistant_tool, temp_home, tool_results, user};
    use base64::Engine as _;
    use serde_json::json;

    #[test]
    fn a_long_autonomous_turn_uses_the_summary_without_copying_its_tool_loop() {
        let _home = temp_home("compact-long-turn");
        let mut messages = vec![user("finish the task")];
        for _ in 0..1000 {
            messages.push(assistant_tool(None, "bash", json!({"command": "true"})));
            messages.push(tool_results(&["done"]));
        }
        messages.push(assistant("progress"));
        let mut session = Session::new("test");
        session.replace_context(messages.clone(), None);
        let (next, outcome) =
            compact_thread(&session, "Keep working from the saved findings").unwrap();
        assert_eq!(outcome.tail_messages, 0);
        assert_eq!(next.messages.len(), 1);
        crate::agent::validate_context(&next.messages).unwrap();
        assert_eq!(session.active_thread().messages, messages);
    }

    #[test]
    fn a_large_older_turn_does_not_displace_a_recent_complete_tool_exchange() {
        let messages = vec![
            user(&"old context".repeat(10_000)),
            assistant("old answer"),
            user("recent request"),
            assistant_tool(None, "bash", json!({"command": "true"})),
            tool_results(&["done"]),
            assistant("recent answer"),
        ];
        let tail = select_tail(&messages, 2, 1000);
        assert_eq!(tail, messages[2..]);
        crate::agent::validate_context(&tail).unwrap();
    }

    #[test]
    fn large_arguments_and_many_small_content_parts_cannot_bypass_the_byte_budget() {
        let large_arguments = vec![
            user("request"),
            assistant_tool(None, "bash", json!({"command": "x".repeat(70_000)})),
            tool_results(&["done"]),
        ];
        assert!(select_tail(&large_arguments, 2, 1000).is_empty());
        let many_parts = vec![
            user("request"),
            Message::AssistantMessage {
                content: vec![
                    Content::Text {
                        text: "界".repeat(20)
                    };
                    2000
                ],
                tool_uses: vec![],
                turn_end_reason: None,
            },
        ];
        assert!(select_tail(&many_parts, 2, 1000).is_empty());
    }

    #[test]
    fn images_are_budgeted_at_resolved_size_for_each_occurrence() {
        let _home = temp_home("compact-image-budget");
        let store = ImageStore::for_profile().unwrap();
        let mut content = vec![Content::Image {
            source: base64::engine::general_purpose::STANDARD.encode(vec![42; 8 * 1024]),
        }];
        store.externalize(&mut content).unwrap();
        let small = vec![Message::UserMessage {
            content: content.clone(),
        }];
        assert_eq!(select_tail(&small, 1, 1000), small);
        let large = vec![Message::UserMessage {
            content: vec![content[0].clone(); 8],
        }];
        assert!(serde_json::to_vec(&large).unwrap().len() < 2000);
        assert!(select_tail(&large, 1, 1000).is_empty());
        let Content::Image { source } = &content[0] else {
            panic!("expected image")
        };
        std::fs::remove_file(store.path(source).unwrap()).unwrap();
        assert!(select_tail(&small, 1, 1000).is_empty());
    }

    #[test]
    fn an_unknown_size_remote_image_uses_the_summary_instead_of_an_unbounded_request() {
        let messages = vec![Message::UserMessage {
            content: vec![Content::Image {
                source: "https://example.test/image.png".into(),
            }],
        }];
        assert!(select_tail(&messages, 1, 1000).is_empty());
    }

    #[test]
    fn body_previews_respect_even_tiny_limits_without_expanding_the_serialized_tail() {
        let messages = vec![
            user("request"),
            assistant_tool(None, "bash", json!({"command": "true"})),
            tool_results(&[&"界".repeat(100)]),
        ];
        for limit in [0, 1, 10, 31, 50] {
            let tail = select_tail(&messages, 1, limit);
            let text = tail[2]
                .content()
                .find_map(|part| match part {
                    Content::Text { text } => Some(text),
                    _ => None,
                })
                .unwrap();
            assert!(text.chars().count() <= limit);
            assert!(
                serde_json::to_vec(&tail).unwrap().len()
                    <= serde_json::to_vec(&messages).unwrap().len()
            );
            crate::agent::validate_context(&tail).unwrap();
        }
    }
}
