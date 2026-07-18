//! Root-only `memory` tool: persistent memory shared across agents and sessions.
//!
//! The memory document is a **set of atomic entries**: immutable, UUIDed,
//! timestamped. Entries are only ever created or deleted — nothing is
//! rewritten in place and no locks are taken, so concurrent sessions and
//! subagents are safe even on weakly consistent network filesystems
//! (maildir's write-once-unique-name pattern). Each entry is one file,
//! `~/.myco/memory/{YYYY-MM}/{utc-ms-timestamp}-{uuid}.md`:
//!
//! ```text
//! {uuid}
//! {UTC timestamp} ({local timestamp} local) agent={hex8}
//!
//! {body}
//! ```
//!
//! Readers resolve the document by listing entries in name (= time) order,
//! joining with two blank lines. Timestamp shard dirs bound directory size
//! and make GC a directory delete (not automated yet).
//!
//! Search uses a dedicated in-RAM [`SearchIndex`] with one document per entry
//! file, covering only the **latest shard**; older shards stay on disk for
//! bash/grep. The shard listing is diffed before each query so entries from
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
use crate::session::{myco_home, uuid_simple_hex};
use crate::text_search::{Hit, SearchIndex, embed_for_index};

use super::{HostDispatchContext, ToolService};

const TOOL_DESCRIPTION: &str = r#"
Persistent memory shared across agents and sessions (one document per machine, under
`~/.myco/memory/`).

The document is a set of **atomic entries** — immutable, UUIDed, timestamped. Entries are
only ever created or deleted (each is a write-once file `{YYYY-MM}/{timestamp}-{uuid}.md`;
nothing is rewritten in place, no locks), so concurrent sessions and subagents never
conflict. To correct a stale fact: append the corrected entry, then delete the old one.
Searches cover only the latest month shard; older shards stay on disk for bash/grep
(GC = delete a shard dir; not automated yet).

Actions:
- append: add an entry (markdown body; UUID + timestamp header added automatically).
  Keep entries short, durable, one fact each — user preferences, project facts,
  decisions, gotchas. Not a scratchpad: use session_meta set_scratchpad for
  session-local notes. Returns the entry id.
- delete: remove one entry by `id` (uuid from append/search results; unique prefix ok).
  The deleted entry is echoed back, so a mistaken delete can be re-appended.
- exact_search: Tantivy full-text over entries of the latest shard (identifiers,
  literal phrases). Hits show entry ids and file:line refs.
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
    /// Indexed entry files: path key → entry metadata (for hit display).
    entries: HashMap<String, EntryMeta>,
}

struct EntryMeta {
    id: String,
    /// Timestamp/attribution line (entry line 2).
    stamp: String,
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
                ActionKind::Delete => match input.id.as_deref() {
                    Some(id) => self.delete(id).await,
                    None => Err(
                        "delete requires id (entry uuid from append/search results; \
                         unique prefix ok)"
                            .into(),
                    ),
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
    /// Create one immutable entry as a new unique file.
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
        f.write_all(format!("{id}\n{stamp}\n\n{text}\n").as_bytes())
            .map_err(|e| format!("write {}: {e}", path.display()))?;

        Ok(format!(
            "appended\nid={id}\nfile={}\n{stamp}\n",
            path.display()
        ))
    }

    /// Delete one entry by uuid (or unique prefix), echoing its content.
    async fn delete(&self, id: &str) -> Result<String, String> {
        let id = id.trim().to_ascii_lowercase();
        if id.len() < 4 || !id.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(format!(
                "delete id must be a hex entry uuid (unique prefix ok, min 4 chars), got {id:?}"
            ));
        }
        let dir = self.dir()?;
        tokio::task::spawn_blocking(move || {
            let mut matches: Vec<PathBuf> = Vec::new();
            for shard in shard_dirs(&dir) {
                for file in entry_files(&shard) {
                    if entry_uuid(&file).is_some_and(|u| u.starts_with(&id)) {
                        matches.push(file);
                    }
                }
            }
            match matches.as_slice() {
                [] => Err(format!("no entry matching id {id:?}")),
                [file] => {
                    let content = std::fs::read_to_string(file).unwrap_or_default();
                    std::fs::remove_file(file)
                        .map_err(|e| format!("delete {}: {e}", file.display()))?;
                    Ok(format!(
                        "deleted {}\n--- deleted entry (append the body again to restore) ---\n{content}",
                        file.display()
                    ))
                }
                many => Err(format!(
                    "ambiguous id {id:?}; candidates: {}",
                    many.iter()
                        .filter_map(|p| entry_uuid(p))
                        .take(8)
                        .collect::<Vec<_>>()
                        .join(", ")
                )),
            }
        })
        .await
        .map_err(|e| format!("delete join: {e}"))?
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

/// Timestamp shard dirs under `dir`, name-sorted ascending.
fn shard_dirs(dir: &Path) -> Vec<PathBuf> {
    let Ok(read) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
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
    shards
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

/// Entry uuid from a `{timestamp}-{uuid}.md` filename.
fn entry_uuid(path: &Path) -> Option<String> {
    let name = path.file_name()?.to_str()?.strip_suffix(".md")?;
    let (_, id) = name.rsplit_once('-')?;
    (!id.is_empty() && id.chars().all(|c| c.is_ascii_hexdigit())).then(|| id.to_string())
}

/// Newest timestamp shard that contains entry files, with its entries.
fn latest_populated_shard(dir: &Path) -> Option<(PathBuf, Vec<PathBuf>)> {
    for shard in shard_dirs(dir).into_iter().rev() {
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
/// Entry files are immutable, so the diff is by name only: unseen files are
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
        // Filename is authoritative; body line 1 is the readable fallback.
        let id = entry_uuid(&path)
            .or_else(|| text.lines().next().map(|l| l.trim().to_string()))
            .unwrap_or_default();
        let stamp = text.lines().nth(1).unwrap_or("").trim().to_string();
        let key = path_key(&path);
        state.index.upsert_file(&path, text, vector);
        state.entries.insert(key, EntryMeta { id, stamp });
    }
    state.index.commit()?;
    Ok(shard)
}

fn format_report(
    semantic: bool,
    shard: &Path,
    hits: &[Hit],
    entries: &HashMap<String, EntryMeta>,
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
        let meta = entries.get(&path_key(&h.path));
        out.push_str(&format!(
            "\n[{}] score={:.4} id={} {}:{}\n",
            i + 1,
            h.score,
            meta.map(|m| m.id.as_str()).unwrap_or("?"),
            h.path.display(),
            h.line_number.unwrap_or(1),
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
        if let Some(body) = body
            && body != stamp
            && meta.is_none_or(|m| m.id != body)
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
    /// Entry markdown body for `append` (UUID + timestamp header added automatically).
    #[serde(default)]
    text: Option<String>,
    /// Entry id for `delete` (uuid from append/search results; unique prefix ok).
    #[serde(default)]
    id: Option<String>,
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
    Delete,
    ExactSearch,
    SemanticSearch,
}

impl std::fmt::Display for ActionKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ActionKind::Append => write!(f, "append"),
            ActionKind::Delete => write!(f, "delete"),
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

    #[tokio::test]
    async fn each_append_is_a_new_immutable_uuid_entry_and_searchable() {
        let dir = tmp_dir();
        let svc = Arc::new(MemoryService::with_dir_for_tests(dir.clone()));

        let r = call(
            &svc,
            json!({"action": "append", "text": "user prefers rebase-first workflow"}),
        )
        .await;
        assert!(!r.is_error, "{r:?}");
        let id = appended_id(&r);
        assert_eq!(id.len(), 32, "{id}");
        assert!(tool_text(&r).contains("agent="), "{}", tool_text(&r));

        let r = call(&svc, json!({"action": "exact_search", "query": "rebase"})).await;
        assert!(!r.is_error, "{r:?}");
        let text = tool_text(&r);
        assert!(text.contains("rebase-first workflow"), "{text}");
        assert!(text.contains(&format!("id={id}")), "{text}");
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
    async fn delete_by_id_removes_entry_and_echoes_it() {
        let dir = tmp_dir();
        let svc = Arc::new(MemoryService::with_dir_for_tests(dir.clone()));

        let a = call(&svc, json!({"action": "append", "text": "stale_token_one"})).await;
        let a_id = appended_id(&a);
        let b = call(&svc, json!({"action": "append", "text": "kept_token_two"})).await;
        let b_id = appended_id(&b);

        // Delete by full uuid; result echoes the entry for recovery.
        let r = call(&svc, json!({"action": "delete", "id": a_id})).await;
        assert!(!r.is_error, "{r:?}");
        let text = tool_text(&r);
        assert!(text.contains("deleted"), "{text}");
        assert!(text.contains("stale_token_one"), "{text}");

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
            old.join("20000101T000000000Z-deadbeefdeadbeefdeadbeefdeadbeef.md"),
            "deadbeefdeadbeefdeadbeefdeadbeef\n\
             2000-01-01T00:00:00Z (2000-01-01T00:00:00+00:00 local) agent=deadbeef\n\
             \n\
             old_token_alpha\n",
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

        // Delete still reaches old shards (search does not).
        let r = call(&svc, json!({"action": "delete", "id": "deadbeefdead"})).await;
        assert!(!r.is_error, "{r:?}");
        assert!(
            tool_text(&r).contains("old_token_alpha"),
            "{}",
            tool_text(&r)
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn hand_deleted_entry_files_drop_out_of_the_index() {
        let dir = tmp_dir();
        let svc = Arc::new(MemoryService::with_dir_for_tests(dir.clone()));

        let a = call(&svc, json!({"action": "append", "text": "stale_token_one"})).await;
        assert!(!a.is_error, "{a:?}");
        let keep = call(&svc, json!({"action": "append", "text": "kept_token_two"})).await;
        assert!(!keep.is_error, "{keep:?}");

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

        // GC by hand (bash rm): the file drops out on the next search.
        let stale_file = tool_text(&a)
            .lines()
            .find_map(|l| l.strip_prefix("file=").map(str::to_string))
            .expect("append result carries file=");
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

        let r = call(&svc, json!({"action": "delete"})).await;
        assert!(r.is_error);
        assert!(tool_text(&r).contains("requires id"), "{}", tool_text(&r));

        let r = call(&svc, json!({"action": "delete", "id": "zz"})).await;
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
