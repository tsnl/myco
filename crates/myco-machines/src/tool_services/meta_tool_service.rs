//! Root-only tool: session metadata + agent-process meta (executable path, pid).
//!
//! Installed on the in-process local worker only (not remotes). Bound to the
//! interactive process's [`ActiveSession`].

use myco_models as generative_model;
use std::sync::Arc;

use myco_api::ToolResult;
use myco_core::Async;
use myco_models;
use myco_session::{
    ActiveSession, Session, format_session_detail, format_session_list_line, list_sessions_filtered,
};

use super::{HostDispatchContext, ToolService};

const TOOL_DESCRIPTION: &str = r#"
Read and update Myco conversation session metadata, and inspect the running agent binary.

Sessions store a title and a markdown scratchpad. Files live at
`~/.myco/session/{shard}/{id}.json`.

Actions (`action` is required):
- get: metadata for the current session (default) or another session via `session_id`
  (id or unique prefix). Always includes the on-disk file path and timestamps.
- list: enumerate sessions (id, created, updated, title, path). Optional `limit`
  (default 20; 0 = all readable sessions). Hidden sessions (subagents, compact
  workers) are omitted unless `include_hidden` is true. Get-by-id always works for
  hidden. Session files are plain JSON — search them with bash (`rg`, `jq`).
- set_title: set the **current** session title. `title` is required: a non-empty string
  sets it, an empty string clears it (omitting `title` is an error, never a clear).
- set_scratchpad: replace the **current** session scratchpad (markdown; size-capped).
  `scratchpad` is required; an empty string clears it.
- executable_path: absolute path of the running `myco` agent binary
  (`std::env::current_exe`). Use with bash (`$path --version`) to read the package
  version when deciding how to update remotes.
- pid: OS process id of the running `myco` agent process (`std::process::id`).
  Use with bash to inspect the live process (`ps -p $pid`, `/proc/$pid`) — e.g.
  memory use or open files — without guessing which `myco` is you (nested agents
  are separate `myco` processes with their own pids).

Use this tool (not bash/editor) for session files. Titles appear in `/sessions`: as soon as
the real task is clear (usually first turn), set_title a short scannable label — replace a
weak auto-title from the first user line. When the session focus shifts, update the
title; do not leave a stale first-line title for long work.
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
            input_schema: super::tool_input_schema::<Input>(),
        }]
    }

    fn dispatch_tool_use(
        self: Arc<Self>,
        tool_use: myco_api::ToolUse,
        _ctx: HostDispatchContext,
    ) -> Async<myco_api::ToolResult> {
        Box::pin(async move {
            let input: Input = match serde_json::from_value(tool_use.input.clone()) {
                Ok(v) => v,
                Err(e) => {
                    return ToolResult::err(format!("invalid session_meta input: {e}"));
                }
            };
            // Off the executor: every action does file IO (session files).
            match tokio::task::spawn_blocking(move || self.execute(input)).await {
                Ok(Ok(text)) => ToolResult::text(text),
                Ok(Err(e)) => ToolResult::err(e),
                Err(e) => ToolResult::err(format!("session_meta join: {e}")),
            }
        })
    }
}

impl SessionMetaTool {
    fn execute(&self, input: Input) -> Result<String, String> {
        match input.action.clone() {
            ActionKind::Get => self.action_get(input.session_id.as_deref()),
            ActionKind::List => {
                self.action_list(input.limit, input.include_hidden.unwrap_or(false))
            }
            ActionKind::SetTitle => self.action_set_title(input.title),
            ActionKind::SetScratchpad => match input.scratchpad {
                Some(text) => self.action_set_scratchpad(text),
                None => Err("set_scratchpad requires `scratchpad` (full markdown; \
                     pass an empty string to clear)"
                    .into()),
            },
            ActionKind::ExecutablePath => self.action_executable_path(),
            ActionKind::Pid => Ok(format!("{}\n", std::process::id())),
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
        let mut list = list_sessions_filtered(0, include_hidden)?;
        if limit > 0 {
            list.truncate(limit);
        }
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
        // A missing/null `title` is an error, not a clear: models that fill
        // every optional key with null must not silently wipe the title.
        let Some(title) = title else {
            return Err("set_title requires `title`: a non-empty string sets it, \
                        an empty string clears it"
                .into());
        };
        self.active.with_mut(|session| {
            if title.trim().is_empty() {
                session.set_title(None)?;
            } else {
                session.set_title(Some(title))?;
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

    fn action_executable_path(&self) -> Result<String, String> {
        let path = std::env::current_exe().map_err(|e| format!("current_exe failed: {e}"))?;
        Ok(format!("{}\n", path.display()))
    }
}

// --- input schema ------------------------------------------------------------

#[derive(Clone, Debug, schemars::JsonSchema, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
struct Input {
    /// Action to perform.
    action: ActionKind,
    /// `get`: another session's id (or unique prefix). Default: current.
    #[serde(default)]
    session_id: Option<String>,
    /// `list`: max sessions returned (default 20; 0 = all).
    #[serde(default)]
    limit: Option<usize>,
    /// `list`: include hidden sessions (subagents, compact workers).
    #[serde(default)]
    include_hidden: Option<bool>,
    /// `set_title`: the new title (empty string clears).
    #[serde(default)]
    title: Option<String>,
    /// `set_scratchpad`: full replacement markdown (empty string clears).
    #[serde(default)]
    scratchpad: Option<String>,
}

#[derive(Clone, Debug, schemars::JsonSchema, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
enum ActionKind {
    Get,
    List,
    SetTitle,
    SetScratchpad,
    ExecutablePath,
    Pid,
}

#[cfg(test)]
mod tests {
    use super::*;
    use myco_test_support::{result_text, temp_home};

    fn tool() -> (Arc<SessionMetaTool>, ActiveSession) {
        let active = ActiveSession::new(Session::new("m"));
        (Arc::new(SessionMetaTool::new(active.clone())), active)
    }

    async fn call(tool: &Arc<SessionMetaTool>, input: serde_json::Value) -> ToolResult {
        tool.clone()
            .dispatch_tool_use(
                myco_api::ToolUse {
                    id: "t".into(),
                    name: "session_meta".into(),
                    input,
                },
                HostDispatchContext::new(uuid::Uuid::nil(), myco_core::CancelToken::new()),
            )
            .await
    }

    #[tokio::test]
    async fn title_and_scratchpad_roundtrip_through_the_tool() {
        let _home = temp_home("meta-tool");
        let (tool, active) = tool();

        let r = call(
            &tool,
            serde_json::json!({"action": "set_title", "title": "port the harness"}),
        )
        .await;
        assert!(!r.is_error, "{}", result_text(&r));
        assert_eq!(active.snapshot().title.as_deref(), Some("port the harness"));

        let r = call(
            &tool,
            serde_json::json!({"action": "set_scratchpad", "scratchpad": "## notes\n"}),
        )
        .await;
        assert!(!r.is_error, "{}", result_text(&r));
        assert_eq!(active.snapshot().scratchpad, "## notes\n");

        // A null title is an error, never a silent clear.
        let r = call(&tool, serde_json::json!({"action": "set_title"})).await;
        assert!(r.is_error);
        assert_eq!(active.snapshot().title.as_deref(), Some("port the harness"));

        let r = call(&tool, serde_json::json!({"action": "get"})).await;
        assert!(!r.is_error);
        assert!(result_text(&r).contains("port the harness"));
    }
}
