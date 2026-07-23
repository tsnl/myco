//! Host tool service: list and rank recent sessions by activity.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use chrono::{DateTime, Utc};
use serde_json::json;

use crate::core::Async;
use crate::generative_model::{self, ToolResult};
use crate::session::{self, Session, SessionKind};

use super::{HostDispatchContext, ToolService};

const TOOL_DESCRIPTION: &str = r#"
List and rank recent sessions by activity (modification time + message count).
Extract statistics, search by keywords, and cross-reference with work items.

Actions:
- list (default): rank recent sessions by activity score (mtime + message count).
  Returns id, title, message count, model, and time span for each session.
- search: filter sessions by keyword in title or recent messages (case-insensitive).
- stats: detailed statistics for a single session (message types, tool uses, models).

Typical workflow:
1. Use `list` to see recent sessions ranked by activity.
2. Use `search` with a keyword to find related work.
3. Use `stats` on a specific session to audit execution and resource usage.

Session ranking combines recency (mtime) and conversation depth (message count)
into an activity score — active, deep sessions bubble to the top.
"#;

/// List and rank sessions by activity. Implements [`ToolService`].
#[derive(Default)]
pub struct ListRecentService;

impl ListRecentService {
    pub fn new() -> Self {
        Self
    }

    fn session_dir() -> PathBuf {
        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("~"));
        home.join(".myco/session")
    }

    fn read_sessions() -> Result<Vec<(PathBuf, Session)>, String> {
        let dir = Self::session_dir();
        if !dir.exists() {
            return Ok(Vec::new());
        }

        let mut sessions = Vec::new();

        for entry in fs::read_dir(&dir).map_err(|e| format!("failed to read session dir: {e}"))? {
            let entry = entry.map_err(|e| format!("read_dir entry: {e}"))?;
            let path = entry.path();

            if !path.is_dir() {
                continue;
            }

            for sub_entry in fs::read_dir(&path)
                .map_err(|e| format!("failed to read shard dir: {e}"))? {
                let sub_entry =
                    sub_entry.map_err(|e| format!("read_dir sub-entry: {e}"))?;
                let file_path = sub_entry.path();

                if file_path.extension().and_then(|s| s.to_str()) != Some("json") {
                    continue;
                }

                match fs::read_to_string(&file_path) {
                    Ok(content) => {
                        match serde_json::from_str::<Session>(&content) {
                            Ok(session) => {
                                sessions.push((file_path, session));
                            }
                            Err(_) => {
                                // Skip malformed session files
                            }
                        }
                    }
                    Err(_) => {
                        // Skip unreadable files
                    }
                }
            }
        }

        Ok(sessions)
    }

    fn mtime_for_path(path: &Path) -> Option<u64> {
        fs::metadata(path)
            .ok()?
            .modified()
            .ok()?
            .duration_since(SystemTime::UNIX_EPOCH)
            .ok()
            .map(|d| d.as_secs())
    }

    fn activity_score(mtime_secs: u64, message_count: usize) -> f64 {
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        let age_hours = (now.saturating_sub(mtime_secs)) as f64 / 3600.0;
        let recency_score = (-(age_hours / 24.0)).exp(); // decays over days

        let depth_score = (message_count as f64).ln() + 1.0; // log of message count

        recency_score * 0.7 + (depth_score / 10.0) * 0.3
    }

    fn format_duration(start: DateTime<Utc>, end: DateTime<Utc>) -> String {
        let duration = end
            .signed_duration_since(start)
            .to_std()
            .unwrap_or_default();
        let hours = duration.as_secs() / 3600;
        let minutes = (duration.as_secs() % 3600) / 60;

        if hours > 0 {
            format!("{}h {}m", hours, minutes)
        } else if minutes > 0 {
            format!("{}m", minutes)
        } else {
            "< 1m".to_string()
        }
    }

    fn execute(&self, input: Input) -> Result<String, String> {
        let action = input.action.unwrap_or(ActionKind::List);
        match action {
            ActionKind::List => self.execute_list(input.limit),
            ActionKind::Search => {
                let keyword = input
                    .keyword
                    .ok_or_else(|| "search requires keyword parameter".to_string())?;
                self.execute_search(&keyword)
            }
            ActionKind::Stats => {
                let id = input
                    .id
                    .ok_or_else(|| "stats requires id parameter".to_string())?;
                self.execute_stats(&id)
            }
        }
    }

    fn execute_list(&self, limit: Option<usize>) -> Result<String, String> {
        let limit = limit.unwrap_or(10);
        let mut sessions = Self::read_sessions()?;

        // Sort by activity score (descending)
        sessions.sort_by(|(path_a, session_a), (path_b, session_b)| {
            let mtime_a = Self::mtime_for_path(path_a).unwrap_or(0);
            let mtime_b = Self::mtime_for_path(path_b).unwrap_or(0);
            let score_a = Self::activity_score(mtime_a, session_a.messages.len());
            let score_b = Self::activity_score(mtime_b, session_b.messages.len());
            score_b.partial_cmp(&score_a).unwrap_or(std::cmp::Ordering::Equal)
        });

        let mut output = String::from("# Recent Sessions\n\n");
        output.push_str("| ID | Title | Msgs | Model | Duration | Kind |\n");
        output.push_str("|---|---|---|---|---|---|\n");

        for (_, session) in sessions.iter().take(limit) {
            let id = session.id.chars().take(8).collect::<String>();
            let title = session
                .title
                .as_deref()
                .unwrap_or("(untitled)")
                .chars()
                .take(40)
                .collect::<String>();
            let msg_count = session.messages.len();
            let duration = Self::format_duration(session.created_at, session.updated_at);
            let kind = if session.kind.is_user() {
                ""
            } else {
                &format!(" ({})", session.kind)
            };

            output.push_str(&format!(
                "| `{}` | {} | {} | {} | {} |{}|\n",
                id, title, msg_count, session.model, duration, kind
            ));
        }

        Ok(output)
    }

    fn execute_search(&self, keyword: &str) -> Result<String, String> {
        let sessions = Self::read_sessions()?;
        let keyword_lower = keyword.to_lowercase();

        let mut matches = Vec::new();

        for (_, session) in sessions {
            let title_match = session
                .title
                .as_ref()
                .map(|t| t.to_lowercase().contains(&keyword_lower))
                .unwrap_or(false);

            let recent_text = session
                .messages
                .iter()
                .rev()
                .take(5)
                .map(|m| m.to_string())
                .collect::<String>()
                .to_lowercase();
            let text_match = recent_text.contains(&keyword_lower);

            if title_match || text_match {
                matches.push(session);
            }
        }

        if matches.is_empty() {
            return Ok(format!("No sessions found matching '{}'", keyword));
        }

        let mut output = format!("# Sessions matching '{}'\n\n", keyword);
        output.push_str("| ID | Title | Msgs |\n");
        output.push_str("|---|---|---|\n");

        for session in matches {
            let id = session.id.chars().take(8).collect::<String>();
            let title = session
                .title
                .as_deref()
                .unwrap_or("(untitled)")
                .chars()
                .take(40)
                .collect::<String>();
            let msg_count = session.messages.len();

            output.push_str(&format!("| `{}` | {} | {} |\n", id, title, msg_count));
        }

        Ok(output)
    }

    fn execute_stats(&self, id: &str) -> Result<String, String> {
        let sessions = Self::read_sessions()?;

        let (_, session) = sessions
            .into_iter()
            .find(|(_, s)| s.id.starts_with(id))
            .ok_or_else(|| format!("session '{}' not found", id))?;

        let mut output = String::new();
        output.push_str(&format!("# Session: {}\n\n", session.id.chars().take(8).collect::<String>()));

        if let Some(title) = &session.title {
            output.push_str(&format!("**Title:** {}\n\n", title));
        }

        output.push_str(&format!("**Model:** {}\n", session.model));
        output.push_str(&format!("**Created:** {}\n", session.created_at));
        output.push_str(&format!("**Updated:** {}\n", session.updated_at));
        output.push_str(&format!("**Duration:** {}\n\n", Self::format_duration(session.created_at, session.updated_at)));

        // Message breakdown
        let user_msgs = session.messages.iter().filter(|m| m.role == "user").count();
        let assistant_msgs = session.messages.iter().filter(|m| m.role == "assistant").count();
        let tool_results = session.messages.iter().filter(|m| m.role == "tool").count();

        output.push_str(&format!(
            "**Messages:** {} total (user: {}, assistant: {}, tool: {})\n\n",
            session.messages.len(),
            user_msgs,
            assistant_msgs,
            tool_results
        ));

        // Tool usage count
        let tool_uses = session.messages
            .iter()
            .flat_map(|m| m.content.iter())
            .filter(|c| c.get("type").and_then(|t| t.as_str()) == Some("tool_use"))
            .count();

        output.push_str(&format!("**Tool uses:** {}\n", tool_uses));

        Ok(output)
    }
}

impl ListRecentService {
    /// Tool schemas served by this service (static: no instance required).
    pub fn specs() -> Vec<generative_model::ToolSpec> {
        vec![generative_model::ToolSpec {
            name: "list_recent".to_string(),
            description: TOOL_DESCRIPTION.to_string(),
            input_schema: super::tool_input_schema::<Input>(),
        }]
    }
}

impl ToolService for ListRecentService {
    fn tool_specs(&self) -> Vec<generative_model::ToolSpec> {
        Self::specs()
    }

    fn dispatch_tool_use(
        self: Arc<Self>,
        tool_use: generative_model::ToolUse,
        _ctx: HostDispatchContext,
    ) -> Async<generative_model::ToolResult> {
        Box::pin(async move {
            let input: Input = match serde_json::from_value(tool_use.input.clone()) {
                Ok(v) => v,
                Err(e) => {
                    return ToolResult::err(format!("invalid list_recent input: {e}"));
                }
            };
            match self.execute(input) {
                Ok(text) => ToolResult::text(text),
                Err(e) => ToolResult::err(e),
            }
        })
    }
}

#[derive(Clone, Debug, schemars::JsonSchema, serde::Deserialize, serde::Serialize)]
struct Input {
    /// Action to perform. Defaults to `list`.
    #[serde(default)]
    action: Option<ActionKind>,
    /// Maximum sessions to return for `list`. Defaults to 10.
    #[serde(default)]
    limit: Option<usize>,
    /// Keyword to search for in `search` action.
    #[serde(default)]
    keyword: Option<String>,
    /// Session id (or prefix) for `stats` action.
    #[serde(default)]
    id: Option<String>,
}

#[derive(Clone, Debug, schemars::JsonSchema, serde::Deserialize, serde::Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum ActionKind {
    List,
    Search,
    Stats,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CancelToken;
    use crate::generative_model::ToolUse;
    use serde_json::json;

    fn tool_text(r: &generative_model::ToolResult) -> String {
        r.content
            .iter()
            .filter_map(|c| match c {
                crate::generative_model::Content::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }

    fn ctx() -> HostDispatchContext {
        HostDispatchContext {
            agent_id: uuid::Uuid::nil(),
            cancel: CancelToken::new(),
            agent_root: None,
        }
    }

    #[tokio::test]
    async fn list_recent_tool() {
        let tool = Arc::new(ListRecentService::new());
        let res = tool
            .clone()
            .dispatch_tool_use(
                ToolUse {
                    id: "t1".into(),
                    name: "list_recent".into(),
                    input: json!({"action": "list", "limit": 5}),
                },
                ctx(),
            )
            .await;
        assert!(!res.is_error, "{res:?}");
        let text = tool_text(&res);
        assert!(text.contains("Recent Sessions") || text.is_empty(), "{text}");
    }
}
