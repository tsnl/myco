//! Compaction creates a successor thread with a summary and bounded recent context.

use crate::generative_model::{Content, Message};
use crate::prompts;
use crate::session::Session;

/// How many trailing user turns seed the successor context.
const TAIL_USER_TURNS: usize = 2;
/// Max chars for any single tool body retained in the tail.
const TAIL_TOOL_BODY_MAX_CHARS: usize = 4_000;

#[derive(Debug, Clone)]
pub struct CompactOutcome {
    pub session_id: String,
    pub predecessor_id: String,
    pub successor_id: String,
    pub summary_path: std::path::PathBuf,
    pub tail_messages: usize,
}

/// Build a successor from the summary at `session.summary_path()`. The caller
/// commits the returned thread to the live session atomically.
pub fn compact_thread(
    session: &Session,
    summary_markdown: &str,
) -> Result<(super::Thread, CompactOutcome), String> {
    let predecessor = session.active_thread();
    if predecessor.messages.is_empty() {
        return Err("cannot compact an empty thread".into());
    }
    if summary_markdown.trim().is_empty() {
        return Err("summary markdown is empty".into());
    }
    let tail = select_tail_indexed(
        &predecessor.messages,
        TAIL_USER_TURNS,
        TAIL_TOOL_BODY_MAX_CHARS,
    );
    let mut successor = super::Thread::new();
    successor.predecessor_id = Some(predecessor.id.clone());
    let resume = format!(
        "# Compaction resume\n\n{}\n\n---\nSession: `{}`\nPredecessor thread: `{}`\nSummary file: `{}`\n",
        summary_markdown.trim(),
        session.id,
        predecessor.id,
        session.summary_path().display()
    );
    successor.messages = vec![Message::UserMessage {
        content: vec![
            Content::Text {
                text: prompts::thread_stamp(&session.id, &successor.id, session.created_at),
            },
            Content::Text { text: resume },
        ],
    }];
    for (old_index, message) in tail {
        let index = successor.messages.len();
        successor.messages.push(message);
        if let Some(time) = predecessor.user_turn_timestamps.get(&old_index) {
            successor.user_turn_timestamps.insert(index, *time);
        }
    }
    let outcome = CompactOutcome {
        session_id: session.id.clone(),
        predecessor_id: predecessor.id.clone(),
        successor_id: successor.id.clone(),
        summary_path: session.summary_path(),
        tail_messages: successor.messages.len() - 1,
    };
    Ok((successor, outcome))
}

/// Select the last `user_turns` well-formed user turns (user → … → assistant end).
pub fn select_tail(messages: &[Message], user_turns: usize, tool_body_max: usize) -> Vec<Message> {
    select_tail_indexed(messages, user_turns, tool_body_max)
        .into_iter()
        .map(|(_, message)| message)
        .collect()
}

fn select_tail_indexed(
    messages: &[Message],
    user_turns: usize,
    tool_body_max: usize,
) -> Vec<(usize, Message)> {
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
    let mut out: Vec<_> = slice[..end]
        .iter()
        .cloned()
        .enumerate()
        .map(|(index, message)| (start + index, message))
        .collect();
    for (_, message) in &mut out {
        if let Message::UserMessage { content } = message {
            content.retain(
                |part| !matches!(part, Content::Text { text } if prompts::is_session_stamp(text)),
            );
        }
        truncate_message_bodies(message, tool_body_max);
    }
    out.retain(
        |(_, message)| !matches!(message, Message::UserMessage { content } if content.is_empty()),
    );
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
    use crate::test_support::{assistant, assistant_tool, temp_home, tool_results, user};
    use serde_json::json;

    #[test]
    fn carried_context_cannot_override_the_successor_thread_stamp() {
        let mut session = Session::new("test");
        let original = session.active_thread().id.clone();
        let stamp = prompts::thread_stamp(&session.id, &original, session.created_at);
        session.replace_context(
            vec![
                Message::UserMessage {
                    content: vec![
                        Content::Text { text: stamp },
                        Content::Text {
                            text: "task".into(),
                        },
                    ],
                },
                assistant("done"),
            ],
            None,
        );
        let (successor, _) = compact_thread(&session, "summary").unwrap();
        let stamps: Vec<_> = successor
            .messages
            .iter()
            .filter_map(|message| {
                if let Message::UserMessage { content } = message {
                    Some(content)
                } else {
                    None
                }
            })
            .flatten()
            .filter_map(|part| match part {
                Content::Text { text } if prompts::is_session_stamp(text) => Some(text),
                _ => None,
            })
            .collect();
        assert_eq!(stamps.len(), 1);
        assert!(stamps[0].contains(&successor.id));
        assert!(!stamps[0].contains(&original));
        assert!(
            serde_json::to_string(&successor.messages)
                .unwrap()
                .contains("task")
        );
    }

    #[test]
    fn compaction_preserves_session_identity_and_archives_the_original_thread() {
        let _home = temp_home("thread-compact");
        let mut session = Session::new("m");
        session.replace_context(vec![user("hello"), assistant("answer")], None);
        session.title = Some("ongoing".into());
        let id = session.id.clone();
        let original = serde_json::to_value(session.active_thread()).unwrap();
        let active = super::super::ActiveSession::new(session);
        let (next, outcome) = compact_thread(&active.snapshot(), "# Goal\nContinue work").unwrap();
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime
            .block_on(active.writer())
            .commit_thread(next)
            .unwrap();
        let saved = Session::load_by_id_or_prefix(&id).unwrap();
        assert_eq!(saved.id, id);
        assert_eq!(saved.title.as_deref(), Some("ongoing"));
        assert_eq!(saved.threads().len(), 2);
        assert_eq!(serde_json::to_value(&saved.threads()[0]).unwrap(), original);
        assert_eq!(
            saved.active_thread().predecessor_id.as_deref(),
            Some(outcome.predecessor_id.as_str())
        );
        assert_eq!(saved.active_thread().id, outcome.successor_id);
        assert!(saved.active_thread().last_usage.is_none());
        assert!(
            matches!(&saved.active_thread().messages[0], Message::UserMessage { content }
            if matches!(&content[1], Content::Text { text } if text.contains("Continue work")))
        );
    }

    #[test]
    fn stale_checkpoints_cannot_overwrite_a_successor_thread() {
        let _home = temp_home("thread-stale");
        let mut session = Session::new("m");
        session.replace_context(vec![user("original")], None);
        let original = serde_json::to_value(session.active_thread()).unwrap();
        let predecessor = session.active_thread().id.clone();
        let (next, _) = compact_thread(&session, "summary").unwrap();
        let active = super::super::ActiveSession::new(session);
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let writer = runtime.block_on(active.writer());
        active.with_mut(|session| session.title = Some("updated during compaction".into()));
        writer.commit_thread(next.clone()).unwrap();
        assert!(
            active
                .persist_thread_messages(&predecessor, &[user("stale")], None, true)
                .is_err()
        );
        assert_eq!(
            active.snapshot().title.as_deref(),
            Some("updated during compaction")
        );
        assert_eq!(
            serde_json::to_value(&active.snapshot().threads()[0]).unwrap(),
            original
        );
        assert!(writer.commit_thread(next).is_err());
    }

    #[test]
    fn failed_compaction_save_keeps_the_original_thread_active() {
        let home = temp_home("thread-save-failure");
        let mut session = Session::new("m");
        session.replace_context(vec![user("original")], None);
        let original_id = session.active_thread().id.clone();
        let (next, _) = compact_thread(&session, "summary").unwrap();
        let active = super::super::ActiveSession::new(session);
        std::fs::write(home.path().join("session"), "blocks the store directory").unwrap();
        let runtime = tokio::runtime::Runtime::new().unwrap();
        assert!(
            runtime
                .block_on(active.writer())
                .commit_thread(next)
                .is_err()
        );
        assert_eq!(active.snapshot().active_thread().id, original_id);
        assert_eq!(active.snapshot().threads().len(), 1);
    }

    fn assistant_tools() -> Message {
        assistant_tool(None, "bash", json!({"command": "echo hi"}))
    }

    #[test]
    fn compaction_preserves_original_acceptance_times_and_archive_status() {
        let _home = temp_home("compact-timestamps");
        let mut session = Session::new("test");
        session.replace_context(
            vec![
                user("older"),
                assistant("answer"),
                user("latest"),
                assistant("answer"),
            ],
            None,
        );
        let time = chrono::Utc::now();
        session
            .active_thread_mut()
            .user_turn_timestamps
            .insert(2, time);
        session.archived = true;
        let (next, _) = compact_thread(&session, "summary").unwrap();
        assert_eq!(next.user_turn_timestamps.len(), 1);
        assert_eq!(next.user_turn_timestamps[&3], time);
        let active = super::super::ActiveSession::new(session);
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime
            .block_on(active.writer())
            .commit_thread(next)
            .unwrap();
        let saved = Session::load(&active.snapshot().json_path()).unwrap();
        assert!(saved.archived);
        assert_eq!(saved.threads()[0].user_turn_timestamps[&2], time);
        assert_eq!(saved.active_thread().user_turn_timestamps[&3], time);
    }

    #[test]
    fn select_tail_keeps_last_user_turns_and_tool_loop() {
        let messages = vec![
            user("old"),
            assistant("old a"),
            user("mid"),
            assistant_tools(),
            tool_results(&["hi\n"]),
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
}
