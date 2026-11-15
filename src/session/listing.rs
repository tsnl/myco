//! Where session files live and how they are listed and resolved by id.
//! Unreadable files are skipped from listings, never silently.

use std::fs;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};

use super::{LinkCounts, Session, SessionKind, archive, first_user_text_from_messages};
use crate::core::myco_home;

/// Lightweight row for `/sessions` and `session_meta list`.
#[derive(Debug, Clone)]
pub struct SessionListEntry {
    pub id: String,
    pub path: PathBuf,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub model: String,
    pub archived: bool,
    pub message_count: usize,
    pub title: Option<String>,
    pub snippet: String,
    pub link_counts: LinkCounts,
    pub kind: SessionKind,
    pub parent_session_id: Option<String>,
}

#[derive(
    Debug,
    Clone,
    Copy,
    Default,
    PartialEq,
    Eq,
    serde::Serialize,
    serde::Deserialize,
    schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum ArchiveFilter {
    #[default]
    Active,
    Archived,
    All,
}

impl ArchiveFilter {
    fn includes(self, archived: bool) -> bool {
        match self {
            Self::Active => !archived,
            Self::Archived => archived,
            Self::All => true,
        }
    }
}

pub fn session_root() -> Result<PathBuf, String> {
    Ok(myco_home()?.join("session"))
}

pub fn session_file_path(id: &str, ext: &str) -> PathBuf {
    // Locks never move: a second process must see the same inode throughout
    // archive/restore, including while the session is still running.
    if ext == "lock" {
        return archive::file_path(id, ext, false);
    }
    let archived = !archive::file_path(id, "json", false).exists()
        && archive::file_path(id, "json", true).exists();
    let path = archive::file_path(id, ext, archived);
    let other = archive::file_path(id, ext, !archived);
    if !path.exists() && other.exists() {
        other
    } else {
        path
    }
}

/// Active sessions a person started, newest first.
pub fn list_sessions() -> Result<Vec<SessionListEntry>, String> {
    list_sessions_with_filter(false, ArchiveFilter::Active)
}

/// List sessions, newest first. When `include_hidden` is false, subagent/compact
/// sessions are omitted.
///
/// Unreadable files (corrupt JSON, wrong [`SESSION_FILE_VERSION`](super::SESSION_FILE_VERSION)) are skipped
/// rather than failing the listing, but never silently: they are reported once
/// per process. A session that vanishes from
/// `/sessions` without a word is indistinguishable from one that was never
/// there.
pub fn list_sessions_with_filter(
    include_hidden: bool,
    archive: ArchiveFilter,
) -> Result<Vec<SessionListEntry>, String> {
    let root = session_root()?;
    if !root.exists() {
        return Ok(Vec::new());
    }

    let (mut metas, skipped) = collect_session_entries(&root, include_hidden, archive)?;
    warn_about_skipped_sessions(&skipped);

    metas.sort_by_key(|m| std::cmp::Reverse(m.updated_at));
    Ok(metas)
}

/// Listable session entries, plus `(path, reason)` for each file that could not
/// be read.
type SessionScan = (Vec<SessionListEntry>, Vec<(PathBuf, String)>);

/// Read every session document under `root`, partitioned into listable entries
/// and the ones that could not be read.
pub(super) fn collect_session_entries(
    root: &Path,
    include_hidden: bool,
    archive: ArchiveFilter,
) -> Result<SessionScan, String> {
    let mut metas = Vec::new();
    let mut skipped = Vec::new();
    let paths = if archive == ArchiveFilter::Active {
        iter_sharded_session_files(root)?
    } else {
        iter_session_json_files(root)?
    };
    for path in paths {
        match session_list_entry_from_path(&path) {
            Ok(entry) => {
                if (include_hidden || entry.kind.is_user()) && archive.includes(entry.archived) {
                    metas.push(entry);
                }
            }
            Err(reason) => skipped.push((path, reason)),
        }
    }
    Ok((metas, skipped))
}

/// Report unreadable session files on stderr, once per process.
///
/// Listings run several times a session (`/sessions`, the
/// `session_meta` and `list_recent` tools); repeating the same warning each
/// time would train the reader to ignore it.
fn warn_about_skipped_sessions(skipped: &[(PathBuf, String)]) {
    use std::sync::atomic::{AtomicBool, Ordering};
    static WARNED: AtomicBool = AtomicBool::new(false);

    if skipped.is_empty() || WARNED.swap(true, Ordering::SeqCst) {
        return;
    }
    eprintln!(
        "warning: {} session file(s) under {} could not be read and are omitted from listings",
        skipped.len(),
        session_root()
            .map(|r| r.display().to_string())
            .unwrap_or_else(|_| "the selected profile's session directory".into())
    );
    for (path, reason) in skipped.iter().take(3) {
        eprintln!("  {}: {reason}", path.display());
    }
    if skipped.len() > 3 {
        eprintln!("  … and {} more", skipped.len() - 3);
    }
}

fn session_list_entry_from_path(path: &Path) -> Result<SessionListEntry, String> {
    // Prefer full parse so version is enforced; fall back is not used for wrong version.
    let session = Session::load(path)?;
    let snippet = first_user_text_from_messages(&session.threads()[0].messages).unwrap_or_default();
    let message_count = session
        .threads()
        .iter()
        .map(|thread| thread.messages.len())
        .sum();
    Ok(SessionListEntry {
        id: session.id,
        path: path.to_path_buf(),
        created_at: session.created_at,
        updated_at: session.updated_at,
        model: session.model,
        archived: session.archived,
        message_count,
        title: session.title,
        snippet,
        link_counts: LinkCounts::from_links(&session.links),
        kind: session.kind,
        parent_session_id: session.parent_session_id,
    })
}

pub fn resolve_session_id(id_or_prefix: &str) -> Result<String, String> {
    let needle = id_or_prefix.trim().to_ascii_lowercase();
    if needle.is_empty() {
        return Err("empty session id".into());
    }

    if needle.len() == 32 && needle.chars().all(|c| c.is_ascii_hexdigit()) {
        let path = session_file_path(&needle, "json");
        if path.exists() {
            return Ok(needle);
        }
    }

    let root = session_root()?;
    if !root.exists() {
        return Err(format!("no sessions directory at {}", root.display()));
    }

    let mut matches = Vec::new();
    for path in iter_session_json_files(&root)? {
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        if stem == needle || stem.starts_with(&needle) {
            matches.push(stem);
        }
    }

    matches.sort();
    matches.dedup();
    match matches.as_slice() {
        [] => Err(format!("no session matching {id_or_prefix:?}")),
        [one] => Ok(one.clone()),
        many => Err(format!(
            "ambiguous prefix {id_or_prefix:?}; candidates: {}",
            many.iter().take(8).cloned().collect::<Vec<_>>().join(", ")
        )),
    }
}

pub fn iter_session_json_files(root: &Path) -> Result<Vec<PathBuf>, String> {
    let mut paths = iter_sharded_session_files(root)?;
    let archived = root.join("archived");
    if archived.exists() {
        paths.extend(iter_sharded_session_files(&archived)?);
    }
    Ok(paths)
}

pub(super) fn iter_sharded_session_files(root: &Path) -> Result<Vec<PathBuf>, String> {
    let mut paths = Vec::new();
    let shards = fs::read_dir(root).map_err(|e| e.to_string())?;
    for shard_ent in shards {
        let shard_ent = shard_ent.map_err(|e| e.to_string())?;
        if shard_ent.file_name() == "archived" {
            continue;
        }
        let shard_path = shard_ent.path();
        if !shard_path.is_dir() {
            continue;
        }
        let Ok(files) = fs::read_dir(&shard_path) else {
            continue;
        };
        for file_ent in files {
            let Ok(file_ent) = file_ent else { continue };
            let path = file_ent.path();
            if path.extension().and_then(|e| e.to_str()) == Some("json") {
                paths.push(path);
            }
        }
    }
    Ok(paths)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::generative_model::{Content, Message};
    use crate::test_support::{temp_home, user};

    /// Unreadable files must be reported as skipped, not dropped on the floor:
    /// a corrupt newest session would otherwise make bare `--resume` open an
    /// older one with no explanation.
    #[test]
    fn corrupt_and_wrong_version_files_are_reported_as_skipped() {
        let root = crate::test_support::temp_dir("session-skip");
        let dir = root.path();
        let shard = dir.join("aa");
        fs::create_dir_all(&shard).unwrap();

        let mut good = Session::new_with_id("m", "aa00bb11cc22dd33ee44ff5566778899");
        good.active_thread_mut()
            .messages
            .push(Message::UserMessage {
                content: vec![Content::Text {
                    text: "readable".into(),
                }],
            });
        fs::write(
            shard.join(format!("{}.json", good.id)),
            serde_json::to_vec(&good).unwrap(),
        )
        .unwrap();

        fs::write(shard.join("aabroken.json"), b"{ not json at all").unwrap();
        fs::write(
            shard.join("aalegacy.json"),
            br#"{"version":1,"id":"aalegacy","created_at":"2020-01-01T00:00:00Z","updated_at":"2020-01-01T00:00:00Z","model":"x","messages":[]}"#,
        )
        .unwrap();

        let (entries, skipped) =
            collect_session_entries(dir, false, ArchiveFilter::Active).unwrap();
        assert_eq!(entries.len(), 1, "{entries:?}");
        assert_eq!(entries[0].id, good.id);
        assert_eq!(skipped.len(), 2, "{skipped:?}");

        let reasons: String = skipped.iter().map(|(_, why)| why.as_str()).collect();
        assert!(reasons.contains("unsupported session version"), "{reasons}");
    }

    #[test]
    fn hidden_default_false_and_omitted_from_list() {
        let _home = temp_home("session-hidden");

        let mut visible = Session::new("claude-haiku-4-5");
        visible.active_thread_mut().messages.push(user("visible"));
        visible.save().unwrap();

        let mut hidden = Session::new_hidden(
            "claude-haiku-4-5",
            "bbccddeeff00112233445566778899aa",
            SessionKind::Subagent,
            Some(visible.id.clone()),
        );
        hidden
            .active_thread_mut()
            .messages
            .push(user("hidden subagent"));
        hidden.save().unwrap();

        let listed = list_sessions().unwrap();
        assert!(
            listed.iter().any(|e| e.id == visible.id),
            "visible missing: {listed:?}"
        );
        assert!(
            listed.iter().all(|e| e.id != hidden.id),
            "hidden should be filtered: {listed:?}"
        );

        let all = list_sessions_with_filter(true, ArchiveFilter::Active).unwrap();
        assert!(all.iter().any(|e| e.id == hidden.id && !e.kind.is_user()));

        // Explicit id still loads hidden.
        let loaded = Session::load_by_id_or_prefix(&hidden.id).unwrap();
        assert!(loaded.is_hidden());
        assert_eq!(loaded.kind, SessionKind::Subagent);
        assert_eq!(
            loaded.parent_session_id.as_deref(),
            Some(visible.id.as_str())
        );
    }
}
