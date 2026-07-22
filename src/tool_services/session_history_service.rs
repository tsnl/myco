//! Root-only tool: explore session transcripts by id (including hidden sessions).

use std::sync::Arc;

use crate::core::Async;
use crate::generative_model::{self, Content, Message, ToolResult, ToolUse};
use crate::session::Session;

use super::{HostDispatchContext, ToolService};

const DEFAULT_MAX_CHARS: usize = 2_000;
const HARD_MAX_CHARS: usize = 32_000;

fn tool_description() -> String {
    format!(
        r#"
Explore a conversation session transcript by id (or unique prefix). Works for visible and
**hidden** sessions (subagents, compact workers).

Actions:
- stats: message count, rough char size, role breakdown, path, hidden/kind/parent.
- range: messages [start, end) with truncated previews (max_chars per message body,
  default {DEFAULT_MAX_CHARS}, hard max {HARD_MAX_CHARS}).
- expand: full text for one message index (or a tool_use / tool_result body by tool id).
- search: case-insensitive substring over text + tool names; returns matching indices.
- write_summary: write markdown summary next to the session file (`{{id}}.summary.md`).
  Used by compaction workers; prefer this over free-form filesystem writes.

Do not dump entire long sessions into context — use stats/search/range, expand only what you need.
"#
    )
}

pub struct SessionHistoryTool;

impl SessionHistoryTool {
    pub fn new() -> Self {
        Self
    }
}

impl Default for SessionHistoryTool {
    fn default() -> Self {
        Self::new()
    }
}

impl ToolService for SessionHistoryTool {
    fn tool_specs(&self) -> Vec<generative_model::ToolSpec> {
        vec![generative_model::ToolSpec {
            name: "session_history".to_string(),
            description: tool_description(),
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
                Err(e) => return ToolResult::err(format!("invalid session_history input: {e}")),
            };
            match self.execute(input) {
                Ok(text) => ToolResult::text(text),
                Err(e) => ToolResult::err(e),
            }
        })
    }
}

impl SessionHistoryTool {
    fn execute(&self, input: Input) -> Result<String, String> {
        let action = input.action.unwrap_or(ActionKind::Stats);
        let session_id = input
            .session_id
            .as_deref()
            .ok_or_else(|| "session_history requires session_id".to_string())?;
        let session = Session::load_by_id_or_prefix(session_id)?;
        match action {
            ActionKind::Stats => Ok(format_stats(&session)),
            ActionKind::Range => {
                let start = input.start.unwrap_or(0);
                let end = input.end.unwrap_or(session.messages.len());
                let max_chars = input
                    .max_chars
                    .unwrap_or(DEFAULT_MAX_CHARS)
                    .min(HARD_MAX_CHARS);
                Ok(format_range(&session, start, end, max_chars))
            }
            ActionKind::Expand => {
                let index = input
                    .index
                    .ok_or_else(|| "expand requires index".to_string())?;
                let max_chars = input
                    .max_chars
                    .unwrap_or(HARD_MAX_CHARS)
                    .min(HARD_MAX_CHARS);
                Ok(format_expand(
                    &session,
                    index,
                    input.tool_use_id.as_deref(),
                    max_chars,
                )?)
            }
            ActionKind::Search => {
                let query = input
                    .query
                    .as_deref()
                    .ok_or_else(|| "search requires query".to_string())?;
                let max_results = input.max_results.unwrap_or(20).min(100);
                Ok(format_search(&session, query, max_results))
            }
            ActionKind::WriteSummary => {
                let markdown = input
                    .markdown
                    .ok_or_else(|| "write_summary requires markdown".to_string())?;
                let path = session.summary_path();
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
                }
                crate::session::atomically_write(path.as_path(), markdown.as_bytes())?;
                Ok(format!(
                    "summary written ({} bytes)\npath={}\n",
                    markdown.len(),
                    path.display()
                ))
            }
        }
    }
}

fn format_stats(session: &Session) -> String {
    let mut users = 0usize;
    let mut assistants = 0usize;
    let mut tool_results = 0usize;
    let mut chars = 0usize;
    for m in &session.messages {
        match m {
            Message::UserMessage { content } => {
                users += 1;
                chars += content_chars(content);
            }
            Message::AssistantMessage {
                content, tool_uses, ..
            } => {
                assistants += 1;
                chars += content_chars(content);
                for t in tool_uses {
                    chars += t.name.len() + t.input.to_string().len();
                }
            }
            Message::ToolResults { tool_use_results } => {
                tool_results += 1;
                for r in tool_use_results {
                    chars += content_chars(&r.content);
                }
            }
        }
    }
    format!(
        "id:        {}\npath:      {}\nsummary:   {}\nhidden:    {}\nkind:      {}\nparent:    {}\nmessages:  {}\n  user:         {}\n  assistant:    {}\n  tool_results: {}\napprox_chars: {}\n",
        session.id,
        session.json_path().display(),
        session.summary_path().display(),
        session.is_hidden(),
        session.kind,
        session.parent_session_id.as_deref().unwrap_or("(none)"),
        session.messages.len(),
        users,
        assistants,
        tool_results,
        chars,
    )
}

fn format_range(session: &Session, start: usize, end: usize, max_chars: usize) -> String {
    let n = session.messages.len();
    let start = start.min(n);
    let end = end.min(n).max(start);
    let mut out = format!("messages [{start}, {end}) of {n}  (max_chars={max_chars})\n");
    for (i, msg) in session.messages[start..end].iter().enumerate() {
        let idx = start + i;
        out.push_str(&format!("\n--- [{idx}] {} ---\n", message_kind(msg)));
        out.push_str(&preview_message(msg, max_chars));
        out.push('\n');
    }
    out
}

fn format_expand(
    session: &Session,
    index: usize,
    tool_use_id: Option<&str>,
    max_chars: usize,
) -> Result<String, String> {
    let msg = session.messages.get(index).ok_or_else(|| {
        format!(
            "index {index} out of range ({} messages)",
            session.messages.len()
        )
    })?;
    if let Some(tid) = tool_use_id {
        return expand_tool(msg, tid, max_chars);
    }
    Ok(format!(
        "[{index}] {}\n{}\n",
        message_kind(msg),
        preview_message(msg, max_chars)
    ))
}

fn expand_tool(msg: &Message, tool_use_id: &str, max_chars: usize) -> Result<String, String> {
    match msg {
        Message::AssistantMessage { tool_uses, .. } => {
            for t in tool_uses {
                if t.id == tool_use_id {
                    let body = serde_json::to_string_pretty(&t.input).unwrap_or_default();
                    return Ok(format!(
                        "tool_use id={} name={}\n{}\n",
                        t.id,
                        t.name,
                        truncate(&body, max_chars)
                    ));
                }
            }
            Err(format!(
                "tool_use_id {tool_use_id:?} not in this assistant message"
            ))
        }
        Message::ToolResults { tool_use_results } => {
            for r in tool_use_results {
                if r.id == tool_use_id {
                    let body = content_text(&r.content);
                    return Ok(format!(
                        "tool_result id={} is_error={}\n{}\n",
                        r.id,
                        r.is_error,
                        truncate(&body, max_chars)
                    ));
                }
            }
            Err(format!(
                "tool_use_id {tool_use_id:?} not in this tool_results message"
            ))
        }
        _ => Err("tool_use_id expand requires an assistant or tool_results message".into()),
    }
}

fn format_search(session: &Session, query: &str, max_results: usize) -> String {
    let q = query.to_ascii_lowercase();
    let mut hits = Vec::new();
    for (i, msg) in session.messages.iter().enumerate() {
        let hay = preview_message(msg, HARD_MAX_CHARS).to_ascii_lowercase();
        if hay.contains(&q) {
            hits.push(i);
            if hits.len() >= max_results {
                break;
            }
        }
    }
    let mut out = format!("query={query:?}  hits={} (max {max_results})\n", hits.len());
    for i in hits {
        out.push_str(&format!(
            "  [{i}] {}  {}\n",
            message_kind(&session.messages[i]),
            truncate(
                &preview_message(&session.messages[i], 120).replace('\n', " "),
                120
            )
        ));
    }
    out
}

fn message_kind(msg: &Message) -> &'static str {
    match msg {
        Message::UserMessage { .. } => "UserMessage",
        Message::AssistantMessage { .. } => "AssistantMessage",
        Message::ToolResults { .. } => "ToolResults",
    }
}

fn preview_message(msg: &Message, max_chars: usize) -> String {
    match msg {
        Message::UserMessage { content } => truncate(&content_text(content), max_chars),
        Message::AssistantMessage {
            content, tool_uses, ..
        } => {
            let mut s = content_text(content);
            if !tool_uses.is_empty() {
                if !s.is_empty() {
                    s.push('\n');
                }
                for t in tool_uses {
                    s.push_str(&format!(
                        "tool_use id={} name={} input={}\n",
                        t.id,
                        t.name,
                        truncate(&t.input.to_string(), 200)
                    ));
                }
            }
            truncate(&s, max_chars)
        }
        Message::ToolResults { tool_use_results } => {
            let mut s = String::new();
            for r in tool_use_results {
                s.push_str(&format!(
                    "tool_result id={} is_error={} {}\n",
                    r.id,
                    r.is_error,
                    truncate(&content_text(&r.content), 400)
                ));
            }
            truncate(&s, max_chars)
        }
    }
}

fn content_text(content: &[Content]) -> String {
    content
        .iter()
        .filter_map(|c| match c {
            Content::Text { text } => Some(text.as_str()),
            Content::Thinking { text, .. } if !text.is_empty() => Some(text.as_str()),
            Content::Image { .. } => Some("[image]"),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn content_chars(content: &[Content]) -> usize {
    content_text(content).len()
}

fn truncate(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let t: String = s.chars().take(max_chars.saturating_sub(1)).collect();
    format!("{t}…")
}

#[derive(Clone, Debug, schemars::JsonSchema, serde::Deserialize, serde::Serialize)]
struct Input {
    /// Session id or unique prefix (required).
    session_id: Option<String>,
    #[serde(default)]
    action: Option<ActionKind>,
    #[serde(default)]
    start: Option<usize>,
    #[serde(default)]
    end: Option<usize>,
    #[serde(default)]
    index: Option<usize>,
    #[serde(default)]
    tool_use_id: Option<String>,
    #[serde(default)]
    query: Option<String>,
    #[serde(default)]
    max_chars: Option<usize>,
    #[serde(default)]
    max_results: Option<usize>,
    /// Markdown body for `write_summary`.
    #[serde(default)]
    markdown: Option<String>,
}

#[derive(Clone, Debug, schemars::JsonSchema, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
enum ActionKind {
    Stats,
    Range,
    Expand,
    Search,
    WriteSummary,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The tool description is the model-facing contract: it must state the
    /// defaults/limits actually enforced, not stale hardcoded copies.
    #[test]
    fn tool_description_states_actual_defaults() {
        let specs = SessionHistoryTool::new().tool_specs();
        let d = &specs[0].description;
        for needle in [DEFAULT_MAX_CHARS.to_string(), HARD_MAX_CHARS.to_string()] {
            assert!(d.contains(&needle), "description missing {needle}: {d}");
        }
        // `format!` must not have swallowed the literal `{id}` placeholder.
        assert!(d.contains("{id}.summary.md"), "{d}");
    }
}
