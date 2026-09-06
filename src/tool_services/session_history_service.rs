//! Root-only tool: explore session transcripts by id (including hidden sessions).

use std::sync::Arc;

use crate::core::Async;
use crate::generative_model::{self, Content, Message, ToolResult, ToolUse};
use crate::session::{Session, Thread};

use super::{HostDispatchContext, ToolService};

const DEFAULT_MAX_CHARS: usize = 2_000;
const HARD_MAX_CHARS: usize = 32_000;

fn tool_description() -> String {
    format!(
        r#"
Explore a conversation session transcript by id (or unique prefix). Works for visible and
**hidden** sessions (subagents, compact workers). Omit thread_id for the active thread;
pass an older thread_id to retrieve its recorded observations. Compaction preserves these
observations while live tools continue to change.

Actions:
- threads: list thread ids, predecessors, and message counts (newest first).
- stats: message count, rough char size, role breakdown, path, hidden/kind/parent.
- range: messages [start, end) with truncated previews (max_chars per message body,
  default {DEFAULT_MAX_CHARS}, hard max {HARD_MAX_CHARS}).
- expand: full text for one message index (or a single tool_use / tool_result body via
  tool_ordinal, its zero-based position within the message).
- search: case-insensitive substring over text + tool names; returns matching indices.
- write_summary: write markdown summary for the active thread (`{{session_id}}.{{thread_id}}.summary.md`).
  Used by compaction workers; prefer this over free-form filesystem writes.

Do not dump entire long sessions into context — use stats/search/range, expand only what you need.
Raw session files are minified single-line JSON — prefer this tool (or `jq`) over `cat`/`grep`.
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
        if matches!(action, ActionKind::Threads) {
            return Ok(format_threads(
                &session,
                input.max_results.unwrap_or(20).min(100),
            ));
        }
        let thread = session.find_thread(
            input
                .thread_id
                .as_deref()
                .unwrap_or(&session.active_thread().id),
        )?;
        match action {
            ActionKind::Threads => unreachable!("handled before thread selection"),
            ActionKind::Stats => Ok(format_stats(&session, thread)),
            ActionKind::Range => {
                let start = input.start.unwrap_or(0);
                let end = input.end.unwrap_or(thread.messages.len());
                let max_chars = input
                    .max_chars
                    .unwrap_or(DEFAULT_MAX_CHARS)
                    .min(HARD_MAX_CHARS);
                Ok(format_range(thread, start, end, max_chars))
            }
            ActionKind::Expand => {
                let index = input
                    .index
                    .ok_or_else(|| "expand requires index".to_string())?;
                let max_chars = input
                    .max_chars
                    .unwrap_or(HARD_MAX_CHARS)
                    .min(HARD_MAX_CHARS);
                Ok(format_expand(thread, index, input.tool_ordinal, max_chars)?)
            }
            ActionKind::Search => {
                let query = input
                    .query
                    .as_deref()
                    .ok_or_else(|| "search requires query".to_string())?;
                let max_results = input.max_results.unwrap_or(20).min(100);
                Ok(format_search(thread, query, max_results))
            }
            ActionKind::WriteSummary => {
                if thread.id != session.active_thread().id {
                    return Err("only the active thread accepts a new summary".into());
                }
                let markdown = input
                    .markdown
                    .ok_or_else(|| "write_summary requires markdown".to_string())?;
                let path = session.summary_path();
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
                }
                crate::core::atomically_write(path.as_path(), markdown.as_bytes())?;
                Ok(format!(
                    "summary written ({} bytes)\npath={}\n",
                    markdown.len(),
                    path.display()
                ))
            }
        }
    }
}

fn format_threads(session: &Session, limit: usize) -> String {
    let mut text = format!(
        "session: {}\nactive_thread: {}\nthreads: {}\n",
        session.id,
        session.active_thread().id,
        session.threads().len()
    );
    for thread in session.threads().iter().rev().take(limit) {
        text.push_str(&format!(
            "{}  messages={}  predecessor={}\n",
            thread.id,
            thread.messages.len(),
            thread.predecessor_id.as_deref().unwrap_or("(none)")
        ));
    }
    text
}

fn format_stats(session: &Session, thread: &Thread) -> String {
    let mut users = 0usize;
    let mut assistants = 0usize;
    let mut tool_results = 0usize;
    let mut chars = 0usize;
    for m in &thread.messages {
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
        "id:        {}\nthread:    {}\npredecessor: {}\npath:      {}\nsummary:   {}\nhidden:    {}\nkind:      {}\nparent:    {}\nmessages:  {}\n  user:         {}\n  assistant:    {}\n  tool_results: {}\napprox_chars: {}\n",
        session.id,
        thread.id,
        thread.predecessor_id.as_deref().unwrap_or("(none)"),
        session.json_path().display(),
        session.thread_summary_path(&thread.id).display(),
        session.is_hidden(),
        session.kind,
        session.parent_session_id.as_deref().unwrap_or("(none)"),
        thread.messages.len(),
        users,
        assistants,
        tool_results,
        chars,
    )
}

/// Total-output cap for `range`: the per-message `max_chars` alone lets a
/// whole-session range emit ~1 MB — on exactly the long sessions this tool
/// serves (compaction workers). Stop honestly instead.
const RANGE_TOTAL_CHARS: usize = 64_000;

fn format_range(thread: &Thread, start: usize, end: usize, max_chars: usize) -> String {
    let n = thread.messages.len();
    let start = start.min(n);
    let end = end.min(n).max(start);
    let mut out = format!("messages [{start}, {end}) of {n}  (max_chars={max_chars})\n");
    for (i, msg) in thread.messages[start..end].iter().enumerate() {
        let idx = start + i;
        if out.len() >= RANGE_TOTAL_CHARS {
            out.push_str(&format!(
                "\n(stopped at index {idx} of requested [{start}, {end}) — total output cap \
                 {RANGE_TOTAL_CHARS} chars reached; narrow the range or lower max_chars)\n"
            ));
            return out;
        }
        out.push_str(&format!("\n--- [{idx}] {} ---\n", message_kind(msg)));
        out.push_str(&preview_message(msg, max_chars));
        out.push('\n');
    }
    out
}

fn format_expand(
    thread: &Thread,
    index: usize,
    tool_ordinal: Option<usize>,
    max_chars: usize,
) -> Result<String, String> {
    let msg = thread.messages.get(index).ok_or_else(|| {
        format!(
            "index {index} out of range ({} messages)",
            thread.messages.len()
        )
    })?;
    if let Some(ordinal) = tool_ordinal {
        return expand_tool(msg, ordinal, max_chars);
    }
    Ok(format!(
        "[{index}] {}\n{}\n",
        message_kind(msg),
        preview_message(msg, max_chars)
    ))
}

fn expand_tool(msg: &Message, ordinal: usize, max_chars: usize) -> Result<String, String> {
    match msg {
        Message::AssistantMessage { tool_uses, .. } => {
            let Some(t) = tool_uses.get(ordinal) else {
                return Err(format!(
                    "tool_ordinal {ordinal} out of range ({} tool calls in this assistant message)",
                    tool_uses.len()
                ));
            };
            let body = serde_json::to_string_pretty(&t.input).unwrap_or_default();
            Ok(format!(
                "tool_use [{ordinal}] name={}\n{}\n",
                t.name,
                truncate(&body, max_chars)
            ))
        }
        Message::ToolResults { tool_use_results } => {
            let Some(r) = tool_use_results.get(ordinal) else {
                return Err(format!(
                    "tool_ordinal {ordinal} out of range ({} results in this tool_results message)",
                    tool_use_results.len()
                ));
            };
            let body = content_text(&r.content);
            Ok(format!(
                "tool_result [{ordinal}] is_error={}\n{}\n",
                r.is_error,
                truncate(&body, max_chars)
            ))
        }
        _ => Err("tool_ordinal expand requires an assistant or tool_results message".into()),
    }
}

fn format_search(thread: &Thread, query: &str, max_results: usize) -> String {
    let q = query.to_ascii_lowercase();
    let mut hits = Vec::new();
    for (i, msg) in thread.messages.iter().enumerate() {
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
            message_kind(&thread.messages[i]),
            truncate(
                &preview_message(&thread.messages[i], 120).replace('\n', " "),
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
                for (j, t) in tool_uses.iter().enumerate() {
                    s.push_str(&format!(
                        "tool_use [{j}] name={} input={}\n",
                        t.name,
                        truncate(&t.input.to_string(), 200)
                    ));
                }
            }
            truncate(&s, max_chars)
        }
        Message::ToolResults { tool_use_results } => {
            let mut s = String::new();
            for (j, r) in tool_use_results.iter().enumerate() {
                s.push_str(&format!(
                    "tool_result [{j}] is_error={} {}\n",
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

/// `deny_unknown_fields` also makes a routing `host` an error rather than a
/// silent no-op: this is a root-only tool that always runs on `local`, so a call
/// that asks for a remote must not come back with local results labelled as
/// success.
#[derive(Clone, Debug, schemars::JsonSchema, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
struct Input {
    /// Session id or unique prefix (required).
    session_id: Option<String>,
    /// Exact thread id; defaults to the active thread.
    #[serde(default)]
    thread_id: Option<String>,
    #[serde(default)]
    action: Option<ActionKind>,
    #[serde(default)]
    start: Option<usize>,
    #[serde(default)]
    end: Option<usize>,
    #[serde(default)]
    index: Option<usize>,
    /// Zero-based position of one tool_use / tool_result inside the message at
    /// `index` (expand only). History carries no tool ids; position is the key.
    #[serde(default)]
    tool_ordinal: Option<usize>,
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
    Threads,
    Stats,
    Range,
    Expand,
    Search,
    WriteSummary,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn history_can_retrieve_an_older_threads_original_tool_output() {
        let _home = crate::test_support::temp_home("thread-history");
        let mut session = Session::new("m");
        session.replace_context(
            vec![
                crate::test_support::user("task"),
                crate::test_support::assistant_tool(None, "bash", serde_json::json!({})),
                crate::test_support::tool_results(&["original observation"]),
            ],
            None,
        );
        let thread_id = session.active_thread().id.clone();
        let session_id = session.id.clone();
        let (next, _) = crate::session::compact_thread(&session, "summary").unwrap();
        let active = crate::session::ActiveSession::new(session);
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime
            .block_on(active.writer())
            .commit_thread(next)
            .unwrap();
        let tool = SessionHistoryTool::new();
        let input = serde_json::from_value(serde_json::json!({"session_id":session_id, "thread_id":thread_id, "action":"expand", "index":2})).unwrap();
        assert!(
            tool.execute(input)
                .unwrap()
                .contains("original observation")
        );
        let input = serde_json::from_value(
            serde_json::json!({"session_id":session_id, "action":"threads"}),
        )
        .unwrap();
        let listing = tool.execute(input).unwrap();
        assert!(listing.contains(&thread_id));
        assert!(listing.contains(&active.snapshot().active_thread().id));
        let input = serde_json::from_value(serde_json::json!({"session_id":session_id, "thread_id":thread_id, "action":"write_summary", "markdown":"changed"})).unwrap();
        assert!(tool.execute(input).is_err());
    }

    /// The tool description is the model-facing contract: it must state the
    /// defaults/limits actually enforced, not stale hardcoded copies.
    #[test]
    fn tool_description_states_actual_defaults() {
        let specs = SessionHistoryTool::new().tool_specs();
        let d = &specs[0].description;
        for needle in [DEFAULT_MAX_CHARS.to_string(), HARD_MAX_CHARS.to_string()] {
            assert!(d.contains(&needle), "description missing {needle}: {d}");
        }
        // `format!` must not have swallowed the literal path placeholders.
        assert!(d.contains("{session_id}.{thread_id}.summary.md"), "{d}");
    }

    /// A whole-session range on a long session must stop at the total cap
    /// with an honest marker, not emit ~1 MB into the caller's context.
    #[test]
    fn range_stops_at_total_output_cap() {
        use crate::generative_model::{Content, Message};
        let mut session = crate::session::Session::new("test-model");
        for i in 0..200 {
            session
                .active_thread_mut()
                .messages
                .push(Message::UserMessage {
                    content: vec![Content::Text {
                        text: format!("msg {i}: {}", "x".repeat(2_000)),
                    }],
                });
        }
        let out = format_range(
            session.active_thread(),
            0,
            session.active_thread().messages.len(),
            2_000,
        );
        assert!(
            out.len() < RANGE_TOTAL_CHARS + 4_000,
            "output should stop near the cap, got {} chars",
            out.len()
        );
        assert!(out.contains("stopped at index"), "{}", &out[..200]);
        assert!(out.contains("narrow the range"), "missing hint");
    }
}
