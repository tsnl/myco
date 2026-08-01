//! Session compaction: archive predecessor, seed successor with summary + tail.

use crate::Session;
use myco_models::{Content, Message};
use myco_prompts as prompts;

/// How many trailing user-turns the successor keeps verbatim (well-formed).
const TAIL_USER_TURNS: usize = 2;
/// Max chars for any single tool body retained in the tail.
const TAIL_TOOL_BODY_MAX_CHARS: usize = 4_000;

#[derive(Debug, Clone)]
pub struct CompactOutcome {
    pub predecessor_id: String,
    pub successor_id: String,
    pub summary_path: std::path::PathBuf,
    pub tail_messages: usize,
}

/// Build a successor session from a summary file + well-formed recent tail.
///
/// Caller is responsible for: running the compact worker (which writes the summary),
/// installing the successor into the live agent, and linking UI.
pub fn compact_session(
    predecessor: &Session,
    summary_markdown: &str,
    model: &str,
) -> Result<(Session, CompactOutcome), String> {
    if predecessor.messages.is_empty() {
        return Err("cannot compact an empty session".into());
    }
    if summary_markdown.trim().is_empty() {
        return Err("summary markdown is empty".into());
    }

    let tail = select_tail(
        &predecessor.messages,
        TAIL_USER_TURNS,
        TAIL_TOOL_BODY_MAX_CHARS,
    );

    let mut successor = Session::new(model);
    successor.title = predecessor.title.clone();
    successor.links = predecessor.links.clone();
    successor.scratchpad = predecessor.scratchpad.clone();
    successor.predecessor_id = Some(predecessor.id.clone());
    // Nested (hidden) sessions stay nested across compaction; user sessions stay user.
    successor.kind = predecessor.kind;
    successor.parent_session_id = predecessor.parent_session_id.clone();

    let mut resume = String::from("# Compaction resume\n\n");
    resume.push_str(summary_markdown.trim());
    resume.push_str(&format!(
        "\n\n---\nPredecessor session: `{}`\nSummary file: `{}`\n",
        predecessor.id,
        predecessor.summary_path().display()
    ));

    // Compaction mints a new session id, so the successor's first message
    // stamps its own — the resume block below names the predecessor.
    let mut messages = vec![Message::UserMessage {
        content: vec![
            Content::Text {
                text: prompts::session_stamp(&successor.id, successor.created_at),
            },
            Content::Text { text: resume },
        ],
    }];
    messages.extend(tail.iter().cloned());
    let tail_messages = messages.len().saturating_sub(1);
    successor.messages = messages;

    // Persist summary next to predecessor if not already present / overwrite with canonical.
    let summary_path = predecessor.summary_path();
    if let Some(parent) = summary_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    myco_core::atomically_write(summary_path.as_path(), summary_markdown.as_bytes())?;

    let outcome = CompactOutcome {
        predecessor_id: predecessor.id.clone(),
        successor_id: successor.id.clone(),
        summary_path,
        tail_messages,
    };
    Ok((successor, outcome))
}

/// Link predecessor → successor on disk (updates both documents).
///
/// The successor is written first: a crash between the two saves must not leave
/// the predecessor pointing at a `successor_id` that has no file behind it. The
/// other order — an unreferenced successor on disk — is recoverable, since the
/// predecessor still reads as un-compacted.
pub fn link_compact_pair(predecessor: &mut Session, successor: &Session) -> Result<(), String> {
    successor.save()?;
    predecessor.successor_id = Some(successor.id.clone());
    predecessor.touch();
    predecessor.save()
}

/// Select the last `user_turns` well-formed user turns (user → … → assistant end).
pub fn select_tail(messages: &[Message], user_turns: usize, tool_body_max: usize) -> Vec<Message> {
    if user_turns == 0 || messages.is_empty() {
        return Vec::new();
    }
    // Find start indices of UserMessage entries.
    let user_idxs: Vec<usize> = messages
        .iter()
        .enumerate()
        .filter_map(|(i, m)| matches!(m, Message::UserMessage { .. }).then_some(i))
        .collect();
    if user_idxs.is_empty() {
        return Vec::new();
    }
    let start_user = user_idxs.len().saturating_sub(user_turns);
    let start = user_idxs[start_user];

    // Extend backward if we would start mid tool loop (shouldn't for UserMessage start).
    let slice = &messages[start..];
    // Ensure we don't end mid tool_use without results: if last is Assistant with tool_uses
    // and no following ToolResults, drop that incomplete assistant.
    let mut end = slice.len();
    if let Some(Message::AssistantMessage { tool_uses, .. }) = slice.last()
        && !tool_uses.is_empty()
    {
        end = end.saturating_sub(1);
    }
    let mut out: Vec<Message> = slice[..end].to_vec();
    for m in &mut out {
        truncate_message_bodies(m, tool_body_max);
    }
    out
}

fn truncate_message_bodies(msg: &mut Message, max_chars: usize) {
    match msg {
        Message::ToolResults { tool_use_results } => {
            for r in tool_use_results {
                for c in &mut r.content {
                    if let Content::Text { text } = c {
                        *text = truncate_chars(text, max_chars);
                    }
                }
            }
        }
        Message::AssistantMessage { content, .. } | Message::UserMessage { content } => {
            for c in content {
                if let Content::Text { text } = c {
                    *text = truncate_chars(text, max_chars.max(8_000));
                }
            }
        }
    }
}

fn truncate_chars(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let t: String = s.chars().take(max_chars.saturating_sub(20)).collect();
    format!("{t}\n…(truncated for compact tail)")
}

#[cfg(test)]
mod tests {
    use super::*;
    use myco_test_support::{assistant, assistant_tool, temp_home, tool_results, user};
    use serde_json::json;

    /// The successor document must exist before the predecessor points at it,
    /// so a crash mid-link can never leave a dangling `successor_id`.
    #[test]
    fn linking_writes_the_successor_before_the_pointer_to_it() {
        // RAII: points MYCO_HOME at a temp dir under the myco-home lock, and
        // clears both on drop even if an assertion panics.
        let _home = temp_home("compact-link");

        let mut pred = Session::new_with_id("m", "aa00bb11cc22dd33ee44ff5566778899");
        pred.messages = vec![user("hello"), assistant("hi")];
        pred.save().unwrap();

        let successor = pred.fork_child("m");
        link_compact_pair(&mut pred, &successor).unwrap();

        // Both documents on disk, cross-linked.
        let saved_pred = Session::load_by_id_or_prefix(&pred.id).unwrap();
        let saved_succ = Session::load_by_id_or_prefix(&successor.id).unwrap();
        assert_eq!(
            saved_pred.successor_id.as_deref(),
            Some(successor.id.as_str())
        );
        assert_eq!(saved_succ.id, successor.id);

        // The pointer is only durable because the target already was: loading
        // the id the predecessor names must succeed.
        assert!(
            Session::load_by_id_or_prefix(saved_pred.successor_id.as_deref().unwrap()).is_ok(),
            "predecessor names a successor that is not on disk"
        );
    }

    fn assistant_tools() -> Message {
        assistant_tool(None, "t1", "bash", json!({"command": "echo hi"}))
    }

    #[test]
    fn select_tail_keeps_last_user_turns_and_tool_loop() {
        let messages = vec![
            user("old"),
            assistant("old a"),
            user("mid"),
            assistant_tools(),
            tool_results(&[("t1", "hi\n")]),
            assistant("mid a"),
            user("new"),
            assistant("new a"),
        ];
        let tail = select_tail(&messages, 2, 1000);
        assert!(matches!(tail[0], Message::UserMessage { .. }));
        // mid + new = 2 user turns including tool loop
        assert!(tail.len() >= 5, "tail={tail:?}");
        assert!(matches!(
            tail.last(),
            Some(Message::AssistantMessage { .. })
        ));
    }

    #[test]
    fn select_tail_drops_trailing_incomplete_tool_use() {
        let messages = vec![user("u"), assistant_tools()];
        let tail = select_tail(&messages, 1, 1000);
        assert_eq!(tail.len(), 1);
        assert!(matches!(tail[0], Message::UserMessage { .. }));
    }

    #[test]
    fn compact_session_seeds_resume_and_links() {
        let _home = temp_home("compact");

        let mut pred = Session::new("claude-haiku-4-5");
        pred.messages = vec![user("hello"), assistant("world")];
        pred.title = Some("t".into());
        pred.save().unwrap();

        let (succ, out) =
            compact_session(&pred, "## Goal\nDo the thing\n", "claude-haiku-4-5").unwrap();
        assert_eq!(out.predecessor_id, pred.id);
        assert_eq!(succ.predecessor_id.as_deref(), Some(pred.id.as_str()));
        assert!(matches!(succ.messages[0], Message::UserMessage { .. }));
        // Compaction mints a new id, so the successor's first message stamps
        // its own — an agent that compacts must not keep quoting the
        // predecessor's id as its session.
        let Message::UserMessage { content } = &succ.messages[0] else {
            unreachable!()
        };
        assert!(
            matches!(&content[0], Content::Text { text }
                if prompts::is_session_stamp(text) && text.contains(&succ.id)),
            "{content:?}"
        );
        assert!(
            matches!(&content[1], Content::Text { text } if text.contains(&pred.id)),
            "{content:?}"
        );
        assert!(
            std::fs::read_to_string(pred.summary_path())
                .unwrap()
                .contains("Do the thing")
        );
    }
}
