//! Root-only `memory` tool: persistent memory shared across agents and sessions.
//!
//! One memory document per machine under `~/.myco/memory/`. Updates are
//! **timestamped append-only** entries written with `O_APPEND`, so concurrent
//! sessions/subagents never rewrite each other. When the current file reaches
//! [`MEMORY_ROTATE_BYTES`] the next append starts a new timestamped file; only
//! the **latest** file is indexed for search (older files stay on disk for
//! bash/grep — GC/pruning is future work).
//!
//! Search uses a dedicated in-RAM [`SearchIndex`] with one document per entry
//! (pseudo-path `<file>#L<start_line>`), refreshed from disk before each query
//! so appends from other myco processes are visible. Pure appends index only
//! the new tail; any other on-disk change rebuilds the index. File reads and
//! MiniLM embedding run on the blocking pool (same rule as the engine: candle
//! never runs on an executor thread).

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::Utc;

use crate::core::Async;
use crate::generative_model::{self, ToolResult, ToolUse};
use crate::session::{myco_home, uuid_simple_hex};
use crate::text_search::{Hit, SearchIndex, embed_for_index};

use super::{HostDispatchContext, ToolService};

const TOOL_DESCRIPTION: &str = r#"
Persistent memory shared across agents and sessions (one document per machine, under
`~/.myco/memory/`).

Entries are timestamped and **append-only**: appends never rewrite earlier content, so
concurrent sessions and subagents cannot conflict. Files rotate at a size cap; searches
cover only the **latest** memory file (older files stay on disk for bash/grep; no GC yet).

Actions:
- append: add a timestamped entry (markdown). Keep entries short and durable — user
  preferences, project facts, decisions, gotchas. Not a scratchpad: use session_meta
  set_scratchpad for session-local notes.
- exact_search: Tantivy full-text over entries of the latest memory file (identifiers,
  literal phrases). Hits are file:line refs you can open with the editor.
- semantic_search: Candle MiniLM cosine over per-entry embeddings (intent queries like
  "how does the user prefer commits formatted").
"#;

/// Rotate to a new timestamped file once the current one reaches this size.
pub const MEMORY_ROTATE_BYTES: u64 = 256 * 1024;
const MAX_APPEND_BYTES: usize = 16 * 1024;
const DEFAULT_MAX_RESULTS: usize = 10;
const MAX_MAX_RESULTS: usize = 100;

/// Root-only tool service backed by `~/.myco/memory/` (see module docs).
pub struct MemoryService {
    dir_override: Option<PathBuf>,
    rotate_bytes: u64,
    state: tokio::sync::Mutex<IndexState>,
}

struct IndexState {
    index: SearchIndex,
    /// Latest memory file currently indexed.
    file: Option<PathBuf>,
    /// Full text of `file` at last refresh; prefix check detects pure appends.
    text: String,
    /// Pseudo-path key (`<file>#L<line>`) → entry info for formatting hits.
    entries: HashMap<String, EntryInfo>,
}

struct EntryInfo {
    start_line: usize,
    header: String,
}

impl IndexState {
    fn new() -> Self {
        Self {
            index: SearchIndex::new().expect("tantivy ram index"),
            file: None,
            text: String::new(),
            entries: HashMap::new(),
        }
    }
}

impl MemoryService {
    pub fn new() -> Self {
        Self {
            dir_override: None,
            rotate_bytes: MEMORY_ROTATE_BYTES,
            state: tokio::sync::Mutex::new(IndexState::new()),
        }
    }

    #[cfg(test)]
    fn with_dir_for_tests(dir: PathBuf, rotate_bytes: u64) -> Self {
        Self {
            dir_override: Some(dir),
            rotate_bytes,
            state: tokio::sync::Mutex::new(IndexState::new()),
        }
    }

    fn dir(&self) -> Result<PathBuf, String> {
        match &self.dir_override {
            Some(d) => Ok(d.clone()),
            None => Ok(myco_home()?.join("memory")),
        }
    }
}

impl Default for MemoryService {
    fn default() -> Self {
        Self::new()
    }
}

impl ToolService for MemoryService {
    fn tool_specs(&self) -> Vec<generative_model::ToolSpec> {
        vec![generative_model::ToolSpec {
            name: "memory".to_string(),
            description: TOOL_DESCRIPTION.to_string(),
            input_schema: schemars::schema_for!(Input).to_value(),
        }]
    }

    fn dispatch_tool_use(
        self: Arc<Self>,
        tool_use: ToolUse,
        ctx: HostDispatchContext,
    ) -> Async<ToolResult> {
        Box::pin(async move {
            let input: Input = match serde_json::from_value(tool_use.input) {
                Ok(v) => v,
                Err(e) => return ToolResult::err(format!("invalid memory input: {e}")),
            };
            let result = match input.action {
                ActionKind::Append => match input.text.as_deref() {
                    Some(text) => self.append(ctx.agent_id, text),
                    None => Err("append requires text".into()),
                },
                ActionKind::ExactSearch | ActionKind::SemanticSearch => {
                    let semantic = matches!(input.action, ActionKind::SemanticSearch);
                    match input.query.as_deref() {
                        Some(query) => {
                            let limit = input.max_results.unwrap_or(DEFAULT_MAX_RESULTS);
                            self.search(semantic, query, limit).await
                        }
                        None => Err(format!("{} requires query", input.action)),
                    }
                }
            };
            match result {
                Ok(text) => ToolResult::text(text),
                Err(e) => ToolResult::err(e),
            }
        })
    }
}

impl MemoryService {
    /// Append one timestamped entry (creating / rotating the memory file as needed).
    fn append(&self, agent_id: uuid::Uuid, text: &str) -> Result<String, String> {
        let text = text.trim_end();
        if text.trim().is_empty() {
            return Err("append requires non-empty text".into());
        }
        if text.len() > MAX_APPEND_BYTES {
            return Err(format!(
                "entry too large ({} bytes; max {MAX_APPEND_BYTES}). Memory is for short \
                 durable notes; keep long material in files and reference it.",
                text.len()
            ));
        }

        let dir = self.dir()?;
        std::fs::create_dir_all(&dir).map_err(|e| format!("create {}: {e}", dir.display()))?;

        let now = Utc::now();
        let (file, rotated) = match latest_memory_file(&dir) {
            Some(f) if std::fs::metadata(&f).map(|m| m.len()).unwrap_or(0) < self.rotate_bytes => {
                (f, false)
            }
            existing => {
                // Millisecond stamp keeps names unique and lexicographically ordered.
                // A same-millisecond rotation race lands both appends in one file —
                // harmless (append mode), it just rotates a little later.
                let name = format!("memory-{}.md", now.format("%Y%m%dT%H%M%S%3fZ"));
                (dir.join(name), existing.is_some())
            }
        };

        let header = format!(
            "## {} agent={}",
            now.format("%Y-%m-%dT%H:%M:%SZ"),
            &uuid_simple_hex(agent_id)[..8]
        );
        // Leading newline guards against a previous partial line; parsers treat
        // any `## ` line as an entry start, so interleaved appends stay separable.
        let entry = format!("\n{header}\n\n{text}\n");
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&file)
            .map_err(|e| format!("open {}: {e}", file.display()))?;
        f.write_all(entry.as_bytes())
            .map_err(|e| format!("append {}: {e}", file.display()))?;

        Ok(format!(
            "appended\nfile={}{}\n{header}\n",
            file.display(),
            if rotated {
                " (rotated: previous file hit the size cap; searches cover only this file)"
            } else {
                ""
            }
        ))
    }

    async fn search(
        &self,
        semantic: bool,
        query: &str,
        max_results: usize,
    ) -> Result<String, String> {
        let query = query.trim().to_string();
        if query.is_empty() {
            return Err("query must not be empty".into());
        }
        let limit = max_results.clamp(1, MAX_MAX_RESULTS);
        let dir = self.dir()?;

        // Embed the query on the blocking pool, before taking the state lock
        // (same rule as the engine: candle never runs on an executor thread).
        let q_vec = if semantic {
            let q = query.clone();
            Some(
                tokio::task::spawn_blocking(move || embed_for_index(&q))
                    .await
                    .map_err(|e| format!("embed join: {e}"))??,
            )
        } else {
            None
        };

        let mut state = self.state.lock().await;
        let file = refresh_index(&mut state, &dir).await?;
        let hits = match &q_vec {
            Some(v) => state.index.search_semantic(&query, v, None, limit)?,
            None => state.index.search_exact(&query, None, limit)?,
        };
        Ok(format_report(semantic, &file, &hits, &state.entries))
    }
}

// ---------------------------------------------------------------------------
// Store: latest file discovery, entry parsing, index refresh
// ---------------------------------------------------------------------------

fn latest_memory_file(dir: &Path) -> Option<PathBuf> {
    std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.is_file()
                && p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("memory-") && n.ends_with(".md"))
        })
        .max()
}

struct ParsedEntry {
    start_line: usize,
    byte_start: usize,
    text: String,
}

/// Split the file into entries at lines starting with `## `. Non-blank text
/// before the first header (hand edits) becomes one preamble entry.
fn parse_entries(text: &str) -> Vec<ParsedEntry> {
    let mut entries: Vec<ParsedEntry> = Vec::new();
    let mut byte = 0usize;
    for (i, line) in text.split_inclusive('\n').enumerate() {
        let starts_entry = line.starts_with("## ");
        if starts_entry || entries.is_empty() {
            if !starts_entry && line.trim().is_empty() {
                byte += line.len();
                continue;
            }
            entries.push(ParsedEntry {
                start_line: i + 1,
                byte_start: byte,
                text: String::new(),
            });
        }
        entries
            .last_mut()
            .expect("entry pushed above")
            .text
            .push_str(line);
        byte += line.len();
    }
    entries
}

/// Sync the index with the latest memory file; returns that file's path.
///
/// Unchanged file → no work. Pure append (old text is a prefix) → upsert only
/// entries overlapping the new bytes. Anything else (rotation, external edit,
/// truncation) → full rebuild. Reading and embedding run on the blocking pool;
/// the caller's state lock only guards cheap index mutations.
async fn refresh_index(state: &mut IndexState, dir: &Path) -> Result<PathBuf, String> {
    let latest = latest_memory_file(dir).ok_or_else(|| {
        format!(
            "no memory recorded yet (no memory-*.md under {}). Use memory append first.",
            dir.display()
        )
    })?;
    let text = tokio::task::spawn_blocking({
        let latest = latest.clone();
        move || {
            std::fs::read_to_string(&latest).map_err(|e| format!("read {}: {e}", latest.display()))
        }
    })
    .await
    .map_err(|e| format!("read join: {e}"))??;

    let same_file = state.file.as_deref() == Some(latest.as_path());
    if same_file && text == state.text {
        return Ok(latest);
    }

    let pure_append = same_file && text.len() > state.text.len() && text.starts_with(&state.text);
    if !pure_append {
        for key in state.entries.keys() {
            state.index.remove_file(Path::new(key));
        }
        state.entries.clear();
    }
    let indexed_up_to = if pure_append { state.text.len() } else { 0 };

    // Entries entirely inside the already-indexed prefix are unchanged; embed
    // the rest off the executor (best-effort vectors, exact search regardless).
    let changed: Vec<ParsedEntry> = parse_entries(&text)
        .into_iter()
        .filter(|e| e.byte_start + e.text.len() > indexed_up_to)
        .collect();
    let embedded = tokio::task::spawn_blocking(move || {
        changed
            .into_iter()
            .map(|e| {
                let vector = embed_for_index(&e.text).ok();
                (e, vector)
            })
            .collect::<Vec<_>>()
    })
    .await
    .map_err(|e| format!("embed join: {e}"))?;

    for (entry, vector) in embedded {
        let key = format!("{}#L{}", latest.display(), entry.start_line);
        let header = entry.text.lines().next().unwrap_or("").trim().to_string();
        state.index.upsert_file(Path::new(&key), entry.text, vector);
        state.entries.insert(
            key,
            EntryInfo {
                start_line: entry.start_line,
                header,
            },
        );
    }
    state.index.commit()?;
    state.file = Some(latest.clone());
    state.text = text;
    Ok(latest)
}

fn format_report(
    semantic: bool,
    file: &Path,
    hits: &[Hit],
    entries: &HashMap<String, EntryInfo>,
) -> String {
    let mut out = format!(
        "mode={}\nmemory_file={} ({} entries indexed; latest memory file only)\nhits: {}\n",
        if semantic {
            "semantic_candle"
        } else {
            "exact_tantivy"
        },
        file.display(),
        entries.len(),
        hits.len(),
    );
    for (i, h) in hits.iter().enumerate() {
        let key = h.path.to_string_lossy().into_owned();
        let info = entries.get(&key);
        let file_disp = key
            .rsplit_once("#L")
            .map(|(f, _)| f.to_string())
            .unwrap_or_else(|| key.clone());
        // Hit line numbers are entry-relative; report absolute file lines.
        let line = match (h.line_number, info) {
            (Some(n), Some(info)) => info.start_line + n - 1,
            (_, Some(info)) => info.start_line,
            _ => 1,
        };
        out.push_str(&format!(
            "\n[{}] score={:.4} {file_disp}:{line}\n",
            i + 1,
            h.score
        ));
        if let Some(info) = info
            && !info.header.is_empty()
        {
            out.push_str(&format!("  {}\n", info.header));
        }
        let body = h
            .line_text
            .as_deref()
            .map(str::trim_end)
            .filter(|s| !s.is_empty())
            .or_else(|| Some(h.snippet.trim_end()).filter(|s| !s.is_empty()));
        if let Some(body) = body
            && info.is_none_or(|i| i.header != body)
        {
            out.push_str(&format!("  {body}\n"));
        }
    }
    if hits.is_empty() {
        out.push_str("(no hits)\n");
    }
    out
}

#[derive(Clone, Debug, schemars::JsonSchema, serde::Deserialize, serde::Serialize)]
struct Input {
    action: ActionKind,
    /// Entry markdown for `append` (timestamped header is added automatically).
    #[serde(default)]
    text: Option<String>,
    /// Query for `exact_search` / `semantic_search`.
    #[serde(default)]
    query: Option<String>,
    /// Max hits (default 10, max 100).
    #[serde(default)]
    max_results: Option<usize>,
}

#[derive(Clone, Debug, schemars::JsonSchema, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
enum ActionKind {
    Append,
    ExactSearch,
    SemanticSearch,
}

impl std::fmt::Display for ActionKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ActionKind::Append => write!(f, "append"),
            ActionKind::ExactSearch => write!(f, "exact_search"),
            ActionKind::SemanticSearch => write!(f, "semantic_search"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CancelToken;
    use serde_json::json;
    use std::fs;

    fn tmp_dir() -> PathBuf {
        std::env::temp_dir().join(format!("myco-memory-{}", uuid::Uuid::new_v4()))
    }

    fn ctx() -> HostDispatchContext {
        HostDispatchContext::bare(uuid::Uuid::new_v4(), CancelToken::new())
    }

    fn tool_text(r: &ToolResult) -> String {
        r.content
            .iter()
            .filter_map(|c| match c {
                generative_model::Content::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }

    async fn call(svc: &Arc<MemoryService>, input: serde_json::Value) -> ToolResult {
        svc.clone()
            .dispatch_tool_use(
                ToolUse {
                    id: "t".into(),
                    name: "memory".into(),
                    input,
                },
                ctx(),
            )
            .await
    }

    #[tokio::test]
    async fn append_then_exact_search_sees_new_entries() {
        let dir = tmp_dir();
        let svc = Arc::new(MemoryService::with_dir_for_tests(
            dir.clone(),
            MEMORY_ROTATE_BYTES,
        ));

        let r = call(
            &svc,
            json!({"action": "append", "text": "user prefers rebase-first workflow"}),
        )
        .await;
        assert!(!r.is_error, "{r:?}");
        assert!(tool_text(&r).contains("agent="), "{}", tool_text(&r));

        let r = call(&svc, json!({"action": "exact_search", "query": "rebase"})).await;
        assert!(!r.is_error, "{r:?}");
        let text = tool_text(&r);
        assert!(text.contains("rebase-first workflow"), "{text}");
        assert!(
            text.contains(".md:"),
            "hits should be file:line refs: {text}"
        );

        // Second append lands in the same file and is picked up incrementally.
        let r = call(
            &svc,
            json!({"action": "append", "text": "CI runs on unique_token_ci_gadget"}),
        )
        .await;
        assert!(!r.is_error, "{r:?}");
        let r = call(
            &svc,
            json!({"action": "exact_search", "query": "unique_token_ci_gadget"}),
        )
        .await;
        let text = tool_text(&r);
        assert!(text.contains("unique_token_ci_gadget"), "{text}");
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 1, "one memory file");

        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn semantic_search_ranks_relevant_entry() {
        let dir = tmp_dir();
        let svc = Arc::new(MemoryService::with_dir_for_tests(
            dir.clone(),
            MEMORY_ROTATE_BYTES,
        ));

        for text in [
            "Extract PDF text and fill forms with the pdf skill",
            "recipe for banana bread and muffins",
        ] {
            let r = call(&svc, json!({"action": "append", "text": text})).await;
            assert!(!r.is_error, "{r:?}");
        }

        let r = call(
            &svc,
            json!({"action": "semantic_search", "query": "extract documents and forms"}),
        )
        .await;
        assert!(!r.is_error, "{r:?}");
        let text = tool_text(&r);
        let pdf = text.find("PDF").expect("pdf entry surfaced");
        // Banana entry may rank below or be absent; if present it must come after.
        if let Some(banana) = text.find("banana") {
            assert!(pdf < banana, "{text}");
        }

        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn rotation_covers_only_latest_file() {
        let dir = tmp_dir();
        // Cap of 1 byte: any non-empty file rotates on the next append.
        let svc = Arc::new(MemoryService::with_dir_for_tests(dir.clone(), 1));

        let r = call(&svc, json!({"action": "append", "text": "old_token_alpha"})).await;
        assert!(!r.is_error, "{r:?}");
        // Millisecond filename stamps: ensure the rotated file sorts strictly later.
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        let r = call(&svc, json!({"action": "append", "text": "new_token_beta"})).await;
        assert!(!r.is_error, "{r:?}");
        assert!(tool_text(&r).contains("rotated"), "{}", tool_text(&r));
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 2, "two memory files");

        let r = call(
            &svc,
            json!({"action": "exact_search", "query": "new_token_beta"}),
        )
        .await;
        assert!(
            tool_text(&r).contains("new_token_beta"),
            "{}",
            tool_text(&r)
        );

        let r = call(
            &svc,
            json!({"action": "exact_search", "query": "old_token_alpha"}),
        )
        .await;
        assert!(!r.is_error, "{r:?}");
        assert!(
            tool_text(&r).contains("hits: 0"),
            "old file must not be indexed: {}",
            tool_text(&r)
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn external_edit_triggers_full_rebuild() {
        let dir = tmp_dir();
        let svc = Arc::new(MemoryService::with_dir_for_tests(
            dir.clone(),
            MEMORY_ROTATE_BYTES,
        ));

        let r = call(&svc, json!({"action": "append", "text": "stale_token_one"})).await;
        assert!(!r.is_error, "{r:?}");
        let r = call(
            &svc,
            json!({"action": "exact_search", "query": "stale_token_one"}),
        )
        .await;
        assert!(
            tool_text(&r).contains("stale_token_one"),
            "{}",
            tool_text(&r)
        );

        // Hand-prune the file (not a pure append) — index must rebuild.
        let file = latest_memory_file(&dir).unwrap();
        fs::write(
            &file,
            "## 2026-01-01T00:00:00Z agent=deadbeef\n\nfresh_token_two\n",
        )
        .unwrap();

        let r = call(
            &svc,
            json!({"action": "exact_search", "query": "stale_token_one"}),
        )
        .await;
        assert!(tool_text(&r).contains("hits: 0"), "{}", tool_text(&r));
        let r = call(
            &svc,
            json!({"action": "exact_search", "query": "fresh_token_two"}),
        )
        .await;
        assert!(
            tool_text(&r).contains("fresh_token_two"),
            "{}",
            tool_text(&r)
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn errors_are_actionable() {
        let dir = tmp_dir();
        let svc = Arc::new(MemoryService::with_dir_for_tests(
            dir.clone(),
            MEMORY_ROTATE_BYTES,
        ));

        let r = call(&svc, json!({"action": "exact_search", "query": "anything"})).await;
        assert!(r.is_error);
        assert!(
            tool_text(&r).contains("no memory recorded yet"),
            "{}",
            tool_text(&r)
        );

        let r = call(&svc, json!({"action": "append", "text": "  \n "})).await;
        assert!(r.is_error);

        let r = call(&svc, json!({"action": "append"})).await;
        assert!(r.is_error);

        let r = call(&svc, json!({"action": "semantic_search"})).await;
        assert!(r.is_error);
        assert!(
            tool_text(&r).contains("requires query"),
            "{}",
            tool_text(&r)
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn parse_entries_splits_on_headers_and_keeps_offsets() {
        let text = "\n## t1 agent=aa\n\nfirst\n\n## t2 agent=bb\n\nsecond\n";
        let entries = parse_entries(text);
        assert_eq!(entries.len(), 2, "{:?}", entries.len());
        assert_eq!(entries[0].start_line, 2);
        assert!(entries[0].text.contains("first"));
        assert!(!entries[0].text.contains("second"));
        assert_eq!(entries[1].start_line, 6);
        assert!(entries[1].text.contains("second"));
        // Offsets partition the text after the skipped blank preamble.
        assert_eq!(entries[0].byte_start, 1);
        assert_eq!(entries[1].byte_start + entries[1].text.len(), text.len());
    }

    #[test]
    fn memory_absent_from_standard_host_catalog() {
        let names: Vec<String> = crate::host::HostWorker::standard_tool_specs()
            .into_iter()
            .map(|s| s.name)
            .collect();
        assert!(
            !names.contains(&"memory".to_string()),
            "memory is root-only, not a standard host tool: {names:?}"
        );
    }
}
