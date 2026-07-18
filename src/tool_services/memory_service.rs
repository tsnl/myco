//! Root-only `memory` tool: persistent memory shared across agents and sessions.
//!
//! The memory document is a **set of atomic entries**: immutable, UUIDed,
//! timestamped, titled. Entries are only ever created or deleted — nothing is
//! rewritten in place and no locks are taken, so concurrent sessions and
//! subagents are safe even on weakly consistent network filesystems
//! (maildir's write-once-unique-name pattern). Each entry is one file,
//! `~/.myco/memory/{YYYY-MM}/{utc-ms-timestamp}-{uuid}.md`:
//!
//! ```text
//! {uuid}
//! {UTC timestamp} ({local timestamp} local) agent={hex8}
//! # {title}
//!
//! {body}
//! ```
//!
//! Readers resolve the document by listing entries in name (= time) order,
//! joining with two blank lines. The timestamp shard dirs are a storage
//! layout detail (they keep directories small) with no visibility semantics:
//! **every entry stays indexed and readable until explicitly deleted**.
//! GC/pruning is deliberately out of scope for now.
//!
//! Search uses a dedicated in-RAM [`SearchIndex`] with one document per
//! entry, diffed against the store listing before each query so entries from
//! concurrent myco processes appear and deleted files drop out. File reads
//! and MiniLM embedding run on the blocking pool (same rule as the engine:
//! candle never runs on an executor thread).

use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::{Local, Utc};

use crate::core::Async;
use crate::generative_model::{self, ToolResult, ToolUse};
use crate::session::{myco_home, normalize_title, uuid_simple_hex};
use crate::text_search::{Hit, SearchIndex, embed_for_index};

use super::{HostDispatchContext, ToolService};

const TOOL_DESCRIPTION: &str = r#"
Persistent memory shared across agents and sessions (one document per machine, under
`~/.myco/memory/`).

The document is a set of **atomic entries** — immutable, UUIDed, timestamped, titled.
Entries are only ever created or deleted (each is a write-once file; nothing is rewritten
in place, no locks), so concurrent sessions and subagents never conflict. Every entry
stays indexed and readable **until explicitly deleted** — to correct a stale fact, append
the corrected entry and delete the old one.

Actions:
- append: add an entry. Requires `title` (short one-line label) and `text` (markdown
  body); UUID + timestamp are added automatically and the id is returned. Keep entries
  short, durable, one fact each — user preferences, project facts, decisions, gotchas.
  Not a scratchpad: use session_meta set_scratchpad for session-local notes.
- delete: remove one entry by `id` (uuid from append/list/search results; unique prefix
  ok). The deleted entry is echoed back, so a mistaken delete can be re-appended.
- list: compact index of all entries (id, timestamp, title), newest first.
- read: full entries — the document in time order (last `max_results`, default 50), or
  one entry via `id`.
- search: query entries. `mode` = "exact" (default; Tantivy full-text — identifiers,
  literal phrases) or "semantic" (Candle MiniLM cosine — intent queries like "how does
  the user prefer commits formatted"). Hits show entry ids for read/delete.
"#;

const MAX_APPEND_BYTES: usize = 16 * 1024;
const DEFAULT_MAX_RESULTS: usize = 10;
const DEFAULT_READ_ENTRIES: usize = 50;
const MAX_MAX_RESULTS: usize = 1000;

/// Root-only tool service backed by `~/.myco/memory/` (see module docs).
pub struct MemoryService {
    dir_override: Option<PathBuf>,
    state: tokio::sync::Mutex<IndexState>,
}

struct IndexState {
    index: SearchIndex,
    /// Indexed entry files: path key → entry metadata (for hit display).
    entries: HashMap<String, EntryMeta>,
}

struct EntryMeta {
    id: String,
    /// Timestamp/attribution line (entry line 2).
    stamp: String,
    title: String,
}

impl IndexState {
    fn new() -> Self {
        Self {
            index: SearchIndex::new().expect("tantivy ram index"),
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
                ActionKind::Append => match (input.title.as_deref(), input.text.as_deref()) {
                    (Some(title), Some(text)) => self.append(ctx.agent_id, title, text),
                    _ => Err("append requires title (short one-line label) and text".into()),
                },
                ActionKind::Delete => match input.id.as_deref() {
                    Some(id) => self.delete(id).await,
                    None => Err(
                        "delete requires id (entry uuid from append/list/search results; \
                         unique prefix ok)"
                            .into(),
                    ),
                },
                ActionKind::List => self.list().await,
                ActionKind::Read => {
                    let max = input.max_results.unwrap_or(DEFAULT_READ_ENTRIES);
                    self.read(input.id.as_deref(), max).await
                }
                ActionKind::Search => match input.query.as_deref() {
                    Some(query) => {
                        let semantic = matches!(input.mode, Some(SearchMode::Semantic));
                        let limit = input.max_results.unwrap_or(DEFAULT_MAX_RESULTS);
                        self.search(semantic, query, limit).await
                    }
                    None => Err("search requires query".into()),
                },
            };
            match result {
                Ok(text) => ToolResult::text(text),
                Err(e) => ToolResult::err(e),
            }
        })
    }
}

impl MemoryService {
    /// Create one immutable entry as a new unique file.
    fn append(&self, agent_id: uuid::Uuid, title: &str, text: &str) -> Result<String, String> {
        let title = normalize_title(title).map_err(|e| format!("append title: {e}"))?;
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

        let id = uuid_simple_hex(uuid::Uuid::new_v4());
        let stamp = format!(
            "{} ({} local) agent={}",
            now.format("%Y-%m-%dT%H:%M:%SZ"),
            now.with_timezone(&Local).format("%Y-%m-%dT%H:%M:%S%:z"),
            &uuid_simple_hex(agent_id)[..8]
        );
        // Millisecond stamp keeps names in time order; the uuid makes them
        // unique across hosts/processes and is the entry's stable id.
        let name = format!("{}-{id}.md", now.format("%Y%m%dT%H%M%S%3fZ"));
        let path = shard.join(name);
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|e| format!("create {}: {e}", path.display()))?;
        f.write_all(format!("{id}\n{stamp}\n# {title}\n\n{text}\n").as_bytes())
            .map_err(|e| format!("write {}: {e}", path.display()))?;

        Ok(format!(
            "appended\nid={id}\ntitle={title}\nfile={}\n{stamp}\n",
            path.display()
        ))
    }

    /// Delete one entry by uuid (or unique prefix), echoing its content.
    async fn delete(&self, id: &str) -> Result<String, String> {
        let dir = self.dir()?;
        let id = id.to_string();
        tokio::task::spawn_blocking(move || {
            let file = resolve_entry_file(&dir, &id)?;
            let content = std::fs::read_to_string(&file).unwrap_or_default();
            std::fs::remove_file(&file).map_err(|e| format!("delete {}: {e}", file.display()))?;
            Ok(format!(
                "deleted {}\n--- deleted entry (append it again to restore) ---\n{content}",
                file.display()
            ))
        })
        .await
        .map_err(|e| format!("delete join: {e}"))?
    }

    /// Compact index of all entries, newest first.
    async fn list(&self) -> Result<String, String> {
        let dir = self.dir()?;
        let loaded = {
            let dir = dir.clone();
            tokio::task::spawn_blocking(move || load_all_entries(&dir))
                .await
                .map_err(|e| format!("list join: {e}"))??
        };
        let mut out = format!(
            "memory entries: {} ({}; newest first)\n",
            loaded.len(),
            dir.display()
        );
        for e in loaded.iter().rev() {
            out.push_str(&format!("  {}  {}  {}\n", e.id, e.utc_stamp(), e.title));
        }
        Ok(out)
    }

    /// Full entries: the document in time order, or one entry by id.
    async fn read(&self, id: Option<&str>, max_entries: usize) -> Result<String, String> {
        let dir = self.dir()?;
        if let Some(id) = id {
            let id = id.to_string();
            return tokio::task::spawn_blocking(move || {
                let file = resolve_entry_file(&dir, &id)?;
                std::fs::read_to_string(&file).map_err(|e| format!("read {}: {e}", file.display()))
            })
            .await
            .map_err(|e| format!("read join: {e}"))?;
        }

        let max = max_entries.clamp(1, MAX_MAX_RESULTS);
        let loaded = {
            let dir = dir.clone();
            tokio::task::spawn_blocking(move || load_all_entries(&dir))
                .await
                .map_err(|e| format!("read join: {e}"))??
        };
        let total = loaded.len();
        let shown = &loaded[total.saturating_sub(max)..];
        let mut out = format!(
            "memory document: showing {} of {} entries ({}; time order)\n---\n",
            shown.len(),
            total,
            dir.display()
        );
        out.push_str(
            &shown
                .iter()
                .map(|e| e.text.trim_end().to_string())
                .collect::<Vec<_>>()
                .join("\n\n\n"),
        );
        out.push('\n');
        Ok(out)
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
        refresh_index(&mut state, dir.clone()).await?;
        let hits = match &q_vec {
            Some(v) => state.index.search_semantic(&query, v, None, limit)?,
            None => state.index.search_exact(&query, None, limit)?,
        };
        Ok(format_report(semantic, &dir, &hits, &state.entries))
    }
}

// ---------------------------------------------------------------------------
// Store: entry listing, id resolve, index refresh
// ---------------------------------------------------------------------------

/// Timestamp shard dirs under `dir` (storage layout only; no visibility semantics).
fn shard_dirs(dir: &Path) -> Vec<PathBuf> {
    let Ok(read) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    read.flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.is_dir()
                && p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.chars().all(|c| c.is_ascii_digit() || c == '-'))
        })
        .collect()
}

/// Entry files in `shard`, any order (`{timestamp}-{uuid}.md`; leading digit).
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

/// Every entry file in the store, sorted by filename (= time order).
fn all_entry_files(dir: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = shard_dirs(dir)
        .iter()
        .flat_map(|s| entry_files(s))
        .collect();
    files.sort_by(|a, b| a.file_name().cmp(&b.file_name()));
    files
}

/// Entry uuid from a `{timestamp}-{uuid}.md` filename.
fn entry_uuid(path: &Path) -> Option<String> {
    let name = path.file_name()?.to_str()?.strip_suffix(".md")?;
    let (_, id) = name.rsplit_once('-')?;
    (!id.is_empty() && id.chars().all(|c| c.is_ascii_hexdigit())).then(|| id.to_string())
}

/// One entry file whose uuid matches `id` (unique prefix ok).
fn resolve_entry_file(dir: &Path, id: &str) -> Result<PathBuf, String> {
    let id = id.trim().to_ascii_lowercase();
    if id.len() < 4 || !id.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(format!(
            "entry id must be a hex uuid (unique prefix ok, min 4 chars), got {id:?}"
        ));
    }
    let matches: Vec<PathBuf> = all_entry_files(dir)
        .into_iter()
        .filter(|f| entry_uuid(f).is_some_and(|u| u.starts_with(&id)))
        .collect();
    match matches.as_slice() {
        [] => Err(format!("no entry matching id {id:?}")),
        [one] => Ok(one.clone()),
        many => Err(format!(
            "ambiguous id {id:?}; candidates: {}",
            many.iter()
                .filter_map(|p| entry_uuid(p))
                .take(8)
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

struct LoadedEntry {
    id: String,
    stamp: String,
    title: String,
    text: String,
}

impl LoadedEntry {
    /// UTC part of the stamp line (first whitespace token).
    fn utc_stamp(&self) -> &str {
        self.stamp.split_whitespace().next().unwrap_or("")
    }
}

fn parse_entry(path: &Path, text: String) -> LoadedEntry {
    // Filename is authoritative for the id; body line 1 is the readable fallback.
    let id = entry_uuid(path)
        .or_else(|| text.lines().next().map(|l| l.trim().to_string()))
        .unwrap_or_default();
    let stamp = text.lines().nth(1).unwrap_or("").trim().to_string();
    let title_line = text.lines().nth(2).unwrap_or("").trim();
    let title = title_line
        .strip_prefix("# ")
        .unwrap_or(title_line)
        .to_string();
    LoadedEntry {
        id,
        stamp,
        title,
        text,
    }
}

/// All entries in time (= name) order, fully loaded. Errors when the store is empty.
fn load_all_entries(dir: &Path) -> Result<Vec<LoadedEntry>, String> {
    let files = all_entry_files(dir);
    if files.is_empty() {
        return Err(format!(
            "no memory recorded yet (no entry files under {}). Use memory append first.",
            dir.display()
        ));
    }
    Ok(files
        .into_iter()
        .filter_map(|p| {
            let text = std::fs::read_to_string(&p).ok()?;
            Some(parse_entry(&p, text))
        })
        .collect())
}

fn path_key(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

/// Sync the index with the store (all entries, every shard).
///
/// Entry files are immutable, so the diff is by name only: unseen files are
/// read and embedded on the blocking pool, deleted files drop out. The
/// caller's state lock only guards cheap index mutations.
async fn refresh_index(state: &mut IndexState, dir: PathBuf) -> Result<(), String> {
    let files = tokio::task::spawn_blocking({
        let dir = dir.clone();
        move || all_entry_files(&dir)
    })
    .await
    .map_err(|e| format!("list join: {e}"))?;
    if files.is_empty() {
        return Err(format!(
            "no memory recorded yet (no entry files under {}). Use memory append first.",
            dir.display()
        ));
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
        return Ok(());
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
        let entry = parse_entry(&path, text.clone());
        let key = path_key(&path);
        state.index.upsert_file(&path, text, vector);
        state.entries.insert(
            key,
            EntryMeta {
                id: entry.id,
                stamp: entry.stamp,
                title: entry.title,
            },
        );
    }
    state.index.commit()?;
    Ok(())
}

fn format_report(
    semantic: bool,
    dir: &Path,
    hits: &[Hit],
    entries: &HashMap<String, EntryMeta>,
) -> String {
    let mut out = format!(
        "mode={}\nmemory={} ({} entries indexed)\nhits: {}\n",
        if semantic {
            "semantic_candle"
        } else {
            "exact_tantivy"
        },
        dir.display(),
        entries.len(),
        hits.len(),
    );
    for (i, h) in hits.iter().enumerate() {
        let meta = entries.get(&path_key(&h.path));
        out.push_str(&format!(
            "\n[{}] score={:.4} id={}  {}\n",
            i + 1,
            h.score,
            meta.map(|m| m.id.as_str()).unwrap_or("?"),
            meta.map(|m| m.title.as_str()).unwrap_or(""),
        ));
        let stamp = meta.map(|m| m.stamp.as_str()).unwrap_or("");
        if !stamp.is_empty() {
            out.push_str(&format!("  {stamp}\n"));
        }
        let body = h
            .line_text
            .as_deref()
            .map(str::trim_end)
            .filter(|s| !s.is_empty())
            .or_else(|| Some(h.snippet.trim_end()).filter(|s| !s.is_empty()));
        // Skip echoing lines the header already shows (title/stamp/id rows).
        if let Some(body) = body
            && body != stamp
            && meta.is_none_or(|m| {
                let plain = body.strip_prefix("# ").unwrap_or(body);
                plain != m.title && body != m.id
            })
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
    /// Short one-line label for `append` (required there).
    #[serde(default)]
    title: Option<String>,
    /// Entry markdown body for `append` (UUID + timestamp added automatically).
    #[serde(default)]
    text: Option<String>,
    /// Entry id for `delete` / `read` (uuid from results; unique prefix ok).
    #[serde(default)]
    id: Option<String>,
    /// Query for `search`.
    #[serde(default)]
    query: Option<String>,
    /// Search mode: exact (default) or semantic.
    #[serde(default)]
    mode: Option<SearchMode>,
    /// Max search hits (default 10) / max `read` entries (default 50).
    #[serde(default)]
    max_results: Option<usize>,
}

#[derive(Clone, Debug, schemars::JsonSchema, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
enum ActionKind {
    Append,
    Delete,
    List,
    Read,
    Search,
}

#[derive(Clone, Debug, schemars::JsonSchema, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
enum SearchMode {
    Exact,
    Semantic,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CancelToken;
    use serde_json::json;
    use std::fs;
    use std::time::Duration;

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

    fn appended_id(r: &ToolResult) -> String {
        tool_text(r)
            .lines()
            .find_map(|l| l.strip_prefix("id=").map(str::to_string))
            .expect("append result carries id=")
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

    async fn append(svc: &Arc<MemoryService>, title: &str, text: &str) -> ToolResult {
        let r = call(
            svc,
            json!({"action": "append", "title": title, "text": text}),
        )
        .await;
        assert!(!r.is_error, "{r:?}");
        r
    }

    /// Entry file in an old month shard, as an earlier session would have left it.
    fn write_old_shard_entry(dir: &Path) {
        let old = dir.join("2000-01");
        fs::create_dir_all(&old).unwrap();
        fs::write(
            old.join("20000101T000000000Z-deadbeefdeadbeefdeadbeefdeadbeef.md"),
            "deadbeefdeadbeefdeadbeefdeadbeef\n\
             2000-01-01T00:00:00Z (2000-01-01T00:00:00+00:00 local) agent=deadbeef\n\
             # Old fact\n\
             \n\
             old_token_alpha\n",
        )
        .unwrap();
    }

    #[tokio::test]
    async fn append_makes_titled_uuid_entries_and_search_is_entry_shaped() {
        let dir = tmp_dir();
        let svc = Arc::new(MemoryService::with_dir_for_tests(dir.clone()));

        let r = append(&svc, "Git workflow", "user prefers rebase-first workflow").await;
        let id = appended_id(&r);
        assert_eq!(id.len(), 32, "{id}");
        assert!(
            tool_text(&r).contains("title=Git workflow"),
            "{}",
            tool_text(&r)
        );

        let r = call(&svc, json!({"action": "search", "query": "rebase"})).await;
        assert!(!r.is_error, "{r:?}");
        let text = tool_text(&r);
        assert!(text.contains("rebase-first workflow"), "{text}");
        assert!(text.contains(&format!("id={id}")), "{text}");
        assert!(text.contains("Git workflow"), "hits show titles: {text}");
        assert!(
            !text.contains(".md:"),
            "search results are entry-shaped, not file refs: {text}"
        );

        // Second append is a new file, picked up incrementally.
        append(&svc, "CI host", "CI runs on unique_token_ci_gadget").await;
        let r = call(
            &svc,
            json!({"action": "search", "query": "unique_token_ci_gadget"}),
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
    async fn entries_persist_across_shards_until_deleted() {
        let dir = tmp_dir();
        let svc = Arc::new(MemoryService::with_dir_for_tests(dir.clone()));

        write_old_shard_entry(&dir);
        append(&svc, "New fact", "new_token_beta").await;

        // Old-shard entries are first-class: searchable, listed, and read in order.
        let r = call(
            &svc,
            json!({"action": "search", "query": "old_token_alpha"}),
        )
        .await;
        let text = tool_text(&r);
        assert!(text.contains("old_token_alpha"), "{text}");
        assert!(text.contains("Old fact"), "{text}");
        let r = call(&svc, json!({"action": "list"})).await;
        let text = tool_text(&r);
        assert!(text.contains("memory entries: 2"), "{text}");
        assert!(
            text.contains("Old fact") && text.contains("New fact"),
            "{text}"
        );
        let r = call(&svc, json!({"action": "read"})).await;
        let text = tool_text(&r);
        let old = text.find("old_token_alpha").unwrap();
        let new = text.find("new_token_beta").unwrap();
        assert!(old < new, "time order across shards: {text}");

        // Explicit delete is the only way an entry leaves the document.
        let r = call(&svc, json!({"action": "delete", "id": "deadbeefdead"})).await;
        assert!(!r.is_error, "{r:?}");
        assert!(
            tool_text(&r).contains("old_token_alpha"),
            "{}",
            tool_text(&r)
        );
        let r = call(
            &svc,
            json!({"action": "search", "query": "old_token_alpha"}),
        )
        .await;
        assert!(tool_text(&r).contains("hits: 0"), "{}", tool_text(&r));
        let r = call(&svc, json!({"action": "list"})).await;
        assert!(
            tool_text(&r).contains("memory entries: 1"),
            "{}",
            tool_text(&r)
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn list_is_newest_first_with_ids_and_titles() {
        let dir = tmp_dir();
        let svc = Arc::new(MemoryService::with_dir_for_tests(dir.clone()));

        let a = append(&svc, "Alpha fact", "alpha body").await;
        tokio::time::sleep(Duration::from_millis(10)).await;
        let b = append(&svc, "Beta fact", "beta body").await;

        let r = call(&svc, json!({"action": "list"})).await;
        assert!(!r.is_error, "{r:?}");
        let text = tool_text(&r);
        assert!(text.contains("memory entries: 2"), "{text}");
        assert!(text.contains(&appended_id(&a)), "{text}");
        assert!(text.contains(&appended_id(&b)), "{text}");
        let alpha = text.find("Alpha fact").unwrap();
        let beta = text.find("Beta fact").unwrap();
        assert!(beta < alpha, "newest first: {text}");
        assert!(!text.contains("alpha body"), "list is compact: {text}");

        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn read_returns_document_in_time_order_or_one_entry() {
        let dir = tmp_dir();
        let svc = Arc::new(MemoryService::with_dir_for_tests(dir.clone()));

        let a = append(&svc, "Alpha fact", "alpha body").await;
        tokio::time::sleep(Duration::from_millis(10)).await;
        append(&svc, "Beta fact", "beta body").await;

        let r = call(&svc, json!({"action": "read"})).await;
        assert!(!r.is_error, "{r:?}");
        let text = tool_text(&r);
        assert!(text.contains("showing 2 of 2"), "{text}");
        let alpha = text.find("alpha body").unwrap();
        let beta = text.find("beta body").unwrap();
        assert!(alpha < beta, "time order: {text}");
        assert!(
            text.contains("\n\n\n"),
            "two blank lines between entries: {text}"
        );

        // Tail cap keeps the most recent entries.
        let r = call(&svc, json!({"action": "read", "max_results": 1})).await;
        let text = tool_text(&r);
        assert!(text.contains("showing 1 of 2"), "{text}");
        assert!(
            text.contains("beta body") && !text.contains("alpha body"),
            "{text}"
        );

        // Read one entry by id (prefix ok).
        let a_id = appended_id(&a);
        let r = call(&svc, json!({"action": "read", "id": &a_id[..8]})).await;
        let text = tool_text(&r);
        assert!(
            text.contains("alpha body") && !text.contains("beta body"),
            "{text}"
        );
        assert!(text.contains("# Alpha fact"), "{text}");

        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn delete_by_id_removes_entry_and_echoes_it() {
        let dir = tmp_dir();
        let svc = Arc::new(MemoryService::with_dir_for_tests(dir.clone()));

        let a = append(&svc, "Stale", "stale_token_one").await;
        let a_id = appended_id(&a);
        let b = append(&svc, "Kept", "kept_token_two").await;
        let b_id = appended_id(&b);

        // Delete by full uuid; result echoes the entry for recovery.
        let r = call(&svc, json!({"action": "delete", "id": a_id})).await;
        assert!(!r.is_error, "{r:?}");
        let text = tool_text(&r);
        assert!(text.contains("deleted"), "{text}");
        assert!(text.contains("stale_token_one"), "{text}");

        let r = call(
            &svc,
            json!({"action": "search", "query": "stale_token_one"}),
        )
        .await;
        assert!(tool_text(&r).contains("hits: 0"), "{}", tool_text(&r));
        let r = call(&svc, json!({"action": "search", "query": "kept_token_two"})).await;
        assert!(
            tool_text(&r).contains("kept_token_two"),
            "{}",
            tool_text(&r)
        );

        // Unique prefix works; a second delete of the same id is a clean error.
        let r = call(&svc, json!({"action": "delete", "id": &b_id[..8]})).await;
        assert!(!r.is_error, "{r:?}");
        let r = call(&svc, json!({"action": "delete", "id": b_id})).await;
        assert!(r.is_error);
        assert!(
            tool_text(&r).contains("no entry matching"),
            "{}",
            tool_text(&r)
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn semantic_search_ranks_relevant_entry() {
        let dir = tmp_dir();
        let svc = Arc::new(MemoryService::with_dir_for_tests(dir.clone()));

        append(
            &svc,
            "PDF extraction",
            "Extract PDF text and fill forms with the pdf skill",
        )
        .await;
        append(&svc, "Baking", "recipe for banana bread and muffins").await;

        let r = call(
            &svc,
            json!({"action": "search", "mode": "semantic", "query": "extract documents and forms"}),
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
    async fn hand_deleted_entry_files_drop_out_of_the_index() {
        let dir = tmp_dir();
        let svc = Arc::new(MemoryService::with_dir_for_tests(dir.clone()));

        let a = append(&svc, "Stale", "stale_token_one").await;
        append(&svc, "Kept", "kept_token_two").await;

        let r = call(
            &svc,
            json!({"action": "search", "query": "stale_token_one"}),
        )
        .await;
        assert!(
            tool_text(&r).contains("stale_token_one"),
            "{}",
            tool_text(&r)
        );

        // GC by hand (bash rm): the file drops out on the next search.
        let stale_file = tool_text(&a)
            .lines()
            .find_map(|l| l.strip_prefix("file=").map(str::to_string))
            .expect("append result carries file=");
        fs::remove_file(&stale_file).unwrap();
        let r = call(
            &svc,
            json!({"action": "search", "query": "stale_token_one"}),
        )
        .await;
        assert!(tool_text(&r).contains("hits: 0"), "{}", tool_text(&r));
        let r = call(&svc, json!({"action": "search", "query": "kept_token_two"})).await;
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

        for action in ["search", "list", "read"] {
            let r = call(&svc, json!({"action": action, "query": "anything"})).await;
            assert!(r.is_error, "{action}");
            assert!(
                tool_text(&r).contains("no memory recorded yet"),
                "{action}: {}",
                tool_text(&r)
            );
        }

        let r = call(
            &svc,
            json!({"action": "append", "text": "body but no title"}),
        )
        .await;
        assert!(r.is_error);
        assert!(tool_text(&r).contains("title"), "{}", tool_text(&r));

        let r = call(
            &svc,
            json!({"action": "append", "title": "t", "text": "  \n "}),
        )
        .await;
        assert!(r.is_error);

        let r = call(&svc, json!({"action": "delete"})).await;
        assert!(r.is_error);
        assert!(tool_text(&r).contains("requires id"), "{}", tool_text(&r));

        let r = call(&svc, json!({"action": "delete", "id": "zz"})).await;
        assert!(r.is_error);

        let r = call(&svc, json!({"action": "search"})).await;
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
