//! Root-only tool: session metadata + agent-process meta (executable path).
//!
//! Installed on the in-process local worker only (not remotes). Bound to the
//! interactive process's [`ActiveSession`].

use std::sync::Arc;

use crate::core::Async;
use crate::generative_model::{self, ToolResult};
use crate::session::{
    ActiveSession, Session, SessionLink, format_link_one_line, format_session_detail,
    format_session_list_line, list_all_sessions, list_all_sessions_including_hidden,
    list_sessions_filtered, normalize_pr_url, parse_pr_fields,
};

use super::{HostDispatchContext, ToolService};

const TOOL_DESCRIPTION: &str = r#"
Read and update Myco conversation session metadata, and inspect the running agent binary.

Sessions store a title, structured links (GitHub PRs across repos, worktree paths on any
host), and a markdown scratchpad. Files live at `~/.myco/session/{shard}/{id}.json`.

Actions:
- get: metadata for the current session (default) or another session via `session_id`
  (id or unique prefix). Always includes the on-disk file path and timestamps.
- list: enumerate sessions (id, created, updated, title, link counts, path). Optional
  `limit` (default 20; 0 = all readable sessions). Hidden sessions (subagents, compact
  workers) are omitted unless `include_hidden` is true. Get-by-id always works for hidden.
- set_title: set or clear (`title` null/empty clears) the **current** session title.
- set_scratchpad: replace the **current** session scratchpad (markdown; size-capped).
- add_link: attach a GitHub PR or worktree to the **current** session (deduped).
- remove_link: drop a link from the **current** session by `index`, or by `url` /
  `host`+`path`.
- executable_path: absolute path of the running `myco` agent binary
  (`std::env::current_exe`). Use with bash (`$path --version`) to read the package
  version when deciding how to update remotes (see manual `harness-ops`).

Use this tool (not bash/editor) for session files. Titles appear in `/sessions`: as soon as
the real task is clear (usually first turn), set_title a short scannable label — replace a
weak CLI auto-title from the first user line. When the session focus shifts, update the
title; do not leave a stale first-line title for long work. When you create a worktree or
open/receive a PR, add_link it (absolute path + host for worktrees).
"#;

/// Local tool bound to the interactive process's [`ActiveSession`].
pub struct SessionMetaTool {
    active: ActiveSession,
}

impl SessionMetaTool {
    pub fn new(active: ActiveSession) -> Self {
        Self { active }
    }
}

impl ToolService for SessionMetaTool {
    fn tool_specs(&self) -> Vec<generative_model::ToolSpec> {
        vec![generative_model::ToolSpec {
            name: "session_meta".to_string(),
            description: TOOL_DESCRIPTION.to_string(),
            input_schema: schemars::schema_for!(Input).to_value(),
        }]
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
                    return ToolResult::err(format!("invalid session_meta input: {e}"));
                }
            };
            match self.execute(input) {
                Ok(text) => ToolResult::text(text),
                Err(e) => ToolResult::err(e),
            }
        })
    }
}

impl SessionMetaTool {
    fn execute(&self, input: Input) -> Result<String, String> {
        let action = input.action.clone().unwrap_or(ActionKind::Get);
        match action {
            ActionKind::Get => self.action_get(input.session_id.as_deref()),
            ActionKind::List => {
                self.action_list(input.limit, input.include_hidden.unwrap_or(false))
            }
            ActionKind::SetTitle => self.action_set_title(input.title),
            ActionKind::SetScratchpad => {
                let text = input.scratchpad.unwrap_or_default();
                self.action_set_scratchpad(text)
            }
            ActionKind::AddLink => self.action_add_link(input),
            ActionKind::RemoveLink => self.action_remove_link(input),
            ActionKind::ExecutablePath => self.action_executable_path(),
        }
    }

    fn action_get(&self, session_id: Option<&str>) -> Result<String, String> {
        let session = match session_id {
            None => self.active.snapshot(),
            Some(id) => Session::load_by_id_or_prefix(id)?,
        };
        // For current session, path/title reflect in-memory state (may be newer than disk).
        Ok(format_session_detail(&session))
    }

    fn action_list(&self, limit: Option<usize>, include_hidden: bool) -> Result<String, String> {
        let limit = limit.unwrap_or(20);
        let list = if limit == 0 {
            if include_hidden {
                list_all_sessions_including_hidden()?
            } else {
                list_all_sessions()?
            }
        } else {
            list_sessions_filtered(limit, include_hidden)?
        };
        if list.is_empty() {
            return Ok("(no sessions)\n".into());
        }
        let mut out = format!("sessions: {}\n", list.len());
        for (i, entry) in list.iter().enumerate() {
            out.push_str(&format_session_list_line(i + 1, entry));
            out.push('\n');
            out.push_str(&format!("      path={}\n", entry.path.display()));
            out.push_str(&format!(
                "      created={}  updated={}\n",
                entry.created_at.to_rfc3339(),
                entry.updated_at.to_rfc3339()
            ));
        }
        Ok(out)
    }

    fn action_set_title(&self, title: Option<String>) -> Result<String, String> {
        self.active.with_mut(|session| {
            let cleared = title.as_ref().map(|t| t.trim().is_empty()).unwrap_or(true);
            if cleared {
                session.set_title(None)?;
            } else {
                session.set_title(title)?;
            }
            session.touch();
            session.save()?;
            Ok(format!(
                "title set to {}\npath={}\n",
                session
                    .title
                    .as_deref()
                    .map(|t| format!("{t:?}"))
                    .unwrap_or_else(|| "(none)".into()),
                session.json_path().display()
            ))
        })
    }

    fn action_set_scratchpad(&self, text: String) -> Result<String, String> {
        self.active.with_mut(|session| {
            session.set_scratchpad(text)?;
            session.touch();
            session.save()?;
            Ok(format!(
                "scratchpad updated ({} bytes)\npath={}\n",
                session.scratchpad.len(),
                session.json_path().display()
            ))
        })
    }

    fn action_add_link(&self, input: Input) -> Result<String, String> {
        let kind = input
            .link_kind
            .ok_or_else(|| "add_link requires link_kind (github_pr | worktree)".to_string())?;
        let link = match kind {
            LinkKind::GithubPr => {
                let url_raw = input
                    .url
                    .as_deref()
                    .ok_or_else(|| "add_link github_pr requires url".to_string())?;
                let url = normalize_pr_url(url_raw)?;
                let (repo, number) = parse_pr_fields(&url);
                SessionLink::GitHubPr {
                    url,
                    repo: input.repo.or(repo),
                    number: input.number.or(number),
                    note: input.note,
                }
            }
            LinkKind::Worktree => {
                let host = input
                    .host
                    .filter(|h| !h.trim().is_empty())
                    .ok_or_else(|| "add_link worktree requires host".to_string())?;
                let path = input
                    .path
                    .filter(|p| !p.trim().is_empty())
                    .ok_or_else(|| "add_link worktree requires path".to_string())?;
                SessionLink::Worktree {
                    host: host.trim().to_string(),
                    path: path.trim().to_string(),
                    branch: input.branch,
                    note: input.note,
                }
            }
        };

        self.active.with_mut(|session| {
            session.upsert_link(link.clone())?;
            session.touch();
            session.save()?;
            Ok(format!(
                "link upserted: {}\nlinks={}\npath={}\n",
                format_link_one_line(&link),
                session.links.len(),
                session.json_path().display()
            ))
        })
    }

    fn action_remove_link(&self, input: Input) -> Result<String, String> {
        self.active.with_mut(|session| {
            let removed = if let Some(index) = input.index {
                session.remove_link_at(index)?
            } else if input.url.is_some() || input.host.is_some() {
                session.remove_link_matching(
                    input.url.as_deref(),
                    input.host.as_deref(),
                    input.path.as_deref(),
                )?
            } else {
                return Err("remove_link requires index, or url, or host (+ optional path)".into());
            };
            session.touch();
            session.save()?;
            Ok(format!(
                "removed: {}\nlinks remaining={}\npath={}\n",
                format_link_one_line(&removed),
                session.links.len(),
                session.json_path().display()
            ))
        })
    }

    fn action_executable_path(&self) -> Result<String, String> {
        let path = std::env::current_exe().map_err(|e| format!("current_exe failed: {e}"))?;
        Ok(format!("{}\n", path.display()))
    }
}

// --- input schema ------------------------------------------------------------

#[derive(
    Clone, Debug, schemars::JsonSchema, serde::Deserialize, serde::Serialize, PartialEq, Eq,
)]
struct Input {
    /// Action to perform. Defaults to `get`.
    #[serde(default)]
    action: Option<ActionKind>,
    /// Target session id or unique prefix for `get`. Omit for the current session.
    #[serde(default)]
    session_id: Option<String>,
    /// Max sessions for `list` (default 20; 0 = all).
    #[serde(default)]
    limit: Option<usize>,
    /// When true, `list` includes hidden sessions (subagents, compact workers).
    #[serde(default)]
    include_hidden: Option<bool>,
    /// New title for `set_title`. Empty/null clears.
    #[serde(default)]
    title: Option<String>,
    /// Full scratchpad markdown for `set_scratchpad`.
    #[serde(default)]
    scratchpad: Option<String>,
    /// Link type for `add_link`.
    #[serde(default)]
    link_kind: Option<LinkKind>,
    /// GitHub PR URL or `org/repo#N` for `add_link` / `remove_link`.
    #[serde(default)]
    url: Option<String>,
    /// Optional org/repo for PR links.
    #[serde(default)]
    repo: Option<String>,
    /// Optional PR number.
    #[serde(default)]
    number: Option<u32>,
    /// Host name for worktree links (`local`, `devbox`, …).
    #[serde(default)]
    host: Option<String>,
    /// Absolute worktree path on `host`.
    #[serde(default)]
    path: Option<String>,
    /// Optional branch name for worktree links.
    #[serde(default)]
    branch: Option<String>,
    /// Optional free-form note on a link.
    #[serde(default)]
    note: Option<String>,
    /// Link index (from `get`) for `remove_link`.
    #[serde(default)]
    index: Option<usize>,
}

#[derive(
    Clone, Debug, schemars::JsonSchema, serde::Deserialize, serde::Serialize, PartialEq, Eq,
)]
#[serde(rename_all = "snake_case")]
enum ActionKind {
    Get,
    List,
    SetTitle,
    SetScratchpad,
    AddLink,
    RemoveLink,
    ExecutablePath,
}

#[derive(
    Clone, Debug, schemars::JsonSchema, serde::Deserialize, serde::Serialize, PartialEq, Eq,
)]
#[serde(rename_all = "snake_case")]
enum LinkKind {
    GithubPr,
    Worktree,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CancelToken;
    use crate::tool_services::{HostDispatchContext, ToolService};
    use std::sync::Arc;

    fn tool_with_session(session: Session) -> (SessionMetaTool, ActiveSession) {
        let active = ActiveSession::new(session);
        (SessionMetaTool::new(active.clone()), active)
    }

    #[tokio::test]
    async fn set_title_and_get() {
        let dir = std::env::temp_dir().join(format!(
            "myco-meta-tool-{}",
            crate::session::uuid_simple_hex(uuid::Uuid::new_v4())
        ));
        std::fs::create_dir_all(&dir).unwrap();
        // SAFETY: test-only env override.
        unsafe {
            std::env::set_var("MYCO_HOME", &dir);
        }

        let (tool, active) = tool_with_session(Session::new("claude-haiku-4-5"));
        let tool = Arc::new(tool);
        let result = tool
            .clone()
            .dispatch_tool_use(
                generative_model::ToolUse {
                    id: "t1".into(),
                    name: "session_meta".into(),
                    input: serde_json::json!({
                        "action": "set_title",
                        "title": "  My Feature  "
                    }),
                },
                HostDispatchContext::bare(uuid::Uuid::nil(), CancelToken::new()),
            )
            .await;
        assert!(!result.is_error, "{result:?}");
        assert_eq!(active.snapshot().title.as_deref(), Some("My Feature"));

        let got = tool
            .dispatch_tool_use(
                generative_model::ToolUse {
                    id: "t2".into(),
                    name: "session_meta".into(),
                    input: serde_json::json!({"action": "get"}),
                },
                HostDispatchContext::bare(uuid::Uuid::nil(), CancelToken::new()),
            )
            .await;
        assert!(!got.is_error, "{got:?}");
        let text = got
            .content
            .iter()
            .filter_map(|c| match c {
                generative_model::Content::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<String>();
        assert!(text.contains("My Feature"), "{text}");

        let _ = std::fs::remove_dir_all(&dir);
        unsafe {
            std::env::remove_var("MYCO_HOME");
        }
    }

    #[tokio::test]
    async fn executable_path_returns_absolute_path() {
        let dir = std::env::temp_dir().join(format!(
            "myco-meta-exe-{}",
            crate::session::uuid_simple_hex(uuid::Uuid::new_v4())
        ));
        std::fs::create_dir_all(&dir).unwrap();
        unsafe {
            std::env::set_var("MYCO_HOME", &dir);
        }

        let (tool, _) = tool_with_session(Session::new("claude-haiku-4-5"));
        let tool = Arc::new(tool);
        let got = tool
            .dispatch_tool_use(
                generative_model::ToolUse {
                    id: "t1".into(),
                    name: "session_meta".into(),
                    input: serde_json::json!({"action": "executable_path"}),
                },
                HostDispatchContext::bare(uuid::Uuid::nil(), CancelToken::new()),
            )
            .await;
        assert!(!got.is_error, "{got:?}");
        let text = got
            .content
            .iter()
            .filter_map(|c| match c {
                generative_model::Content::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<String>();
        let path = text.trim();
        assert!(!path.is_empty(), "{text}");
        assert!(
            std::path::Path::new(path).is_absolute(),
            "expected absolute path, got {path:?}"
        );

        let _ = std::fs::remove_dir_all(&dir);
        unsafe {
            std::env::remove_var("MYCO_HOME");
        }
    }
}
