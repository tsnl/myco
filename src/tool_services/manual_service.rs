//! Host tool service: browse compile-time manual articles ([`crate::manual`]).

use std::sync::Arc;

use crate::core::Async;
use crate::generative_model::{self, ToolResult};
use crate::manual::{self, format_article, format_catalog};

use super::{HostDispatchContext, ToolService};

const TOOL_DESCRIPTION: &str = r#"
Browse Myco runtime manual articles (overview, CLI, tools, harness-ops). Articles are
embedded at compile time (same text as `myco --help <id>`). Worktrees, computer-use, and
coding norms live in the always-on system prompt (not this catalog).

Actions:
- list (default): catalog of article ids + one-line summaries.
- get: full markdown body for `id` (e.g. overview, harness-ops).

Prefer this tool over guessing when host/install/config behavior is unclear.
"#;

/// Compile-time manual browser. Implements [`ToolService`] (host-placed).
#[derive(Default)]
pub struct ManualService;

impl ManualService {
    pub fn new() -> Self {
        Self
    }

    fn execute(&self, input: Input) -> Result<String, String> {
        let action = input.action.unwrap_or(ActionKind::List);
        match action {
            ActionKind::List => Ok(format_catalog()),
            ActionKind::Get => {
                let id = input
                    .id
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| format!("get requires id; known: {}", manual::known_ids()))?;
                format_article(id)
            }
        }
    }
}

impl ToolService for ManualService {
    fn tool_specs(&self) -> Vec<generative_model::ToolSpec> {
        vec![generative_model::ToolSpec {
            name: "manual".to_string(),
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
                    return ToolResult::err(format!("invalid manual input: {e}"));
                }
            };
            match self.execute(input) {
                Ok(text) => ToolResult::text(text),
                Err(e) => ToolResult::err(e),
            }
        })
    }
}

#[derive(
    Clone, Debug, schemars::JsonSchema, serde::Deserialize, serde::Serialize, PartialEq, Eq,
)]
struct Input {
    /// Action to perform. Defaults to `list`.
    #[serde(default)]
    action: Option<ActionKind>,
    /// Article id for `get` (see `list` output).
    #[serde(default)]
    id: Option<String>,
}

#[derive(
    Clone, Debug, schemars::JsonSchema, serde::Deserialize, serde::Serialize, PartialEq, Eq,
)]
#[serde(rename_all = "snake_case")]
enum ActionKind {
    List,
    Get,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CancelToken;
    use crate::generative_model::{Content, ToolUse};
    use serde_json::json;
    use std::sync::Arc;

    fn tool_text(r: &generative_model::ToolResult) -> String {
        r.content
            .iter()
            .filter_map(|c| match c {
                Content::Text { text } => Some(text.as_str()),
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
    async fn list_and_get() {
        let tool = Arc::new(ManualService::new());
        let list = tool
            .clone()
            .dispatch_tool_use(
                ToolUse {
                    id: "t1".into(),
                    name: "manual".into(),
                    input: json!({"action": "list"}),
                },
                ctx(),
            )
            .await;
        assert!(!list.is_error, "{list:?}");
        let list_text = tool_text(&list);
        assert!(list_text.contains("harness-ops"), "{list_text}");

        let get = tool
            .dispatch_tool_use(
                ToolUse {
                    id: "t2".into(),
                    name: "manual".into(),
                    input: json!({"action": "get", "id": "harness-ops"}),
                },
                ctx(),
            )
            .await;
        assert!(!get.is_error, "{get:?}");
        let body = tool_text(&get);
        assert!(body.contains("git archive"), "{body}");
        assert!(body.contains("github.com/tsnl/myco/releases"), "{body}");
        assert!(body.contains("executable_path"), "{body}");
    }

    #[tokio::test]
    async fn unknown_id_errors() {
        let tool = Arc::new(ManualService::new());
        let res = tool
            .dispatch_tool_use(
                ToolUse {
                    id: "t3".into(),
                    name: "manual".into(),
                    input: json!({"action": "get", "id": "nope"}),
                },
                ctx(),
            )
            .await;
        assert!(res.is_error);
    }
}
