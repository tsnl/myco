//! One-shot content search over saved sessions.
//!
//! Backs `myco --mode session-browser --search`, the paged picker's
//! `s <text>` command, and `session_meta list` with `query`. Builds a
//! dedicated in-RAM [`SearchIndex`] (same pattern as the memory tool's
//! per-entry index) with one document per session: label, first-user-message
//! snippet, scratchpad, and the tail of the `{id}.console` mirror — so recall
//! works on what was *discussed*, not just the title. Nothing persists; the
//! index is rebuilt per call, and the expensive MiniLM pass only runs when
//! exact search found nothing (or the caller forces semantic).

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use crate::session::{Session, SessionListEntry, session_label};
use crate::text_search::{Hit, SearchIndex, embed_for_index};

/// Per-part caps keep each document well under the index's per-file cap
/// (`MAX_FILE_BYTES`), so no session is silently dropped from the corpus.
const SNIPPET_CAP: usize = 8 * 1024;
const SCRATCHPAD_CAP: usize = 8 * 1024;
const CONSOLE_TAIL_CAP: u64 = 32 * 1024;

#[derive(Debug, Clone)]
pub struct SessionSearchReport {
    /// Matching sessions, best first.
    pub entries: Vec<SessionListEntry>,
    /// `exact_tantivy` or `semantic_candle` (same labels as the engine).
    pub mode: &'static str,
}

/// Rank `entries` against `query`: Tantivy keyword search first, MiniLM
/// semantic when it finds nothing or `force_semantic` is set. Callers choose
/// the corpus (visible / including hidden). Blocking (candle on the semantic
/// path) — async callers wrap in `spawn_blocking`.
pub fn search_sessions(
    entries: &[SessionListEntry],
    query: &str,
    limit: usize,
    force_semantic: bool,
) -> Result<SessionSearchReport, String> {
    let query = query.trim();
    if query.is_empty() {
        return Err("query must not be empty".into());
    }
    let limit = limit.clamp(1, 200);
    if entries.is_empty() {
        return Ok(SessionSearchReport {
            entries: Vec::new(),
            mode: "exact_tantivy",
        });
    }

    let mut index = SearchIndex::new()?;
    let mut by_path: HashMap<PathBuf, &SessionListEntry> = HashMap::new();
    let mut docs: Vec<(PathBuf, String)> = Vec::with_capacity(entries.len());
    for entry in entries {
        by_path.insert(entry.path.clone(), entry);
        docs.push((entry.path.clone(), session_document(entry)));
    }
    for (path, doc) in &docs {
        index.upsert_file(path, doc.clone(), None);
    }

    if !force_semantic {
        let hits = index.search_exact(query, None, limit)?;
        if !hits.is_empty() {
            return Ok(report(hits, &by_path, "exact_tantivy"));
        }
    }

    // Semantic pass: embedding the corpus is the expensive part, so it is
    // deferred until needed; vectors are added by re-upserting each document.
    let q_vec = embed_for_index(query)?;
    for (path, doc) in docs {
        let vector = embed_for_index(&doc).ok();
        index.upsert_file(&path, doc, vector);
    }
    let hits = index.search_semantic(query, &q_vec, None, limit)?;
    Ok(report(hits, &by_path, "semantic_candle"))
}

fn report(
    hits: Vec<Hit>,
    by_path: &HashMap<PathBuf, &SessionListEntry>,
    mode: &'static str,
) -> SessionSearchReport {
    let entries = hits
        .into_iter()
        .filter_map(|h| by_path.get(&h.path).map(|e| (*e).clone()))
        .collect();
    SessionSearchReport { entries, mode }
}

/// Searchable text for one session. Head-ordered by representativeness —
/// the semantic embed clips to the first `MAX_EMBED_CHARS`, so the label and
/// snippet must come before the transcript tail.
fn session_document(entry: &SessionListEntry) -> String {
    let mut doc = String::new();
    doc.push_str(&session_label(entry));
    doc.push('\n');
    doc.push_str(head(&entry.snippet, SNIPPET_CAP));
    // Scratchpad needs the full session file; skipped if unreadable.
    if let Ok(session) = Session::load(&entry.path)
        && !session.scratchpad.is_empty()
    {
        doc.push('\n');
        doc.push_str(head(&session.scratchpad, SCRATCHPAD_CAP));
    }
    if let Some(tail) = read_tail(&entry.path.with_extension("console"), CONSOLE_TAIL_CAP) {
        doc.push('\n');
        doc.push_str(&tail);
    }
    doc
}

/// First `max_bytes` of `s` on a char boundary.
fn head(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Last `cap` bytes of the file as lossy UTF-8 (a mid-char start is fine).
fn read_tail(path: &Path, cap: u64) -> Option<String> {
    let mut file = std::fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    if len > cap {
        file.seek(SeekFrom::Start(len - cap)).ok()?;
    }
    let mut buf = Vec::new();
    file.read_to_end(&mut buf).ok()?;
    Some(String::from_utf8_lossy(&buf).into_owned())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{LinkCounts, SessionKind};

    fn entry(id: &str, snippet: &str, dir: &Path) -> SessionListEntry {
        SessionListEntry {
            id: id.to_string(),
            path: dir.join(format!("{id}.json")),
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            model: "test-model".into(),
            message_count: 2,
            title: None,
            snippet: snippet.to_string(),
            link_counts: LinkCounts::default(),
            kind: SessionKind::User,
            parent_session_id: None,
        }
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "myco-session-search-{tag}-{}",
            crate::session::uuid_simple_hex(uuid::Uuid::new_v4())
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn exact_query_matches_snippet_content() {
        let dir = temp_dir("exact");
        let entries = vec![
            entry("aaa", "we debugged the kubernetes ingress controller", &dir),
            entry("bbb", "wrote a haiku about rust lifetimes", &dir),
        ];
        let r = search_sessions(&entries, "kubernetes", 10, false).unwrap();
        assert_eq!(r.mode, "exact_tantivy");
        assert_eq!(r.entries.len(), 1, "{:?}", r.entries);
        assert_eq!(r.entries[0].id, "aaa");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn console_tail_is_searchable() {
        let dir = temp_dir("console");
        let entries = vec![entry("aaa", "hello", &dir), entry("bbb", "unrelated", &dir)];
        std::fs::write(
            dir.join("aaa.console"),
            "ASSISTANT\n\nthe root cause was the flibbertigibbet flag\n",
        )
        .unwrap();
        let r = search_sessions(&entries, "flibbertigibbet", 10, false).unwrap();
        assert_eq!(r.mode, "exact_tantivy");
        assert_eq!(r.entries.len(), 1);
        assert_eq!(r.entries[0].id, "aaa");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn semantic_forced_ranks_by_meaning() {
        // Uses compile-time embedded MiniLM (Candle; build.rs).
        embed_for_index("warmup").expect("MiniLM embedder must load offline");
        let dir = temp_dir("semantic");
        let entries = vec![
            entry("aaa", "how to cook pasta with tomato sauce and basil", &dir),
            entry("bbb", "fixing a segfault in the linker on aarch64", &dir),
        ];
        let r = search_sessions(&entries, "recipe for an italian dinner", 5, true).unwrap();
        assert_eq!(r.mode, "semantic_candle");
        assert_eq!(r.entries.first().map(|e| e.id.as_str()), Some("aaa"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn empty_query_errors_and_empty_corpus_is_empty() {
        let dir = temp_dir("empty");
        assert!(search_sessions(&[entry("aaa", "x", &dir)], "  ", 10, false).is_err());
        let r = search_sessions(&[], "anything", 10, false).unwrap();
        assert!(r.entries.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn head_respects_char_boundaries() {
        assert_eq!(head("abcdef", 3), "abc");
        assert_eq!(head("héllo", 2), "h"); // 'é' is 2 bytes starting at 1
        assert_eq!(head("short", 100), "short");
    }
}
