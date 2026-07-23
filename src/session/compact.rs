//! Session compaction: archive predecessor, seed successor with summary + tail.

use std::sync::Arc;

use crate::core::CancelToken;
use crate::generative_model::{self, CatalogModel, Content, GenerativeModelConfig, Message};
use crate::harness::Harness;
use crate::prompts;
use crate::session::{
    Agent, AgentInteractionError, NullEventSink, Session, SessionKind, TraceContext,
    uuid_simple_hex,
};

/// Options for [`compact_session`].
#[derive(Debug, Clone)]
pub struct CompactOptions {
    /// How many trailing user-turns to keep verbatim (well-formed).
    pub tail_user_turns: usize,
    /// Max chars for any single tool body retained in the tail.
    pub tail_tool_body_max_chars: usize,
}

impl Default for CompactOptions {
    fn default() -> Self {
        Self {
            tail_user_turns: 2,
            tail_tool_body_max_chars: 4_000,
        }
    }
}

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
    opts: &CompactOptions,
) -> Result<(Session, CompactOutcome), String> {
    if predecessor.messages.is_empty() {
        return Err("cannot compact an empty session".into());
    }
    if summary_markdown.trim().is_empty() {
        return Err("summary markdown is empty".into());
    }

    let tail = select_tail(
        &predecessor.messages,
        opts.tail_user_turns,
        opts.tail_tool_body_max_chars,
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

    let mut messages = vec![Message::UserMessage {
        content: vec![Content::Text { text: resume }],
    }];
    messages.extend(tail.iter().cloned());
    let tail_messages = messages.len().saturating_sub(1);
    successor.messages = messages;

    // Persist summary next to predecessor if not already present / overwrite with canonical.
    let summary_path = predecessor.summary_path();
    if let Some(parent) = summary_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    crate::session::atomically_write(summary_path.as_path(), summary_markdown.as_bytes())?;

    let outcome = CompactOutcome {
        predecessor_id: predecessor.id.clone(),
        successor_id: successor.id.clone(),
        summary_path,
        tail_messages,
    };
    Ok((successor, outcome))
}

/// Link predecessor → successor on disk (updates both documents).
pub fn link_compact_pair(predecessor: &mut Session, successor: &Session) -> Result<(), String> {
    predecessor.successor_id = Some(successor.id.clone());
    predecessor.touch();
    predecessor.save()?;
    successor.save()?;
    Ok(())
}

/// How [`run_compact_worker`] ended without producing a successor.
#[derive(Debug)]
pub enum CompactWorkerError {
    /// The worker turn was cancelled (Ctrl-C); the predecessor is unchanged.
    Cancelled,
    /// Any other failure, as a printable reason.
    Failed(String),
}

/// The whole compact worker lifecycle against a saved `predecessor`: create
/// the hidden worker session, run the worker agent (which writes the summary
/// via `session_history`), read the summary back, then build and link the
/// successor. Pure orchestration — the caller owns all UI (progress, error
/// display) and installing the successor into the live REPL.
pub async fn run_compact_worker(
    predecessor: &Session,
    catalog_model: &CatalogModel,
    harness: Arc<Harness>,
    cancel: CancelToken,
) -> Result<(Session, CompactOutcome), CompactWorkerError> {
    let worker_id = uuid::Uuid::new_v4();
    let worker_hex = uuid_simple_hex(worker_id);
    let mut worker_session = Session::new_hidden(
        catalog_model.spec.key.clone(),
        worker_hex.clone(),
        SessionKind::Compact,
        Some(predecessor.id.clone()),
    );
    worker_session.title = Some(format!(
        "compact {}",
        &predecessor.id[..8.min(predecessor.id.len())]
    ));
    if let Err(e) = worker_session.save() {
        eprintln!("warning: could not save compact worker session: {e}");
    }

    let model = match generative_model::new(GenerativeModelConfig {
        model: catalog_model.spec.clone(),
        tools: harness.tool_specs(),
        system_prompt: [
            "You are a myco compaction worker. Follow the user instruction exactly. \
             Prefer session_history over bash for reading sessions."
                .to_string(),
            prompts::agent_prompt_epilogue(),
            prompts::model_stamp(&catalog_model.spec.key),
        ]
        .join("\n\n"),
        backend_config: catalog_model.backend.clone(),
    }) {
        Ok(m) => m,
        Err(e) => {
            return Err(CompactWorkerError::Failed(format!(
                "failed to create model: {e:?}"
            )));
        }
    };

    let sink = Arc::new(NullEventSink);
    let mut worker = Agent::with_context(
        model,
        harness.clone(),
        sink,
        TraceContext {
            agent_id: worker_id,
            depth: 1,
            parent_tool_use_id: None,
        },
    );
    worker.set_context_window_tokens(catalog_model.spec.context_window_tokens);

    let prompt = compact_subagent_prompt(&predecessor.id);
    let result = worker
        .interact(vec![Content::Text { text: prompt }], cancel)
        .await;

    worker_session.messages = worker.history().to_vec();
    worker_session.touch();
    let _ = worker_session.save();

    match result {
        Ok(_) => {}
        Err(AgentInteractionError::Cancelled) => return Err(CompactWorkerError::Cancelled),
        Err(e) => return Err(CompactWorkerError::Failed(format!("worker failed: {e}"))),
    }

    let summary_path = predecessor.summary_path();
    let summary = match std::fs::read_to_string(&summary_path) {
        Ok(s) if !s.trim().is_empty() => s,
        Ok(_) => {
            return Err(CompactWorkerError::Failed(format!(
                "worker finished but summary file is empty ({})",
                summary_path.display()
            )));
        }
        Err(e) => {
            return Err(CompactWorkerError::Failed(format!(
                "worker finished but summary missing at {}: {e}",
                summary_path.display()
            )));
        }
    };

    let (successor, outcome) = compact_session(
        predecessor,
        &summary,
        &catalog_model.spec.key,
        &CompactOptions::default(),
    )
    .map_err(|e| CompactWorkerError::Failed(format!("failed to build successor: {e}")))?;

    let mut pred = predecessor.clone();
    link_compact_pair(&mut pred, &successor)
        .map_err(|e| CompactWorkerError::Failed(format!("failed to link sessions: {e}")))?;

    Ok((successor, outcome))
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

/// Prompt for a compact subagent.
pub fn compact_subagent_prompt(predecessor_id: &str) -> String {
    format!(
        r#"You are a compaction worker. Explore session `{predecessor_id}` with the `session_history` tool (stats, search, range, expand). Do NOT use bash to read session JSON.

Write a concise markdown summary via `session_history` action `write_summary` for that same session_id. The summary MUST use these headings:

# Goal / active task
# Decisions
# Key paths
# Todos / open work
# Constraints
# Recent outcome

Rules:
- Prefer absolute paths, hosts, branch names, PR links, and concrete decisions.
- Drop raw tool stdout and exploratory dead-ends unless they constrain next steps.
- Keep the whole summary under ~1500 tokens.
- After write_summary succeeds, reply with only: SUMMARY_OK path=<path from tool>
"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::generative_model::{ToolResult, ToolUse, TurnEndReason};
    use crate::session::uuid_simple_hex;
    use serde_json::json;

    fn user(text: &str) -> Message {
        Message::UserMessage {
            content: vec![Content::Text { text: text.into() }],
        }
    }

    fn assistant_end(text: &str) -> Message {
        Message::AssistantMessage {
            content: vec![Content::Text { text: text.into() }],
            tool_uses: vec![],
            turn_end_reason: Some(TurnEndReason::EndTurn),
        }
    }

    fn assistant_tools() -> Message {
        Message::AssistantMessage {
            content: vec![],
            tool_uses: vec![ToolUse {
                id: "t1".into(),
                name: "bash".into(),
                input: json!({"command": "echo hi"}),
            }],
            turn_end_reason: Some(TurnEndReason::ToolUse),
        }
    }

    fn tool_results() -> Message {
        Message::ToolResults {
            tool_use_results: vec![ToolResult {
                id: "t1".into(),
                content: vec![Content::Text {
                    text: "hi\n".into(),
                }],
                is_error: false,
            }],
        }
    }

    #[test]
    fn select_tail_keeps_last_user_turns_and_tool_loop() {
        let messages = vec![
            user("old"),
            assistant_end("old a"),
            user("mid"),
            assistant_tools(),
            tool_results(),
            assistant_end("mid a"),
            user("new"),
            assistant_end("new a"),
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
        let _guard = crate::session::lock_myco_home_for_test();
        let dir = std::env::temp_dir().join(format!(
            "myco-compact-{}",
            uuid_simple_hex(uuid::Uuid::new_v4())
        ));
        std::fs::create_dir_all(&dir).unwrap();
        unsafe {
            std::env::set_var("MYCO_HOME", &dir);
        }

        let mut pred = Session::new("claude-haiku-4-5");
        pred.messages = vec![user("hello"), assistant_end("world")];
        pred.title = Some("t".into());
        pred.save().unwrap();

        let (succ, out) = compact_session(
            &pred,
            "## Goal\nDo the thing\n",
            "claude-haiku-4-5",
            &CompactOptions::default(),
        )
        .unwrap();
        assert_eq!(out.predecessor_id, pred.id);
        assert_eq!(succ.predecessor_id.as_deref(), Some(pred.id.as_str()));
        assert!(matches!(succ.messages[0], Message::UserMessage { .. }));
        assert!(
            std::fs::read_to_string(pred.summary_path())
                .unwrap()
                .contains("Do the thing")
        );

        let _ = std::fs::remove_dir_all(&dir);
        unsafe {
            std::env::remove_var("MYCO_HOME");
        }
    }
}
