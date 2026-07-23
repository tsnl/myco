//! Root-only tool: recent sessions ranked by activity.
//!
//! Complements `session_meta` (enumeration, content search via `query`,
//! links) and `session_history` (drill into one transcript): this view
//! answers "what was I actually working on?" — a deep active session
//! outranks a just-touched stub.

use std::sync::Arc;

use crate::core::Async;
use crate::generative_model::{self, ToolResult, ToolUse};
use crate::session::{SessionListEntry, format_session_list_line, list_sessions_filtered};

use super::{HostDispatchContext, ToolService};

const DEFAULT_LIMIT: usize = 10;

const TOOL_DESCRIPTION: &str = r#"
Recent sessions ranked by **activity** — a blend of update recency and message
count, so a deep active session outranks a just-touched stub. Each line shows
id, updated time, model, message count, PR/worktree link counts, and a
one-liner (title or first user text).

Options: `limit` (default 10, 0 = all), `include_hidden` (subagent/compact
sessions; default false).

For content search across sessions use `session_meta` list with `query`; to
drill into one session's transcript use `session_history`.
"#;

/// Activity-ranked session listing. Root-only [`ToolService`]: installed
/// beside `session_meta` / `session_history` on the in-process local worker,
/// never on remotes (sessions live on the interactive machine).
#[derive(Default)]
pub struct ListRecentService;

impl ListRecentService {
    pub fn new() -> Self {
        Self
    }

    fn execute(&self, input: Input) -> Result<String, String> {
        let limit = input.limit.unwrap_or(DEFAULT_LIMIT);
        let include_hidden = input.include_hidden.unwrap_or(false);

        // Rank over the full store, then truncate — otherwise a deep-but-older
        // session could be cut before ranking ever sees it.
        let entries = list_sessions_filtered(0, include_hidden)?;
        if entries.is_empty() {
            return Ok("no sessions found".to_string());
        }
        let order = activity_order(&entries);
        let shown = if limit > 0 {
            order.len().min(limit)
        } else {
            order.len()
        };

        let mut out = format!(
            "sessions: {} (showing {}, ranked by activity = recency + depth)\n",
            entries.len(),
            shown
        );
        for (row, &i) in order.iter().take(shown).enumerate() {
            out.push_str(&format_session_list_line(row + 1, &entries[i]));
            out.push('\n');
        }
        Ok(out)
    }
}

impl ToolService for ListRecentService {
    fn tool_specs(&self) -> Vec<generative_model::ToolSpec> {
        vec![generative_model::ToolSpec {
            name: "list_recent".to_string(),
            description: TOOL_DESCRIPTION.to_string(),
            input_schema: super::tool_input_schema::<Input>(),
        }]
    }

    fn dispatch_tool_use(
        self: Arc<Self>,
        tool_use: ToolUse,
        _ctx: HostDispatchContext,
    ) -> Async<ToolResult> {
        Box::pin(async move {
            let input: Input = match serde_json::from_value(tool_use.input.clone()) {
                Ok(v) => v,
                Err(e) => return ToolResult::err(format!("invalid list_recent input: {e}")),
            };
            match self.execute(input) {
                Ok(text) => ToolResult::text(text),
                Err(e) => ToolResult::err(e),
            }
        })
    }
}

/// Blend recency and depth without magic constants: each session's score is
/// its rank by `updated_at` plus its rank by `message_count` (lower = more
/// active). Ties break toward the more recently updated session. Returns
/// indices into `entries` in display order.
fn activity_order(entries: &[SessionListEntry]) -> Vec<usize> {
    let mut score = vec![0usize; entries.len()];

    let mut by_recency: Vec<usize> = (0..entries.len()).collect();
    by_recency.sort_by_key(|&i| std::cmp::Reverse(entries[i].updated_at));
    for (rank, &i) in by_recency.iter().enumerate() {
        score[i] += rank;
    }

    let mut by_depth: Vec<usize> = (0..entries.len()).collect();
    by_depth.sort_by_key(|&i| std::cmp::Reverse(entries[i].message_count));
    for (rank, &i) in by_depth.iter().enumerate() {
        score[i] += rank;
    }

    let mut order: Vec<usize> = (0..entries.len()).collect();
    order.sort_by_key(|&i| (score[i], std::cmp::Reverse(entries[i].updated_at)));
    order
}

#[derive(Clone, Debug, schemars::JsonSchema, serde::Deserialize, serde::Serialize)]
struct Input {
    /// Max sessions to return (default 10; 0 = all).
    #[serde(default)]
    limit: Option<usize>,
    /// Include hidden sessions (subagents, compact workers). Default false.
    #[serde(default)]
    include_hidden: Option<bool>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CancelToken;
    use crate::generative_model::{Content, Message};
    use crate::session::{
        LinkCounts, Session, SessionKind, lock_myco_home_for_test, uuid_simple_hex,
    };
    use chrono::{Duration, Utc};
    use serde_json::json;

    fn entry(id: &str, updated_minutes_ago: i64, message_count: usize) -> SessionListEntry {
        let updated_at = Utc::now() - Duration::minutes(updated_minutes_ago);
        SessionListEntry {
            id: id.to_string(),
            path: std::path::PathBuf::from(format!("/dev/null/{id}")),
            created_at: updated_at,
            updated_at,
            model: "m".into(),
            message_count,
            title: None,
            snippet: String::new(),
            link_counts: LinkCounts::default(),
            kind: SessionKind::User,
            parent_session_id: None,
        }
    }

    #[test]
    fn activity_blend_prefers_deep_recent_over_fresh_stub() {
        // A: brand-new 1-message stub; B: slightly older but deep; C: old + shallow.
        let entries = vec![
            entry("aaaa", 0, 1),
            entry("bbbb", 10, 100),
            entry("cccc", 60, 50),
        ];
        let order = activity_order(&entries);
        let ids: Vec<&str> = order.iter().map(|&i| entries[i].id.as_str()).collect();
        assert_eq!(ids, ["bbbb", "aaaa", "cccc"], "{ids:?}");
    }

    #[test]
    fn activity_blend_breaks_ties_by_recency() {
        let entries = vec![entry("older", 30, 5), entry("newer", 5, 5)];
        let order = activity_order(&entries);
        assert_eq!(entries[order[0]].id, "newer");
    }

    fn tool_text(r: &ToolResult) -> String {
        r.content
            .iter()
            .filter_map(|c| match c {
                Content::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }

    fn saved_session(model: &str, title: &str, user_text: &str) -> Session {
        let mut s = Session::new(model);
        s.title = Some(title.to_string());
        s.messages.push(Message::UserMessage {
            content: vec![Content::Text {
                text: user_text.to_string(),
            }],
        });
        s.save().unwrap();
        s
    }

    #[tokio::test]
    async fn lists_ranked_sessions_from_real_store() {
        let _guard = lock_myco_home_for_test();
        let dir = std::env::temp_dir().join(format!(
            "myco-list-recent-{}",
            uuid_simple_hex(uuid::Uuid::new_v4())
        ));
        std::fs::create_dir_all(&dir).unwrap();
        // SAFETY: test-only env override; held under the myco-home lock.
        unsafe {
            std::env::set_var("MYCO_HOME", &dir);
        }

        let first = saved_session("m1", "host cancel work", "fix host-side cancel");
        let second = saved_session("m2", "docs pass", "tidy the readme");

        let tool = Arc::new(ListRecentService::new());
        let list = tool
            .dispatch_tool_use(
                ToolUse {
                    id: "t1".into(),
                    name: "list_recent".into(),
                    input: json!({}),
                },
                HostDispatchContext::new(uuid::Uuid::nil(), CancelToken::new()),
            )
            .await;
        assert!(!list.is_error, "{list:?}");
        let text = tool_text(&list);
        assert!(text.contains(&first.id), "{text}");
        assert!(text.contains(&second.id), "{text}");
        assert!(text.contains("ranked by activity"), "{text}");

        // SAFETY: test-only env cleanup; still under the myco-home lock.
        unsafe {
            std::env::remove_var("MYCO_HOME");
        }
    }
}
