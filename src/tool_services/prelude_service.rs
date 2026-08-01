//! Root-only tool: edit the agent prelude (`~/.myco/workspace/prelude/` entries).
//!
//! Installed on the in-process local worker only — the prelude lives on the
//! user's machine (brains stay local), so there is nothing to run remotely.
//! Storage semantics live in [`crate::prelude`]; this service adds the model
//! surface and reports the rendered size against `max_prelude_bytes` so agents
//! notice cap pressure while curating, not at the next startup warning.

use std::path::Path;
use std::sync::Arc;

use crate::core::Async;
use crate::generative_model::{self, ToolResult};
use crate::prelude;

use super::{HostDispatchContext, ToolService};

const TOOL_DESCRIPTION: &str = r#"
Edit your prelude: the maildir-style entries under `~/.myco/workspace/prelude/` rendered, every
entry in filename order, into the `# Prelude` section of every agent system prompt.

The prelude is the default home for durable information — record findings eagerly (user
preferences, project and machine facts, gotchas, settled decisions, what finally worked),
and curate just as eagerly (merge overlap, supersede stale claims, drop dead entries).
See "Workspace & prelude" in your prompt. Entries are write-once files; use this tool, not
bash or the editor, so concurrent agents cannot clobber each other.

Actions (`action` is required):
- add: create a new entry from `text`; returns its id. Concurrent adds never conflict.
- replace: `id` + `text` — write the replacement as a new entry, then drop `id`. If
  another agent already removed `id`, the replacement still lands and the result says
  so; `list` and merge any duplicate that left behind.
- remove: delete entry `id`.
- list: the live entries with ids and text, plus total rendered size against
  `max_prelude_bytes`. The `# Prelude` in your prompt is a snapshot from model build time;
  `list` is the current state — check it before curating.

Edits reach the *next* built agent prompt (session start, model switch, every nested
agent and worker spawn) — your current conversation keeps its snapshot.
"#;

/// Local tool over the prelude directory; carries the resolved `max_prelude_bytes`
/// so every result can report headroom against the cap.
pub struct PreludeTool {
    max_prelude_bytes: usize,
}

impl PreludeTool {
    pub fn new(max_prelude_bytes: usize) -> Self {
        Self { max_prelude_bytes }
    }
}

impl ToolService for PreludeTool {
    fn tool_specs(&self) -> Vec<generative_model::ToolSpec> {
        vec![generative_model::ToolSpec {
            name: "prelude".to_string(),
            description: TOOL_DESCRIPTION.to_string(),
            input_schema: super::tool_input_schema::<Input>(),
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
                Err(e) => return ToolResult::err(format!("invalid prelude input: {e}")),
            };
            // Off the executor: every action reads or writes entry files.
            match tokio::task::spawn_blocking(move || self.execute(input)).await {
                Ok(Ok(text)) => ToolResult::text(text),
                Ok(Err(e)) => ToolResult::err(e),
                Err(e) => ToolResult::err(format!("prelude join: {e}")),
            }
        })
    }
}

impl PreludeTool {
    fn execute(&self, input: Input) -> Result<String, String> {
        let dir = prelude::dir()?;
        match input.action {
            ActionKind::Add => {
                let text = required_text(input.text, "add")?;
                let name = prelude::add_entry(&dir, &text)?;
                Ok(format!("added prelude/{name}\n{}", self.status(&dir)))
            }
            ActionKind::Replace => {
                let id = required_id(input.id, "replace")?;
                let text = required_text(input.text, "replace")?;
                if !prelude::is_entry_name(&id) {
                    return Err(format!(
                        "{id:?} is not a prelude entry id (a plain `*.md` filename, as listed)"
                    ));
                }
                // Replacement lands before the old entry goes, so a crash or
                // race between the two steps duplicates — never loses.
                let name = prelude::add_entry(&dir, &text)?;
                let note = match prelude::remove_entry(&dir, &id)? {
                    true => String::new(),
                    false => format!(
                        "note: prelude/{id} was already gone (concurrent edit?) — \
                         action=list and merge any duplicate\n"
                    ),
                };
                Ok(format!(
                    "replaced prelude/{id} with prelude/{name}\n{note}{}",
                    self.status(&dir)
                ))
            }
            ActionKind::Remove => {
                let id = required_id(input.id, "remove")?;
                match prelude::remove_entry(&dir, &id)? {
                    true => Ok(format!("removed prelude/{id}\n{}", self.status(&dir))),
                    false => Err(format!(
                        "prelude/{id} not found — already removed or replaced by another \
                         agent; action=list shows the live entries"
                    )),
                }
            }
            ActionKind::List => {
                let entries = prelude::entries(&dir);
                if entries.is_empty() {
                    return Ok(format!("(no prelude entries)\n{}", self.status(&dir)));
                }
                Ok(format!(
                    "{}\n\n{}",
                    self.status(&dir),
                    prelude::rendered_body(&entries)
                ))
            }
        }
    }

    /// One line of live totals, plus a loud second line once the rendered
    /// prelude no longer fits — the agent holding the pen is the one who can
    /// merge entries, so it hears about cap pressure immediately.
    fn status(&self, dir: &Path) -> String {
        let entries = prelude::entries(dir);
        let bytes = prelude::rendered_body(&entries).len();
        let mut out = format!(
            "prelude: {} entr{}, {bytes} bytes rendered of {} max_prelude_bytes\n",
            entries.len(),
            if entries.len() == 1 { "y" } else { "ies" },
            self.max_prelude_bytes,
        );
        if bytes > self.max_prelude_bytes {
            out.push_str(
                "WARNING: over max_prelude_bytes — future prompts truncate the tail; merge \
                 entries and move cold material to workspace files\n",
            );
        }
        out
    }
}

fn required_text(text: Option<String>, action: &str) -> Result<String, String> {
    match text {
        Some(t) if !t.trim().is_empty() => Ok(t),
        _ => Err(format!("{action} requires non-empty `text`")),
    }
}

fn required_id(id: Option<String>, action: &str) -> Result<String, String> {
    match id {
        Some(id) if !id.trim().is_empty() => Ok(id.trim().to_string()),
        _ => Err(format!(
            "{action} requires `id` (the entry filename, as listed in `# Prelude` / action=list)"
        )),
    }
}

// --- input schema ------------------------------------------------------------

#[derive(
    Clone, Debug, schemars::JsonSchema, serde::Deserialize, serde::Serialize, PartialEq, Eq,
)]
#[serde(deny_unknown_fields)]
struct Input {
    /// Action to perform (required).
    action: ActionKind,
    /// Entry id (filename, e.g. `20260729T153012Z-3f2a.md`) for `replace` / `remove`.
    #[serde(default)]
    id: Option<String>,
    /// Entry markdown for `add` / `replace`.
    #[serde(default)]
    text: Option<String>,
}

#[derive(
    Clone, Debug, schemars::JsonSchema, serde::Deserialize, serde::Serialize, PartialEq, Eq,
)]
#[serde(rename_all = "snake_case")]
enum ActionKind {
    Add,
    Replace,
    Remove,
    List,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CancelToken;
    use crate::test_support::{result_text, temp_home};
    use crate::tool_services::{HostDispatchContext, ToolService};

    async fn call(
        tool: &Arc<PreludeTool>,
        input: serde_json::Value,
    ) -> generative_model::ToolResult {
        tool.clone()
            .dispatch_tool_use(
                generative_model::ToolUse {
                    name: "prelude".into(),
                    input,
                },
                HostDispatchContext::new(uuid::Uuid::nil(), CancelToken::new()),
            )
            .await
    }

    /// The advertised schema must be safe for OpenAI-compatible gateways:
    /// action enum inlined and required, no refs, closed object.
    #[test]
    fn input_schema_is_flat_required_and_closed() {
        let spec = PreludeTool::new(1024).tool_specs().remove(0);
        let schema = spec.input_schema;
        let text = schema.to_string();

        assert!(!text.contains("$defs"), "{text}");
        assert!(!text.contains("$ref"), "{text}");
        assert!(!text.contains("anyOf"), "{text}");
        assert!(!text.contains("\"default\":null"), "{text}");
        assert_eq!(
            schema["properties"]["action"]["enum"],
            serde_json::json!(["add", "replace", "remove", "list"]),
            "{text}"
        );
        assert_eq!(schema["required"], serde_json::json!(["action"]), "{text}");
        assert_eq!(
            schema["additionalProperties"],
            serde_json::json!(false),
            "{text}"
        );
    }

    #[tokio::test]
    async fn add_replace_remove_list_round_trip() {
        let _home = temp_home("prelude-tool");

        let tool = Arc::new(PreludeTool::new(1024 * 1024));

        // add returns the new id and live totals.
        let added = call(
            &tool,
            serde_json::json!({"action": "add", "text": "fact one"}),
        )
        .await;
        assert!(!added.is_error, "{added:?}");
        let added_text = result_text(&added);
        assert!(added_text.contains("added prelude/"), "{added_text}");
        assert!(added_text.contains("prelude: 1 entry"), "{added_text}");
        let id = added_text
            .lines()
            .next()
            .unwrap()
            .strip_prefix("added prelude/")
            .unwrap()
            .to_string();
        assert!(prelude::is_entry_name(&id), "{id}");

        // list shows the entry under its label — same shape as the prompt.
        let listed = call(&tool, serde_json::json!({"action": "list"})).await;
        assert!(!listed.is_error, "{listed:?}");
        assert!(
            result_text(&listed).contains(&format!("[prelude entry {id}]\nfact one")),
            "{listed:?}"
        );

        // replace supersedes the old id with a fresh entry.
        let replaced = call(
            &tool,
            serde_json::json!({"action": "replace", "id": id, "text": "fact one, amended"}),
        )
        .await;
        assert!(!replaced.is_error, "{replaced:?}");
        let replaced_text = result_text(&replaced);
        assert!(
            replaced_text.contains(&format!("replaced prelude/{id} with prelude/")),
            "{replaced_text}"
        );
        assert!(
            replaced_text.contains("prelude: 1 entry"),
            "{replaced_text}"
        );
        let listed = result_text(&call(&tool, serde_json::json!({"action": "list"})).await);
        assert!(listed.contains("fact one, amended"), "{listed}");
        assert!(
            !listed.contains(&format!("[prelude entry {id}]")),
            "{listed}"
        );

        // replace of an id another agent already dropped still lands the new
        // entry, and says so instead of erroring.
        let raced = call(
            &tool,
            serde_json::json!({"action": "replace", "id": id, "text": "raced rewrite"}),
        )
        .await;
        assert!(!raced.is_error, "{raced:?}");
        assert!(
            result_text(&raced).contains("already gone (concurrent edit?)"),
            "{raced:?}"
        );
        let listed = result_text(&call(&tool, serde_json::json!({"action": "list"})).await);
        assert!(listed.contains("raced rewrite"), "{listed}");
        assert!(listed.contains("prelude: 2 entries"), "{listed}");

        // remove deletes by id; removing again is a loud error, not a wipe.
        let live_id = listed
            .lines()
            .find_map(|l| {
                l.strip_prefix("[prelude entry ")
                    .and_then(|l| l.strip_suffix(']'))
            })
            .unwrap()
            .to_string();
        let removed = call(
            &tool,
            serde_json::json!({"action": "remove", "id": live_id}),
        )
        .await;
        assert!(!removed.is_error, "{removed:?}");
        let again = call(
            &tool,
            serde_json::json!({"action": "remove", "id": live_id}),
        )
        .await;
        assert!(again.is_error, "{again:?}");
        assert!(result_text(&again).contains("not found"), "{again:?}");
    }

    /// Null-filled or missing fields must error instead of writing junk
    /// entries, and cap pressure is reported while the agent can still act.
    #[tokio::test]
    async fn guards_and_cap_warning() {
        let _home = temp_home("prelude-tool-guards");

        let tool = Arc::new(PreludeTool::new(64));

        for input in [
            serde_json::json!({"action": "add"}),
            serde_json::json!({"action": "add", "text": "   "}),
            serde_json::json!({"action": "replace", "text": "x"}),
            serde_json::json!({"action": "remove"}),
            serde_json::json!({"action": "remove", "id": "../escape.md"}),
        ] {
            let r = call(&tool, input.clone()).await;
            assert!(r.is_error, "{input} should error: {r:?}");
        }

        // Over the cap: the result warns immediately, while the agent that
        // wrote the entry can still merge or move material out.
        let big = call(
            &tool,
            serde_json::json!({"action": "add", "text": "y".repeat(200)}),
        )
        .await;
        assert!(!big.is_error, "{big:?}");
        assert!(
            result_text(&big).contains("WARNING: over max_prelude_bytes"),
            "{big:?}"
        );
    }
}
