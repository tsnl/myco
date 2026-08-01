//! Root-only tool: edit the agent memory (`~/.myco/workspace/memory/` entries).
//!
//! Installed on the in-process local worker only — the memory lives on the
//! user's machine (brains stay local), so there is nothing to run remotely.
//! Storage semantics live in [`crate::memory`]; this service adds the model
//! surface and reports the rendered size against `max_memory_bytes` so agents
//! notice cap pressure while curating, not at the next startup warning.

use std::path::Path;
use std::sync::Arc;

use crate::core::Async;
use crate::generative_model::{self, ToolResult};
use crate::memory;

use super::{HostDispatchContext, ToolService};

const TOOL_DESCRIPTION: &str = r#"
Edit your memory: the maildir-style entries under `~/.myco/workspace/memory/` rendered, every
entry in filename order, into the `# Memory` section of every agent system prompt.

Memory is the default home for durable information — record findings eagerly (user
preferences, project and machine facts, gotchas, settled decisions, what finally worked),
and curate just as eagerly (merge overlap, supersede stale claims, drop dead entries).
See "Workspace & memory" in your prompt. Entries are write-once files; use this tool, not
bash or the editor, so concurrent agents cannot clobber each other.

Actions (`action` is required):
- add: create a new entry from `text`; returns its id. Concurrent adds never conflict.
- replace: `id` + `text` — write the replacement as a new entry, then drop `id`. If
  another agent already removed `id`, the replacement still lands and the result says
  so; `list` and merge any duplicate that left behind.
- remove: delete entry `id`.
- list: the live entries with ids and text, plus total rendered size against
  `max_memory_bytes`. The `# Memory` in your prompt is a snapshot from model build time;
  `list` is the current state — check it before curating.

Edits reach the *next* built agent prompt (session start, model switch, every nested
agent and worker spawn) — your current conversation keeps its snapshot.
"#;

/// Local tool over the memory directory; carries the resolved `max_memory_bytes`
/// so every result can report headroom against the cap.
pub struct MemoryTool {
    max_memory_bytes: usize,
}

impl MemoryTool {
    pub fn new(max_memory_bytes: usize) -> Self {
        Self { max_memory_bytes }
    }
}

impl ToolService for MemoryTool {
    fn tool_specs(&self) -> Vec<generative_model::ToolSpec> {
        vec![generative_model::ToolSpec {
            name: "memory".to_string(),
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
                Err(e) => return ToolResult::err(format!("invalid memory input: {e}")),
            };
            // Off the executor: every action reads or writes entry files.
            match tokio::task::spawn_blocking(move || self.execute(input)).await {
                Ok(Ok(text)) => ToolResult::text(text),
                Ok(Err(e)) => ToolResult::err(e),
                Err(e) => ToolResult::err(format!("memory join: {e}")),
            }
        })
    }
}

impl MemoryTool {
    fn execute(&self, input: Input) -> Result<String, String> {
        let dir = memory::dir()?;
        match input.action {
            ActionKind::Add => {
                let text = required_text(input.text, "add")?;
                let name = memory::add_entry(&dir, &text)?;
                Ok(format!("added memory/{name}\n{}", self.status(&dir)))
            }
            ActionKind::Replace => {
                let id = required_id(input.id, "replace")?;
                let text = required_text(input.text, "replace")?;
                if !memory::is_entry_name(&id) {
                    return Err(format!(
                        "{id:?} is not a memory entry id (a plain `*.md` filename, as listed)"
                    ));
                }
                // Replacement lands before the old entry goes, so a crash or
                // race between the two steps duplicates — never loses.
                let name = memory::add_entry(&dir, &text)?;
                let note = match memory::remove_entry(&dir, &id)? {
                    true => String::new(),
                    false => format!(
                        "note: memory/{id} was already gone (concurrent edit?) — \
                         action=list and merge any duplicate\n"
                    ),
                };
                Ok(format!(
                    "replaced memory/{id} with memory/{name}\n{note}{}",
                    self.status(&dir)
                ))
            }
            ActionKind::Remove => {
                let id = required_id(input.id, "remove")?;
                match memory::remove_entry(&dir, &id)? {
                    true => Ok(format!("removed memory/{id}\n{}", self.status(&dir))),
                    false => Err(format!(
                        "memory/{id} not found — already removed or replaced by another \
                         agent; action=list shows the live entries"
                    )),
                }
            }
            ActionKind::List => {
                let entries = memory::entries(&dir);
                if entries.is_empty() {
                    return Ok(format!("(no memory entries)\n{}", self.status(&dir)));
                }
                Ok(format!(
                    "{}\n\n{}",
                    self.status(&dir),
                    memory::rendered_body(&entries)
                ))
            }
        }
    }

    /// One line of live totals, plus a loud second line once the rendered
    /// memory no longer fits — the agent holding the pen is the one who can
    /// merge entries, so it hears about cap pressure immediately.
    fn status(&self, dir: &Path) -> String {
        let entries = memory::entries(dir);
        let bytes = memory::rendered_body(&entries).len();
        let mut out = format!(
            "memory: {} entr{}, {bytes} bytes rendered of {} max_memory_bytes\n",
            entries.len(),
            if entries.len() == 1 { "y" } else { "ies" },
            self.max_memory_bytes,
        );
        if bytes > self.max_memory_bytes {
            out.push_str(
                "WARNING: over max_memory_bytes — future prompts truncate the tail; merge \
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
            "{action} requires `id` (the entry filename, as listed in `# Memory` / action=list)"
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
        tool: &Arc<MemoryTool>,
        input: serde_json::Value,
    ) -> generative_model::ToolResult {
        tool.clone()
            .dispatch_tool_use(
                generative_model::ToolUse {
                    name: "memory".into(),
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
        let spec = MemoryTool::new(1024).tool_specs().remove(0);
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
        let _home = temp_home("memory-tool");

        let tool = Arc::new(MemoryTool::new(1024 * 1024));

        // add returns the new id and live totals.
        let added = call(
            &tool,
            serde_json::json!({"action": "add", "text": "fact one"}),
        )
        .await;
        assert!(!added.is_error, "{added:?}");
        let added_text = result_text(&added);
        assert!(added_text.contains("added memory/"), "{added_text}");
        assert!(added_text.contains("memory: 1 entry"), "{added_text}");
        let id = added_text
            .lines()
            .next()
            .unwrap()
            .strip_prefix("added memory/")
            .unwrap()
            .to_string();
        assert!(memory::is_entry_name(&id), "{id}");

        // list shows the entry under its label — same shape as the prompt.
        let listed = call(&tool, serde_json::json!({"action": "list"})).await;
        assert!(!listed.is_error, "{listed:?}");
        assert!(
            result_text(&listed).contains(&format!("[memory entry {id}]\nfact one")),
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
            replaced_text.contains(&format!("replaced memory/{id} with memory/")),
            "{replaced_text}"
        );
        assert!(replaced_text.contains("memory: 1 entry"), "{replaced_text}");
        let listed = result_text(&call(&tool, serde_json::json!({"action": "list"})).await);
        assert!(listed.contains("fact one, amended"), "{listed}");
        assert!(
            !listed.contains(&format!("[memory entry {id}]")),
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
        assert!(listed.contains("memory: 2 entries"), "{listed}");

        // remove deletes by id; removing again is a loud error, not a wipe.
        let live_id = listed
            .lines()
            .find_map(|l| {
                l.strip_prefix("[memory entry ")
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
        let _home = temp_home("memory-tool-guards");

        let tool = Arc::new(MemoryTool::new(64));

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
            result_text(&big).contains("WARNING: over max_memory_bytes"),
            "{big:?}"
        );
    }
}
