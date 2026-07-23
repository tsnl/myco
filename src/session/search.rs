//! One-shot content search over saved sessions.
//!
//! Backs `myco --mode session-browser --search` and `session_meta list`
//! with `query`. Builds one document per session — label, first-user-message
//! snippet, scratchpad, and the tail of the `{id}.console` mirror — so recall
//! works on what was *discussed*, not just the title, then ranks with plain
//! case-insensitive token matching. Nothing persists and nothing is indexed;
//! the corpus is a handful of small documents rebuilt per call. Semantic
//! ranking is deliberately out of scope: myco is not in the search-engine
//! business (use `rg` or a dedicated search tool on the workspace instead).

use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use crate::session::{Session, SessionListEntry, session_label};

/// Per-part caps bound the per-session document size.
const SNIPPET_CAP: usize = 8 * 1024;
const SCRATCHPAD_CAP: usize = 8 * 1024;
const CONSOLE_TAIL_CAP: u64 = 32 * 1024;

#[derive(Debug, Clone)]
pub struct SessionSearchReport {
    /// Matching sessions, best first.
    pub entries: Vec<SessionListEntry>,
    /// Always `keyword` (kept so callers can display how results were ranked).
    pub mode: &'static str,
}

/// Rank `entries` against `query` by case-insensitive token matching.
/// Callers choose the corpus (visible / including hidden). Does blocking
/// file I/O (session files, console tails) — async callers wrap in
/// `spawn_blocking`.
pub fn search_sessions(
    entries: &[SessionListEntry],
    query: &str,
    limit: usize,
) -> Result<SessionSearchReport, String> {
    let query = query.trim();
    if query.is_empty() {
        return Err("query must not be empty".into());
    }
    let limit = limit.clamp(1, 200);
    let tokens: Vec<String> = query.split_whitespace().map(|t| t.to_lowercase()).collect();

    let mut scored: Vec<(u64, &SessionListEntry)> = entries
        .iter()
        .filter_map(|entry| {
            let doc = session_document(entry).to_lowercase();
            let score = score_document(&doc, &tokens);
            (score > 0).then_some((score, entry))
        })
        .collect();
    // Best score first; recency breaks ties.
    scored.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.updated_at.cmp(&a.1.updated_at)));

    Ok(SessionSearchReport {
        entries: scored
            .into_iter()
            .take(limit)
            .map(|(_, e)| e.clone())
            .collect(),
        mode: "keyword",
    })
}

/// Sum of per-token occurrence counts; tokens that miss entirely score 0 for
/// the document only if *no* token hits (any single hit qualifies the doc).
fn score_document(doc_lower: &str, tokens: &[String]) -> u64 {
    let mut score = 0u64;
    for token in tokens {
        if token.is_empty() {
            continue;
        }
        score += doc_lower.matches(token.as_str()).count() as u64;
    }
    score
}

/// Searchable text for one session, label and snippet first.
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
    use std::path::PathBuf;

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
    fn query_matches_snippet_content_case_insensitively() {
        let dir = temp_dir("exact");
        let entries = vec![
            entry("aaa", "we debugged the Kubernetes ingress controller", &dir),
            entry("bbb", "wrote a haiku about rust lifetimes", &dir),
        ];
        let r = search_sessions(&entries, "kubernetes", 10).unwrap();
        assert_eq!(r.mode, "keyword");
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
        let r = search_sessions(&entries, "flibbertigibbet", 10).unwrap();
        assert_eq!(r.entries.len(), 1);
        assert_eq!(r.entries[0].id, "aaa");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn more_hits_rank_higher() {
        let dir = temp_dir("rank");
        let entries = vec![
            entry("aaa", "wasm wasm wasm build pipeline", &dir),
            entry("bbb", "one mention of wasm here", &dir),
        ];
        let r = search_sessions(&entries, "wasm", 10).unwrap();
        assert_eq!(r.entries.len(), 2);
        assert_eq!(r.entries[0].id, "aaa");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn empty_query_errors_and_empty_corpus_is_empty() {
        let dir = temp_dir("empty");
        assert!(search_sessions(&[entry("aaa", "x", &dir)], "  ", 10).is_err());
        let r = search_sessions(&[], "anything", 10).unwrap();
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
