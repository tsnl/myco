//! Root-only `memory` tool: persistent memory shared across agents and sessions.
//!
//! One memory document per machine under `~/.myco/memory/`, stored
//! maildir-style so no locks (or `O_APPEND`) are needed even on weakly
//! consistent network filesystems: every update is a **write-once** entry file
//! (`{YYYY-MM}/{utc-ms-timestamp}-{id}.md`) and readers resolve the document
//! by listing entries in name (= time) order. Nothing is rewritten in place,
//! so concurrent sessions/subagents cannot conflict. Timestamp shard dirs
//! bound directory size and make GC a directory delete (not automated yet).
//!
//! Search uses a dedicated in-RAM [`SearchIndex`] with one document per entry
//! file, covering only the **latest shard**; older shards stay on disk for
//! bash/grep. The shard listing is diffed before each query so entries from
//! concurrent myco processes appear and hand-deleted files drop out. File
//! reads and MiniLM embedding run on the blocking pool (same rule as the
//! engine: candle never runs on an executor thread).

use std::collections::{HashMap, HashSet};
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

Every update is a **write-once** timestamped entry file (`{YYYY-MM}/{timestamp}-{id}.md`)
— nothing is rewritten in place and no locks are taken, so concurrent sessions and
subagents never conflict. Readers resolve the document by listing entries in name
(= time) order. Searches cover only the latest month shard; older shards stay on disk
for bash/grep (GC = delete a shard dir; not automated yet).

Actions:
- append: add a timestamped entry (markdown). Keep entries short and durable — user
  preferences, project facts, decisions, gotchas. Not a scratchpad: use session_meta
  set_scratchpad for session-local notes.
- exact_search: Tantivy full-text over entries of the latest shard (identifiers,
  literal phrases). Hits are file:line refs you can open with the editor.
- semantic_search: Candle MiniLM cosine over per-entry embeddings (intent queries like
  "how does the user prefer commits formatted").
"#;

const MAX_APPEND_BYTES: usize = 16 * 1024;
const DEFAULT_MAX_RESULTS: usize = 10;
const MAX_MAX_RESULTS: usize = 100;

/// Root-only tool service backed by `~/.myco/memory/` (see module docs).
pub struct MemoryService {
    dir_override: Option<PathBuf>,
    state: tokio::sync::Mutex<IndexState>,
}

struct IndexState {
    index: SearchIndex,
    /// Shard dir currently indexed.
    shard: Option<PathBuf>,
    /// Indexed entry files: path key → header line (for hit display).
    entries: HashMap<String, String>,
}

impl IndexState {
    fn new() -> Self {
        Self {
            index: SearchIndex::new().expect("tantivy ram index"),
            shard: None,
            entries: HashMap::new(),
        }
    }
}

impl MemoryService {
    pub fn new() -> Self {
        Self {
            dir_override: None,
            state: tokio::sync::Mutex::new(IndexState::new()),
        }
    }

    #[cfg(test)]
    fn with_dir_for_tests(dir: PathBuf) -> Self {
        Self {
            dir_override: Some(dir),
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
    /// Write one entry as a new unique file (never touches existing files).
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

        let now = Utc::now();
        let shard = self.dir()?.join(now.format("%Y-%m").to_string());
        std::fs::create_dir_all(&shard).map_err(|e| format!("create {}: {e}", shard.display()))?;

        let header = format!(
            "## {} agent={}",
            now.format("%Y-%m-%dT%H:%M:%SZ"),
            &uuid_simple_hex(agent_id)[..8]
        );
        // Millisecond stamp keeps names in time order; the random suffix makes
        // them unique across hosts/processes without O_EXCL coordination.
        let name = format!(
            "{}-{}.md",
            now.format("%Y%m%dT%H%M%S%3fZ"),
            &uuid_simple_hex(uuid::Uuid::new_v4())[..8]
        );
        let path = shard.join(name);
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|e| format!("create {}: {e}", path.display()))?;
        f.write_all(format!("{header}\n\n{text}\n").as_bytes())
            .map_err(|e| format!("write {}: {e}", path.display()))?;

        Ok(format!("appended\nfile={}\n{header}\n", path.display()))
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
        let shard = refresh_index(&mut state, dir).await?;
        let hits = match &q_vec {
            Some(v) => state.index.search_semantic(&query, v, None, limit)?,
            None => state.index.search_exact(&query, None, limit)?,
        };
        Ok(format_report(semantic, &shard, &hits, &state.entries))
    }
}

// ---------------------------------------------------------------------------
// Store: shard/entry listing, index refresh
// ---------------------------------------------------------------------------

/// Entry files in `shard`, any order (`{timestamp}-{id}.md`; leading digit).
fn entry_files(shard: &Path) -> Vec<PathBuf> {
    let Ok(read) = std::fs::read_dir(shard) else {
        return Vec::new();
    };
    read.flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.is_file()
                && p.file_name().and_then(|n| n.to_str()).is_some_and(|n| {
                    n.ends_with(".md") && n.starts_with(|c: char| c.is_ascii_digit())
                })
        })
        .collect()
}

/// Newest timestamp shard that contains entry files, with its entries.
fn latest_populated_shard(dir: &Path) -> Option<(PathBuf, Vec<PathBuf>)> {
    let read = std::fs::read_dir(dir).ok()?;
    let mut shards: Vec<PathBuf> = read
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.is_dir()
                && p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.chars().all(|c| c.is_ascii_digit() || c == '-'))
        })
        .collect();
    shards.sort();
    for shard in shards.into_iter().rev() {
        let files = entry_files(&shard);
        if !files.is_empty() {
            return Some((shard, files));
        }
    }
    None
}

fn path_key(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

/// Sync the index with the latest shard; returns that shard's path.
///
/// Entry files are write-once, so the diff is by name only: unseen files are
/// read and embedded on the blocking pool, deleted files drop out, and a shard
/// change (new month, GC) resets the index. The caller's state lock only
/// guards cheap index mutations.
async fn refresh_index(state: &mut IndexState, dir: PathBuf) -> Result<PathBuf, String> {
    let listed = tokio::task::spawn_blocking({
        let dir = dir.clone();
        move || latest_populated_shard(&dir)
    })
    .await
    .map_err(|e| format!("list join: {e}"))?;
    let Some((shard, files)) = listed else {
        return Err(format!(
            "no memory recorded yet (no entry files under {}). Use memory append first.",
            dir.display()
        ));
    };

    if state.shard.as_ref() != Some(&shard) {
        for key in state.entries.keys() {
            state.index.remove_file(Path::new(key));
        }
        state.entries.clear();
        state.shard = Some(shard.clone());
    }

    let current: HashSet<String> = files.iter().map(|p| path_key(p)).collect();
    let gone: Vec<String> = state
        .entries
        .keys()
        .filter(|k| !current.contains(*k))
        .cloned()
        .collect();
    for key in &gone {
        state.index.remove_file(Path::new(key));
        state.entries.remove(key);
    }

    let new_files: Vec<PathBuf> = files
        .into_iter()
        .filter(|p| !state.entries.contains_key(&path_key(p)))
        .collect();
    if new_files.is_empty() && gone.is_empty() {
        return Ok(shard);
    }

    // Read + embed off the executor (best-effort vectors; exact regardless).
    let loaded = tokio::task::spawn_blocking(move || {
        new_files
            .into_iter()
            .filter_map(|p| {
                let text = std::fs::read_to_string(&p).ok()?;
                let vector = embed_for_index(&text).ok();
                Some((p, text, vector))
            })
            .collect::<Vec<_>>()
    })
    .await
    .map_err(|e| format!("embed join: {e}"))?;

    for (path, text, vector) in loaded {
        let header = text.lines().next().unwrap_or("").trim().to_string();
        let key = path_key(&path);
        state.index.upsert_file(&path, text, vector);
        state.entries.insert(key, header);
    }
    state.index.commit()?;
    Ok(shard)
}

fn format_report(
    semantic: bool,
    shard: &Path,
    hits: &[Hit],
    entries: &HashMap<String, String>,
) -> String {
    let mut out = format!(
        "mode={}\nmemory_shard={} ({} entries indexed; latest shard only)\nhits: {}\n",
        if semantic {
            "semantic_candle"
        } else {
            "exact_tantivy"
        },
        shard.display(),
        entries.len(),
        hits.len(),
    );
    for (i, h) in hits.iter().enumerate() {
        let header = entries.get(&path_key(&h.path)).map(String::as_str);
        out.push_str(&format!(
            "\n[{}] score={:.4} {}:{}\n",
            i + 1,
            h.score,
            h.path.display(),
            h.line_number.unwrap_or(1),
        ));
        if let Some(header) = header
            && !header.is_empty()
        {
            out.push_str(&format!("  {header}\n"));
        }
        let body = h
            .line_text
            .as_deref()
            .map(str::trim_end)
            .filter(|s| !s.is_empty())
            .or_else(|| Some(h.snippet.trim_end()).filter(|s| !s.is_empty()));
        if let Some(body) = body
            && header != Some(body)
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
    async fn each_append_is_a_new_write_once_file_and_searchable() {
        let dir = tmp_dir();
        let svc = Arc::new(MemoryService::with_dir_for_tests(dir.clone()));

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

        // Second append is a new file in the same shard, picked up incrementally.
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
        assert!(
            tool_text(&r).contains("unique_token_ci_gadget"),
            "{}",
            tool_text(&r)
        );

        let shards: Vec<_> = fs::read_dir(&dir).unwrap().flatten().collect();
        assert_eq!(shards.len(), 1, "one shard dir");
        assert_eq!(
            entry_files(&shards[0].path()).len(),
            2,
            "one file per append"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn semantic_search_ranks_relevant_entry() {
        let dir = tmp_dir();
        let svc = Arc::new(MemoryService::with_dir_for_tests(dir.clone()));

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
    async fn search_covers_only_latest_shard() {
        let dir = tmp_dir();
        let svc = Arc::new(MemoryService::with_dir_for_tests(dir.clone()));

        // Old shard left behind by an earlier month.
        let old = dir.join("2000-01");
        fs::create_dir_all(&old).unwrap();
        fs::write(
            old.join("20000101T000000000Z-deadbeef.md"),
            "## 2000-01-01T00:00:00Z agent=deadbeef\n\nold_token_alpha\n",
        )
        .unwrap();

        let r = call(&svc, json!({"action": "append", "text": "new_token_beta"})).await;
        assert!(!r.is_error, "{r:?}");

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
            "old shard must not be indexed: {}",
            tool_text(&r)
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn deleted_entry_files_drop_out_of_the_index() {
        let dir = tmp_dir();
        let svc = Arc::new(MemoryService::with_dir_for_tests(dir.clone()));

        let r = call(&svc, json!({"action": "append", "text": "stale_token_one"})).await;
        assert!(!r.is_error, "{r:?}");
        let keep = call(&svc, json!({"action": "append", "text": "kept_token_two"})).await;
        assert!(!keep.is_error, "{keep:?}");

        let r = call(
            &svc,
            json!({"action": "exact_search", "query": "stale_token_one"}),
        )
        .await;
        let text = tool_text(&r);
        assert!(text.contains("stale_token_one"), "{text}");
        let stale_file = text
            .lines()
            .find_map(|l| l.split_whitespace().find(|w| w.contains(".md:")))
            .and_then(|w| w.rsplit_once(':').map(|(p, _)| p.to_string()))
            .expect("hit path in report");

        // GC by hand: deleting an entry file drops it on the next search.
        fs::remove_file(&stale_file).unwrap();
        let r = call(
            &svc,
            json!({"action": "exact_search", "query": "stale_token_one"}),
        )
        .await;
        assert!(tool_text(&r).contains("hits: 0"), "{}", tool_text(&r));
        let r = call(
            &svc,
            json!({"action": "exact_search", "query": "kept_token_two"}),
        )
        .await;
        assert!(
            tool_text(&r).contains("kept_token_two"),
            "{}",
            tool_text(&r)
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn errors_are_actionable() {
        let dir = tmp_dir();
        let svc = Arc::new(MemoryService::with_dir_for_tests(dir.clone()));

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
