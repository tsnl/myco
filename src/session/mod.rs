//! Conversation session persistence and metadata.
//!
//! Sessions live under `~/.myco/session/{shard}/{id}.json` (plus a sibling
//! `.history` for readline). Schema is intentionally breaking vs earlier WIP
//! files: only [`SESSION_FILE_VERSION`] is accepted.
//!
//! Persistence only: this module knows how a conversation is stored, not how one
//! is produced. The agent runtime lives in [`crate::agent`] and depends on this
//! module, never the other way round — which is why compaction is split, with
//! the document logic ([`compact_session`], [`select_tail`],
//! [`link_compact_pair`]) here and the worker that drives an agent to write the
//! summary in [`crate::agent::run_compact_worker`].

mod attach;
mod compact;
mod console_log;
mod lock;
mod search;

pub use attach::{MAX_IMAGE_BYTES, MAX_MESSAGE_ATTACHMENT_BYTES, expand_image_attachments};
pub use compact::{CompactOutcome, compact_session, link_compact_pair, select_tail};
pub use console_log::ConsoleLog;
pub use lock::{SessionLockError, SessionWriteLock};
pub use search::{SessionSearchReport, search_sessions};

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use uuid::Uuid;

use crate::core::uuid_simple_hex;
use crate::generative_model::{Message, TokenUsage};

/// On-disk session schema version. Older files are rejected (WIP break).
pub const SESSION_FILE_VERSION: u32 = 2;
pub const RECENT_SESSION_LIMIT: usize = 10;
pub const SESSION_LIST_SNIPPET: usize = 48;
pub const MAX_TITLE_CHARS: usize = 120;
pub const MAX_SCRATCHPAD_BYTES: usize = 64 * 1024;

/// Why this session exists. Default [`SessionKind::User`] for interactive chats.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionKind {
    /// Interactive / user-visible conversation (REPL, successor after compact).
    #[default]
    User,
    /// Nested agent run (`myco --parent-session <id>`; also written by the
    /// removed `subagent` tool). Hidden by default in listings.
    Subagent,
    /// Compaction worker. Hidden by default in listings.
    Compact,
}

impl std::fmt::Display for SessionKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SessionKind::User => write!(f, "user"),
            SessionKind::Subagent => write!(f, "subagent"),
            SessionKind::Compact => write!(f, "compact"),
        }
    }
}

impl SessionKind {
    /// The one visibility predicate: only [`SessionKind::User`] sessions show
    /// in default `/sessions` / bare `--resume` / `session_meta list` —
    /// visibility is derived from kind, not a separate stored flag. Doubles as
    /// the serde skip helper (omit `kind` on disk when it is the default).
    pub fn is_user(&self) -> bool {
        matches!(self, SessionKind::User)
    }
}

/// Full conversation session document.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Session {
    pub version: u32,
    pub id: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub model: String,
    pub messages: Vec<Message>,
    /// Short human label; agent/CLI maintained.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Associated PRs / worktrees (any repo / host).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub links: Vec<SessionLink>,
    /// Per-session markdown scratchpad.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub scratchpad: String,
    /// Session / agent that spawned this one (subagent, compact worker).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session_id: Option<String>,
    /// Classification for filtering and UI. Visibility is derived via
    /// [`SessionKind::is_user`] (only [`SessionKind::User`] is listed by default).
    #[serde(default, skip_serializing_if = "SessionKind::is_user")]
    pub kind: SessionKind,
    /// Session this one was compacted from, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub predecessor_id: Option<String>,
    /// Session created by compacting this one, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub successor_id: Option<String>,
    /// Last provider usage, persisted so a resumed session shows real context
    /// (absent in sessions written before usage tracking).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_usage: Option<TokenUsage>,
}

/// Structured association stored on a session.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SessionLink {
    GitHubPr {
        url: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        repo: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        number: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        note: Option<String>,
    },
    Worktree {
        /// Harness host name (`local`, `devbox`, …).
        host: String,
        /// Absolute path on that host.
        path: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        branch: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        note: Option<String>,
    },
}

/// Lightweight row for `/sessions` and `session_meta list`.
#[derive(Debug, Clone)]
pub struct SessionListEntry {
    pub id: String,
    pub path: PathBuf,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub model: String,
    pub message_count: usize,
    pub title: Option<String>,
    pub snippet: String,
    pub link_counts: LinkCounts,
    pub kind: SessionKind,
    pub parent_session_id: Option<String>,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct LinkCounts {
    pub prs: usize,
    pub worktrees: usize,
}

impl LinkCounts {
    pub fn from_links(links: &[SessionLink]) -> Self {
        let mut c = Self::default();
        for link in links {
            match link {
                SessionLink::GitHubPr { .. } => c.prs += 1,
                SessionLink::Worktree { .. } => c.worktrees += 1,
            }
        }
        c
    }

    pub fn is_empty(self) -> bool {
        self.prs == 0 && self.worktrees == 0
    }
}

/// Shared handle so the CLI and `session_meta` tool mutate the same live session.
#[derive(Clone)]
pub struct ActiveSession {
    inner: Arc<Mutex<Session>>,
}

impl ActiveSession {
    pub fn new(session: Session) -> Self {
        Self {
            inner: Arc::new(Mutex::new(session)),
        }
    }

    pub fn replace(&self, session: Session) {
        let mut guard = self.lock();
        *guard = session;
    }

    pub fn snapshot(&self) -> Session {
        self.lock().clone()
    }

    pub fn id(&self) -> String {
        self.lock().id.clone()
    }

    pub fn with<R>(&self, f: impl FnOnce(&Session) -> R) -> R {
        f(&self.lock())
    }

    pub fn with_mut<R>(&self, f: impl FnOnce(&mut Session) -> R) -> R {
        f(&mut self.lock())
    }

    /// Persist messages + last usage when either changed (or `force`). A `None`
    /// usage keeps the stored value rather than clearing it.
    pub fn persist_messages(
        &self,
        messages: &[Message],
        last_usage: Option<TokenUsage>,
        force: bool,
    ) -> Result<(), String> {
        let mut session = self.lock();
        let usage_changed = last_usage.is_some() && last_usage != session.last_usage;
        if force || messages.len() != session.messages.len() || usage_changed {
            session.messages = messages.to_vec();
            if last_usage.is_some() {
                session.last_usage = last_usage;
            }
            session.touch();
            session.save()?;
        }
        Ok(())
    }

    /// Set title if currently unset, from the first user line. Returns true if set.
    pub fn maybe_auto_title_from_user_text(&self, text: &str) -> Result<bool, String> {
        let mut session = self.lock();
        if session.title.is_some() {
            return Ok(false);
        }
        if let Some(title) = auto_title_from_text(text) {
            session.title = Some(title);
            session.touch();
            session.save()?;
            return Ok(true);
        }
        Ok(false)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Session> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl Session {
    /// `model` is the catalog key from config.toml (recorded as metadata; a
    /// resumed session runs on whatever model the CLI selects).
    pub fn new(model: impl Into<String>) -> Self {
        Self::new_with_id(model, uuid_simple_hex(Uuid::new_v4()))
    }

    /// Create a session with an explicit id (same hex as agent id).
    pub fn new_with_id(model: impl Into<String>, id: impl Into<String>) -> Self {
        let now = Utc::now();
        Self {
            version: SESSION_FILE_VERSION,
            id: id.into(),
            created_at: now,
            updated_at: now,
            model: model.into(),
            messages: Vec::new(),
            title: None,
            links: Vec::new(),
            scratchpad: String::new(),
            parent_session_id: None,
            kind: SessionKind::User,
            predecessor_id: None,
            successor_id: None,
            last_usage: None,
        }
    }

    /// Whether this session is omitted from default listings (derived from
    /// [`Self::kind`]).
    pub fn is_hidden(&self) -> bool {
        !self.kind.is_user()
    }

    /// Sibling summary file written by compact workers: `{id}.summary.md`.
    pub fn summary_path(&self) -> PathBuf {
        session_file_path(&self.id, "summary.md")
    }

    /// Worker session (subagent / compact). Kind must be non-user so it is hidden.
    pub fn new_hidden(
        model: impl Into<String>,
        id: impl Into<String>,
        kind: SessionKind,
        parent_session_id: Option<String>,
    ) -> Self {
        debug_assert!(
            !kind.is_user(),
            "new_hidden requires a non-user SessionKind"
        );
        let mut s = Self::new_with_id(model, id);
        s.kind = kind;
        s.parent_session_id = parent_session_id;
        s
    }

    /// Context fork: a fresh hidden child session seeded with this session's
    /// conversation and usage. New id, `kind: subagent`, parented here; the
    /// parent's own metadata (title, links, scratchpad) stays with the parent.
    /// `model` records the child's catalog key.
    pub fn fork_child(&self, model: impl Into<String>) -> Self {
        let mut child = Self::new(model);
        child.kind = SessionKind::Subagent;
        child.parent_session_id = Some(self.id.clone());
        child.messages = self.messages.clone();
        child.last_usage = self.last_usage;
        child
    }

    pub fn touch(&mut self) {
        self.updated_at = Utc::now();
    }

    pub fn json_path(&self) -> PathBuf {
        session_file_path(&self.id, "json")
    }

    pub fn history_path(&self) -> PathBuf {
        session_file_path(&self.id, "history")
    }

    /// Sibling plain-text console mirror written live by the interactive CLI
    /// ([`ConsoleLog`]): `{id}.console`.
    pub fn console_path(&self) -> PathBuf {
        session_file_path(&self.id, "console")
    }

    pub fn save(&self) -> Result<(), String> {
        let path = self.json_path();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        // Minified, not pretty-printed: the file is rewritten every turn, and
        // structured readers (`session_history`, `jq`) don't need indentation.
        let json = serde_json::to_vec(self).map_err(|e| e.to_string())?;
        atomically_write(&path, &json)
    }

    pub fn load(path: &Path) -> Result<Self, String> {
        let data = fs::read(path).map_err(|e| format!("read {}: {e}", path.display()))?;
        let session: Session =
            serde_json::from_slice(&data).map_err(|e| format!("parse {}: {e}", path.display()))?;
        if session.version != SESSION_FILE_VERSION {
            return Err(format!(
                "unsupported session version {} in {} (expected {SESSION_FILE_VERSION}; \
                 old WIP sessions are not migrated)",
                session.version,
                path.display()
            ));
        }
        if session.id.is_empty() {
            return Err(format!("session file {} has empty id", path.display()));
        }
        Ok(session)
    }

    pub fn load_by_id_or_prefix(id_or_prefix: &str) -> Result<Self, String> {
        let id = resolve_session_id(id_or_prefix)?;
        Self::load(&session_file_path(&id, "json"))
    }

    pub fn set_title(&mut self, title: Option<String>) -> Result<(), String> {
        self.title = match title {
            None => None,
            Some(t) => Some(normalize_title(&t)?),
        };
        Ok(())
    }

    pub fn set_scratchpad(&mut self, text: String) -> Result<(), String> {
        if text.len() > MAX_SCRATCHPAD_BYTES {
            return Err(format!(
                "scratchpad too large ({} bytes; max {MAX_SCRATCHPAD_BYTES})",
                text.len()
            ));
        }
        self.scratchpad = text;
        Ok(())
    }

    /// Insert or update a link (dedup by PR URL or worktree host+path).
    pub fn upsert_link(&mut self, mut link: SessionLink) -> Result<(), String> {
        validate_link(&link)?;
        match &mut link {
            SessionLink::GitHubPr {
                url, repo, number, ..
            } => {
                let url_key = normalize_pr_url(url)?;
                let (parsed_repo, parsed_num) = parse_pr_fields(&url_key);
                *url = url_key.clone();
                if repo.is_none() {
                    *repo = parsed_repo;
                }
                if number.is_none() {
                    *number = parsed_num;
                }
                if let Some(existing) = self.links.iter_mut().find_map(|l| match l {
                    SessionLink::GitHubPr { url, .. } if urls_equal(url, &url_key) => Some(l),
                    _ => None,
                }) {
                    *existing = link;
                } else {
                    self.links.push(link);
                }
            }
            SessionLink::Worktree { host, path, .. } => {
                *host = host.trim().to_string();
                *path = path.trim().to_string();
                let host_key = host.clone();
                let path_key = path.clone();
                if let Some(existing) = self.links.iter_mut().find_map(|l| match l {
                    SessionLink::Worktree { host, path, .. }
                        if host == &host_key && path == &path_key =>
                    {
                        Some(l)
                    }
                    _ => None,
                }) {
                    *existing = link;
                } else {
                    self.links.push(link);
                }
            }
        }
        Ok(())
    }

    pub fn remove_link_at(&mut self, index: usize) -> Result<SessionLink, String> {
        if index >= self.links.len() {
            return Err(format!(
                "link index {index} out of range ({} links)",
                self.links.len()
            ));
        }
        Ok(self.links.remove(index))
    }

    pub fn remove_link_matching(
        &mut self,
        url: Option<&str>,
        host: Option<&str>,
        path: Option<&str>,
    ) -> Result<SessionLink, String> {
        let idx = self
            .links
            .iter()
            .position(|l| match l {
                SessionLink::GitHubPr {
                    url: existing_url, ..
                } => url.map(|u| urls_equal(existing_url, u)).unwrap_or(false),
                SessionLink::Worktree {
                    host: h, path: p, ..
                } => {
                    let host_ok = host.map(|x| x == h.as_str()).unwrap_or(false);
                    let path_ok = path.map(|x| x == p.as_str()).unwrap_or(true);
                    host_ok && path_ok
                }
            })
            .ok_or_else(|| "no matching link".to_string())?;
        Ok(self.links.remove(idx))
    }
}

// ---------------------------------------------------------------------------
// Paths / listing / resolve
// ---------------------------------------------------------------------------

pub fn myco_home() -> Result<PathBuf, String> {
    if let Ok(root) = std::env::var("MYCO_HOME") {
        let p = PathBuf::from(root);
        if !p.as_os_str().is_empty() {
            return Ok(p);
        }
    }
    dirs::home_dir()
        .map(|h| h.join(".myco"))
        .ok_or_else(|| "could not resolve home directory".into())
}

pub fn session_root() -> Result<PathBuf, String> {
    Ok(myco_home()?.join("session"))
}

pub fn session_file_path(id: &str, ext: &str) -> PathBuf {
    let shard = &id[..2.min(id.len())];
    match session_root() {
        Ok(root) => root.join(shard).join(format!("{id}.{ext}")),
        Err(_) => PathBuf::from(format!(".myco/session/{shard}/{id}.{ext}")),
    }
}

pub fn atomically_write(path: &Path, content: &[u8]) -> Result<(), String> {
    let mut file = atomic_write_file::AtomicWriteFile::options()
        .open(path)
        .map_err(|e| e.to_string())?;
    file.write_all(content).map_err(|e| e.to_string())?;
    file.commit().map_err(|e| e.to_string())?;
    Ok(())
}

pub fn list_sessions(limit: usize) -> Result<Vec<SessionListEntry>, String> {
    list_sessions_filtered(limit, /*include_hidden*/ false)
}

/// List sessions. When `include_hidden` is false, subagent/compact sessions are omitted.
///
/// Unreadable files (corrupt JSON, wrong [`SESSION_FILE_VERSION`]) are skipped
/// rather than failing the listing, but never silently: they are reported once
/// per process via [`warn_about_skipped_sessions`]. A session that vanishes from
/// `/sessions` without a word is indistinguishable from one that was never
/// there, and bare `--resume` would quietly open an *older* session instead of
/// the newest one.
pub fn list_sessions_filtered(
    limit: usize,
    include_hidden: bool,
) -> Result<Vec<SessionListEntry>, String> {
    let root = session_root()?;
    if !root.exists() {
        return Ok(Vec::new());
    }

    let (mut metas, skipped) = collect_session_entries(&root, include_hidden)?;
    warn_about_skipped_sessions(&skipped);

    metas.sort_by_key(|m| std::cmp::Reverse(m.updated_at));
    if limit > 0 {
        metas.truncate(limit);
    }
    Ok(metas)
}

/// Listable session entries, plus `(path, reason)` for each file that could not
/// be read.
type SessionScan = (Vec<SessionListEntry>, Vec<(PathBuf, String)>);

/// Read every session document under `root`, partitioned into listable entries
/// and the ones that could not be read.
fn collect_session_entries(root: &Path, include_hidden: bool) -> Result<SessionScan, String> {
    let mut metas = Vec::new();
    let mut skipped = Vec::new();
    for path in iter_session_json_files(root)? {
        match session_list_entry_from_path(&path) {
            Ok(entry) => {
                if include_hidden || entry.kind.is_user() {
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
/// Listings run several times a session (`/sessions`, bare `/resume`, the
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
            .unwrap_or_else(|_| "~/.myco/session".into())
    );
    for (path, reason) in skipped.iter().take(3) {
        eprintln!("  {}: {reason}", path.display());
    }
    if skipped.len() > 3 {
        eprintln!("  … and {} more", skipped.len() - 3);
    }
}

/// List every readable **visible** session (no limit). Wrong-version files are omitted.
pub fn list_all_sessions() -> Result<Vec<SessionListEntry>, String> {
    list_sessions(0)
}

/// List every readable session including hidden (no limit).
pub fn list_all_sessions_including_hidden() -> Result<Vec<SessionListEntry>, String> {
    list_sessions_filtered(0, true)
}

fn session_list_entry_from_path(path: &Path) -> Result<SessionListEntry, String> {
    // Prefer full parse so version is enforced; fall back is not used for wrong version.
    let session = Session::load(path)?;
    let snippet = first_user_text_from_messages(&session.messages).unwrap_or_default();
    Ok(SessionListEntry {
        id: session.id,
        path: path.to_path_buf(),
        created_at: session.created_at,
        updated_at: session.updated_at,
        model: session.model,
        message_count: session.messages.len(),
        title: session.title,
        snippet,
        link_counts: LinkCounts::from_links(&session.links),
        kind: session.kind,
        parent_session_id: session.parent_session_id,
    })
}

/// Load a session by id/prefix, or the most recent when `id_or_prefix` is `None`.
pub fn resolve_and_load_session(id_or_prefix: Option<&str>) -> Result<Session, String> {
    match id_or_prefix {
        Some(id) => Session::load_by_id_or_prefix(id),
        None => {
            let list = list_sessions(1)?;
            let meta = list
                .into_iter()
                .next()
                .ok_or_else(|| "no sessions found under ~/.myco/session".to_string())?;
            Session::load(&meta.path)
        }
    }
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
    let mut paths = Vec::new();
    let shards = fs::read_dir(root).map_err(|e| e.to_string())?;
    for shard_ent in shards {
        let shard_ent = shard_ent.map_err(|e| e.to_string())?;
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

// ---------------------------------------------------------------------------
// Formatting helpers
// ---------------------------------------------------------------------------

pub fn truncate_snippet(s: &str, max: usize) -> String {
    let one_line: String = s.chars().map(|c| if c == '\n' { ' ' } else { c }).collect();
    if one_line.chars().count() <= max {
        return one_line;
    }
    let trimmed: String = one_line.chars().take(max.saturating_sub(1)).collect();
    format!("{trimmed}…")
}

pub fn auto_title_from_text(text: &str) -> Option<String> {
    let line = text.lines().map(str::trim).find(|l| !l.is_empty())?;
    normalize_title(line).ok()
}

pub fn normalize_title(raw: &str) -> Result<String, String> {
    let one_line: String = raw
        .chars()
        .map(|c| if c == '\n' || c == '\r' { ' ' } else { c })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let one_line = one_line.trim().to_string();
    if one_line.is_empty() {
        return Err("title must be non-empty".into());
    }
    if one_line.chars().count() > MAX_TITLE_CHARS {
        let trimmed: String = one_line
            .chars()
            .take(MAX_TITLE_CHARS.saturating_sub(1))
            .collect();
        return Ok(format!("{trimmed}…"));
    }
    Ok(one_line)
}

/// First user message as text, for session labels, snippets, and search. The
/// session stamp myco prepends to that message is skipped — a label should read
/// as what the user asked, not as myco's own payload.
pub fn first_user_text_from_messages(messages: &[Message]) -> Option<String> {
    for msg in messages {
        if let Message::UserMessage { content } = msg {
            let text: String = content
                .iter()
                .filter_map(|c| match c {
                    crate::generative_model::Content::Text { text }
                        if !crate::prompts::is_session_stamp(text) =>
                    {
                        Some(text.as_str())
                    }
                    _ => None,
                })
                .collect();
            if !text.trim().is_empty() {
                return Some(text);
            }
        }
    }
    None
}

/// Human label for a list row: title when set, else the first-user-message
/// snippet, else `(untitled)`.
pub fn session_label(entry: &SessionListEntry) -> String {
    let label = entry
        .title
        .as_deref()
        .filter(|t| !t.is_empty())
        .map(|t| t.to_string())
        .unwrap_or_else(|| truncate_snippet(&entry.snippet, SESSION_LIST_SNIPPET));
    if label.is_empty() {
        "(untitled)".to_string()
    } else {
        label
    }
}

pub fn format_session_list_line(index: usize, entry: &SessionListEntry) -> String {
    let label = session_label(entry);
    let links = if entry.link_counts.is_empty() {
        String::new()
    } else {
        format!(
            "  pr:{} wt:{}",
            entry.link_counts.prs, entry.link_counts.worktrees
        )
    };
    let hidden = if entry.kind.is_user() {
        String::new()
    } else {
        format!("  [{}]", entry.kind)
    };
    format!(
        "  {:>2}. {}  {}  model={}  msgs={}{}{}  {}",
        index,
        entry.id,
        entry.updated_at.to_rfc3339(),
        entry.model,
        entry.message_count,
        links,
        hidden,
        label
    )
}

pub fn format_session_detail(session: &Session) -> String {
    let console = session.console_path();
    // (label incl. padding, value); `None` rows are omitted.
    let rows: [(&str, Option<String>); 13] = [
        ("id:        ", Some(session.id.clone())),
        (
            "path:      ",
            Some(session.json_path().display().to_string()),
        ),
        (
            "console:   ",
            console.exists().then(|| console.display().to_string()),
        ),
        ("created:   ", Some(session.created_at.to_rfc3339())),
        ("updated:   ", Some(session.updated_at.to_rfc3339())),
        ("model:     ", Some(session.model.clone())),
        ("messages:  ", Some(session.messages.len().to_string())),
        ("kind:      ", Some(session.kind.to_string())),
        ("hidden:    ", Some(session.is_hidden().to_string())),
        ("parent:    ", session.parent_session_id.clone()),
        ("predecessor: ", session.predecessor_id.clone()),
        ("successor:   ", session.successor_id.clone()),
        (
            "title:     ",
            Some(
                session
                    .title
                    .as_deref()
                    .filter(|t| !t.is_empty())
                    .unwrap_or("(none)")
                    .to_string(),
            ),
        ),
    ];
    let mut out = String::new();
    for (label, value) in rows {
        if let Some(value) = value {
            out.push_str(&format!("{label}{value}\n"));
        }
    }
    if session.links.is_empty() {
        out.push_str("links:     (none)\n");
    } else {
        out.push_str(&format!("links:     ({})\n", session.links.len()));
        for (i, link) in session.links.iter().enumerate() {
            out.push_str(&format!("  [{i}] {}\n", format_link_one_line(link)));
        }
    }
    if session.scratchpad.is_empty() {
        out.push_str("scratchpad: (empty)\n");
    } else {
        out.push_str(&format!(
            "scratchpad: {} bytes\n---\n{}\n---\n",
            session.scratchpad.len(),
            session.scratchpad
        ));
    }
    out
}

pub fn format_link_one_line(link: &SessionLink) -> String {
    match link {
        SessionLink::GitHubPr {
            url,
            repo,
            number,
            note,
        } => {
            let mut s = format!("pr {url}");
            if let (Some(r), Some(n)) = (repo, number) {
                s = format!("pr {r}#{n} ({url})");
            }
            if let Some(n) = note
                && !n.is_empty()
            {
                s.push_str(&format!(" — {n}"));
            }
            s
        }
        SessionLink::Worktree {
            host,
            path,
            branch,
            note,
        } => {
            let mut s = format!("worktree host={host} path={path}");
            if let Some(b) = branch
                && !b.is_empty()
            {
                s.push_str(&format!(" branch={b}"));
            }
            if let Some(n) = note
                && !n.is_empty()
            {
                s.push_str(&format!(" — {n}"));
            }
            s
        }
    }
}

// ---------------------------------------------------------------------------
// Link validation / PR URL helpers
// ---------------------------------------------------------------------------

fn validate_link(link: &SessionLink) -> Result<(), String> {
    match link {
        SessionLink::GitHubPr { url, .. } => {
            normalize_pr_url(url)?;
            Ok(())
        }
        SessionLink::Worktree { host, path, .. } => {
            if host.trim().is_empty() {
                return Err("worktree host must be non-empty".into());
            }
            let path = path.trim();
            if path.is_empty() {
                return Err("worktree path must be non-empty".into());
            }
            // Allow Unix absolute and Windows drive paths; reject relative.
            let windows_abs = path.len() >= 3 && path.as_bytes()[1] == b':';
            if !path.starts_with('/') && !windows_abs {
                return Err("worktree path must be absolute".into());
            }
            Ok(())
        }
    }
}

/// Normalize a GitHub PR reference to an https URL.
///
/// Accepts:
/// - `https://github.com/org/repo/pull/123`
/// - `http://github.com/org/repo/pull/123`
/// - `github.com/org/repo/pull/123`
/// - `org/repo#123` / `org/repo/pull/123`
pub fn normalize_pr_url(raw: &str) -> Result<String, String> {
    let s = raw.trim();
    if s.is_empty() {
        return Err("PR url must be non-empty".into());
    }

    // org/repo#123
    if let Some((repo, num)) = s.split_once('#')
        && repo.contains('/')
        && !repo.contains("://")
        && num.chars().all(|c| c.is_ascii_digit())
    {
        let num: u32 = num
            .parse()
            .map_err(|_| format!("invalid PR number in {s:?}"))?;
        if num == 0 {
            return Err("PR number must be > 0".into());
        }
        return Ok(format!("https://github.com/{repo}/pull/{num}"));
    }

    let mut url = s.to_string();
    if url.starts_with("github.com/") {
        url = format!("https://{url}");
    }
    if url.starts_with("http://") {
        url = format!("https://{}", &url["http://".len()..]);
    }

    // org/repo/pull/123
    if !url.contains("://")
        && let Some((repo, rest)) = url.split_once("/pull/")
        && repo.contains('/')
        && rest.chars().all(|c| c.is_ascii_digit())
    {
        url = format!("https://github.com/{repo}/pull/{rest}");
    }

    let rest = url.strip_prefix("https://github.com/").ok_or_else(|| {
        format!("PR url must be a github.com pull request URL or org/repo#N (got {raw:?})")
    })?;
    let parts: Vec<&str> = rest.trim_end_matches('/').split('/').collect();
    // org/repo/pull/N
    if parts.len() >= 4
        && parts[2] == "pull"
        && let Ok(n) = parts[3].parse::<u32>()
        && n > 0
    {
        return Ok(format!(
            "https://github.com/{}/{}/pull/{n}",
            parts[0], parts[1]
        ));
    }
    Err(format!(
        "PR url must be a github.com pull request URL or org/repo#N (got {raw:?})"
    ))
}

pub fn parse_pr_fields(url: &str) -> (Option<String>, Option<u32>) {
    let Ok(norm) = normalize_pr_url(url) else {
        return (None, None);
    };
    let rest = norm.trim_start_matches("https://github.com/");
    let parts: Vec<&str> = rest.split('/').collect();
    if parts.len() >= 4 && parts[2] == "pull" {
        let repo = format!("{}/{}", parts[0], parts[1]);
        let number = parts[3].parse().ok();
        return (Some(repo), number);
    }
    (None, None)
}

fn urls_equal(a: &str, b: &str) -> bool {
    match (normalize_pr_url(a), normalize_pr_url(b)) {
        (Ok(x), Ok(y)) => x == y,
        _ => a.trim() == b.trim(),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Serialize tests that mutate `MYCO_HOME` (process-global env).
#[cfg(test)]
pub(crate) fn lock_myco_home_for_test() -> std::sync::MutexGuard<'static, ()> {
    use std::sync::{Mutex, OnceLock};
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::generative_model::{Content, Message, TokenUsage};
    use crate::test_support::{temp_dir, temp_home, user};

    /// Pre-`last_usage` / pre-`kind` v2 file: absent optional fields must default.
    const LEGACY_V2_JSON: &[u8] = br#"{"version":2,"id":"ccddeeff00112233445566778899aabb","created_at":"2020-01-01T00:00:00Z","updated_at":"2020-01-01T00:00:00Z","model":"x","messages":[]}"#;

    fn load_legacy_v2() -> Session {
        let dir = temp_dir("session-legacy");
        let path = dir.path().join("legacy.json");
        fs::write(&path, LEGACY_V2_JSON).unwrap();
        Session::load(&path).unwrap()
    }

    #[test]
    fn save_writes_minified_single_line_json_that_loads_back() {
        let _home = temp_home("session-save");

        let mut session = Session::new("claude-haiku-4-5");
        session.messages.push(user("hello\nworld"));
        session.save().unwrap();

        // Minified: no newlines outside JSON string escapes, no indentation.
        let raw = fs::read_to_string(session.json_path()).unwrap();
        assert!(!raw.contains('\n'), "expected single-line JSON: {raw:?}");

        let loaded = Session::load(&session.json_path()).unwrap();
        assert_eq!(loaded.id, session.id);
        assert_eq!(loaded.messages.len(), 1);
    }

    #[test]
    fn normalize_pr_url_variants() {
        assert_eq!(
            normalize_pr_url("https://github.com/foo/bar/pull/12").unwrap(),
            "https://github.com/foo/bar/pull/12"
        );
        assert_eq!(
            normalize_pr_url("foo/bar#99").unwrap(),
            "https://github.com/foo/bar/pull/99"
        );
        assert_eq!(
            normalize_pr_url("github.com/foo/bar/pull/3").unwrap(),
            "https://github.com/foo/bar/pull/3"
        );
        assert!(normalize_pr_url("https://gitlab.com/x/y/merge_requests/1").is_err());
    }

    #[test]
    fn title_normalization() {
        assert_eq!(normalize_title("  hello   world  ").unwrap(), "hello world");
        assert!(normalize_title("   ").is_err());
        let long = "x".repeat(200);
        let t = normalize_title(&long).unwrap();
        assert!(t.chars().count() <= MAX_TITLE_CHARS);
        assert!(t.ends_with('…'));
    }

    #[test]
    fn link_dedup_pr_and_worktree() {
        let mut s = Session::new("claude-haiku-4-5");
        s.upsert_link(SessionLink::GitHubPr {
            url: "foo/bar#1".into(),
            repo: None,
            number: None,
            note: Some("a".into()),
        })
        .unwrap();
        s.upsert_link(SessionLink::GitHubPr {
            url: "https://github.com/foo/bar/pull/1".into(),
            repo: Some("foo/bar".into()),
            number: Some(1),
            note: Some("b".into()),
        })
        .unwrap();
        assert_eq!(s.links.len(), 1);
        match &s.links[0] {
            SessionLink::GitHubPr { note, .. } => assert_eq!(note.as_deref(), Some("b")),
            _ => panic!("expected pr"),
        }

        s.upsert_link(SessionLink::Worktree {
            host: "local".into(),
            path: "/tmp/wt".into(),
            branch: Some("feat/x".into()),
            note: None,
        })
        .unwrap();
        s.upsert_link(SessionLink::Worktree {
            host: "local".into(),
            path: "/tmp/wt".into(),
            branch: Some("feat/y".into()),
            note: Some("upd".into()),
        })
        .unwrap();
        assert_eq!(s.links.len(), 2);
        match &s.links[1] {
            SessionLink::Worktree { branch, note, .. } => {
                assert_eq!(branch.as_deref(), Some("feat/y"));
                assert_eq!(note.as_deref(), Some("upd"));
            }
            _ => panic!("expected worktree"),
        }
    }

    #[test]
    fn session_file_roundtrip_v2() {
        let dir = temp_dir("session-roundtrip");
        let path = dir.path().join("sess.json");

        let mut session = Session::new("claude-opus-4-8");
        session.messages = vec![user("hello")];
        session.title = Some("hello session".into());
        session.links = vec![SessionLink::Worktree {
            host: "local".into(),
            path: "/tmp/x".into(),
            branch: None,
            note: None,
        }];
        session.scratchpad = "notes".into();

        let json = serde_json::to_vec_pretty(&session).unwrap();
        fs::write(&path, &json).unwrap();

        let loaded = Session::load(&path).unwrap();
        assert_eq!(loaded.id, session.id);
        assert_eq!(loaded.title.as_deref(), Some("hello session"));
        assert_eq!(loaded.scratchpad, "notes");
        assert_eq!(loaded.links.len(), 1);
        assert_eq!(loaded.messages.len(), 1);
    }

    /// Session labels, snippets, and search read the first user message; the
    /// session stamp myco puts in front of it is not the user's words.
    #[test]
    fn first_user_text_skips_the_session_stamp() {
        let messages = vec![Message::UserMessage {
            content: vec![
                Content::Text {
                    text: crate::prompts::session_stamp(
                        "aa00bb11cc22dd33ee44ff5566778899",
                        Utc::now(),
                    ),
                },
                Content::Text {
                    text: "port the harness to windows".into(),
                },
            ],
        }];
        assert_eq!(
            first_user_text_from_messages(&messages).as_deref(),
            Some("port the harness to windows")
        );
    }

    #[test]
    fn fork_child_copies_conversation_not_identity() {
        let mut parent = Session::new_with_id("modelkey", "aa00bb11cc22dd33ee44ff5566778899");
        parent.title = Some("parent title".into());
        parent.scratchpad = "parent notes".into();
        parent.messages = vec![user("hi")];
        parent.last_usage = Some(TokenUsage {
            input_tokens: 100,
            output_tokens: 10,
            cached_input_tokens: 50,
        });

        let child = parent.fork_child("othermodel");
        // Conversation + usage are inherited so the fork resumes the parent's
        // context (and its USER n/m headroom header) exactly.
        assert_eq!(child.messages.len(), 1);
        assert_eq!(child.last_usage, parent.last_usage);
        // Identity is fresh: new id, hidden subagent kind, parented; the
        // parent's metadata does not leak.
        assert_ne!(child.id, parent.id);
        assert_eq!(child.kind, SessionKind::Subagent);
        assert!(child.is_hidden());
        assert_eq!(child.parent_session_id.as_deref(), Some(parent.id.as_str()));
        assert_eq!(child.model, "othermodel");
        assert!(child.title.is_none());
        assert!(child.scratchpad.is_empty());
        assert!(child.links.is_empty());
    }

    #[test]
    fn last_usage_persists_and_old_sessions_default_none() {
        let dir = temp_dir("session-usage");
        let path = dir.path().join("with_usage.json");
        let mut session =
            Session::new_with_id("claude-opus-4-8", "aa00bb11cc22dd33ee44ff5566778899");
        session.last_usage = Some(TokenUsage {
            input_tokens: 12_345,
            output_tokens: 678,
            cached_input_tokens: 1_000,
        });
        let json = serde_json::to_vec_pretty(&session).unwrap();
        fs::write(&path, &json).unwrap();
        let loaded = Session::load(&path).unwrap();
        assert_eq!(loaded.last_usage, session.last_usage);
        assert_eq!(loaded.last_usage.unwrap().context_tokens(), 12_345);

        assert!(load_legacy_v2().last_usage.is_none());
    }

    #[test]
    fn persist_messages_records_usage_and_none_keeps_last() {
        let _home = temp_home("session-persist");

        let usage = TokenUsage {
            input_tokens: 5_000,
            output_tokens: 100,
            cached_input_tokens: 0,
        };
        let active = ActiveSession::new(Session::new("claude-haiku-4-5"));
        let id = active.id();

        active
            .persist_messages(&[user("hi")], Some(usage), true)
            .unwrap();
        assert_eq!(
            Session::load_by_id_or_prefix(&id).unwrap().last_usage,
            Some(usage)
        );

        active
            .persist_messages(&[user("hi"), user("more")], None, true)
            .unwrap();
        assert_eq!(
            Session::load_by_id_or_prefix(&id).unwrap().last_usage,
            Some(usage)
        );
    }

    /// The on-disk schema is a contract, and it is spelled with Rust
    /// identifiers: `Message`, `Content`, `ToolUse`, `ToolResult` and
    /// `TurnEndReason` serialize externally tagged with no `serde(rename)`
    /// pinning them. Renaming a variant or a field therefore compiles, keeps
    /// `version: 2`, and makes every stored session unreadable — after which
    /// `list_sessions_filtered` drops the files from listings.
    ///
    /// This fixture is that contract written down: a v2 document covering every
    /// variant of every persisted type. It must keep loading, and it must
    /// re-serialize byte-for-byte. If this test fails, the disk format changed:
    /// either revert the rename or bump [`SESSION_FILE_VERSION`] deliberately
    /// and replace this fixture.
    #[test]
    fn v2_golden_fixture_loads_and_reserializes_byte_identically() {
        const FIXTURE: &str = include_str!("../../tests/fixtures/session_v2_all_variants.json");

        let session: Session = serde_json::from_str(FIXTURE).expect("fixture must parse as v2");

        assert_eq!(session.version, SESSION_FILE_VERSION);
        assert_eq!(session.id, "aabbccddeeff00112233445566778899");
        assert_eq!(session.model, "opus-catalog-key");
        assert_eq!(session.title.as_deref(), Some("every v2 variant"));
        assert_eq!(session.scratchpad, "scratch notes");
        assert_eq!(session.kind, SessionKind::Subagent);
        assert!(session.is_hidden());
        assert!(session.parent_session_id.is_some());
        assert!(session.predecessor_id.is_some());
        assert!(session.successor_id.is_some());

        let usage = session.last_usage.expect("last_usage");
        assert_eq!(usage.input_tokens, 12_345);
        assert_eq!(usage.output_tokens, 678);
        assert_eq!(usage.cached_input_tokens, 9_000);

        // Both link kinds, with their optional fields populated.
        assert_eq!(session.links.len(), 2);
        match &session.links[0] {
            SessionLink::GitHubPr { repo, number, .. } => {
                assert_eq!(repo.as_deref(), Some("tsnl/myco"));
                assert_eq!(*number, Some(1));
            }
            other => panic!("expected pr link, got {other:?}"),
        }
        match &session.links[1] {
            SessionLink::Worktree { host, path, .. } => {
                assert_eq!(host, "devbox");
                assert_eq!(path, "/tmp/wt");
            }
            other => panic!("expected worktree link, got {other:?}"),
        }

        // Every Message variant, and every Content variant inside them.
        use crate::generative_model::TurnEndReason;
        assert_eq!(session.messages.len(), 6);
        match &session.messages[0] {
            Message::UserMessage { content } => {
                assert!(
                    matches!(&content[0], Content::Text { text } if text == "look at this shot")
                );
                assert!(
                    matches!(&content[1], Content::Image { source } if source.starts_with("data:image/png"))
                );
            }
            other => panic!("expected user message, got {other:?}"),
        }
        match &session.messages[1] {
            Message::AssistantMessage {
                content,
                tool_uses,
                turn_end_reason,
            } => {
                // Signed thinking and the redacted placeholder both survive.
                match &content[0] {
                    Content::Thinking {
                        text,
                        signature,
                        redacted,
                    } => {
                        assert_eq!(text, "weighing options");
                        assert_eq!(signature.as_deref(), Some("sig-abc"));
                        assert!(!redacted);
                    }
                    other => panic!("expected signed thinking, got {other:?}"),
                }
                assert!(
                    matches!(&content[1], Content::Thinking { redacted, signature, .. } if *redacted && signature.is_none())
                );
                assert_eq!(tool_uses.len(), 1);
                assert_eq!(tool_uses[0].id, "toolu_01");
                assert_eq!(tool_uses[0].input["command"], "echo hi");
                assert_eq!(*turn_end_reason, Some(TurnEndReason::ToolUse));
            }
            other => panic!("expected assistant message, got {other:?}"),
        }
        match &session.messages[2] {
            Message::ToolResults { tool_use_results } => {
                assert_eq!(tool_use_results.len(), 2);
                assert!(!tool_use_results[0].is_error);
                assert!(tool_use_results[1].is_error);
            }
            other => panic!("expected tool results, got {other:?}"),
        }
        // The remaining turn-end reasons, including the stringly-typed arm.
        let reasons: Vec<_> = session.messages[3..]
            .iter()
            .map(|m| match m {
                Message::AssistantMessage {
                    turn_end_reason, ..
                } => turn_end_reason.clone(),
                other => panic!("expected assistant message, got {other:?}"),
            })
            .collect();
        assert_eq!(
            reasons,
            vec![
                Some(TurnEndReason::MaxTokens),
                Some(TurnEndReason::Other("Anthropic::PauseTurn".into())),
                Some(TurnEndReason::EndTurn),
            ]
        );

        // Round-trip: the bytes we write must be the bytes we accept.
        let reserialized = format!("{}\n", serde_json::to_string_pretty(&session).unwrap());
        assert_eq!(
            reserialized, FIXTURE,
            "session serialization drifted from the v2 fixture"
        );
    }

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
        good.messages.push(Message::UserMessage {
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

        let (entries, skipped) = collect_session_entries(dir, false).unwrap();
        assert_eq!(entries.len(), 1, "{entries:?}");
        assert_eq!(entries[0].id, good.id);
        assert_eq!(skipped.len(), 2, "{skipped:?}");

        let reasons: String = skipped.iter().map(|(_, why)| why.as_str()).collect();
        assert!(reasons.contains("unsupported session version"), "{reasons}");
    }

    #[test]
    fn reject_wrong_version() {
        let dir = temp_dir("session-version");
        let path = dir.path().join("old.json");
        fs::write(
            &path,
            br#"{"version":1,"id":"aa","created_at":"2020-01-01T00:00:00Z","updated_at":"2020-01-01T00:00:00Z","model":"x","messages":[]}"#,
        )
        .unwrap();
        let err = Session::load(&path).unwrap_err();
        assert!(err.contains("unsupported session version"), "{err}");
    }

    #[test]
    fn active_session_auto_title_once() {
        let _home = temp_home("session-title");
        let s = ActiveSession::new(Session::new("claude-haiku-4-5"));
        assert!(
            s.maybe_auto_title_from_user_text("First line\n\nmore")
                .unwrap()
        );
        assert_eq!(s.snapshot().title.as_deref(), Some("First line"));
        assert!(!s.maybe_auto_title_from_user_text("Second").unwrap());
        assert_eq!(s.snapshot().title.as_deref(), Some("First line"));
    }

    #[test]
    fn scratchpad_cap() {
        let mut s = Session::new("claude-haiku-4-5");
        let big = "a".repeat(MAX_SCRATCHPAD_BYTES + 1);
        assert!(s.set_scratchpad(big).is_err());
        s.set_scratchpad("ok".into()).unwrap();
        assert_eq!(s.scratchpad, "ok");
    }

    #[test]
    fn hidden_default_false_and_omitted_from_list() {
        let _home = temp_home("session-hidden");

        let mut visible = Session::new("claude-haiku-4-5");
        visible.messages.push(user("visible"));
        visible.save().unwrap();

        let mut hidden = Session::new_hidden(
            "claude-haiku-4-5",
            "bbccddeeff00112233445566778899aa",
            SessionKind::Subagent,
            Some(visible.id.clone()),
        );
        hidden.messages.push(user("hidden subagent"));
        hidden.save().unwrap();

        let listed = list_sessions(0).unwrap();
        assert!(
            listed.iter().any(|e| e.id == visible.id),
            "visible missing: {listed:?}"
        );
        assert!(
            listed.iter().all(|e| e.id != hidden.id),
            "hidden should be filtered: {listed:?}"
        );

        let all = list_sessions_filtered(0, true).unwrap();
        assert!(all.iter().any(|e| e.id == hidden.id && !e.kind.is_user()));

        // Bare resume resolves most recent *visible* session.
        let resumed = resolve_and_load_session(None).unwrap();
        assert_eq!(resumed.id, visible.id);

        // Explicit id still loads hidden.
        let loaded = Session::load_by_id_or_prefix(&hidden.id).unwrap();
        assert!(loaded.is_hidden());
        assert_eq!(loaded.kind, SessionKind::Subagent);
        assert_eq!(
            loaded.parent_session_id.as_deref(),
            Some(visible.id.as_str())
        );
    }

    /// No kind/parent fields on disk — serde defaults to user (visible).
    #[test]
    fn old_session_json_defaults_kind_user() {
        let s = load_legacy_v2();
        assert!(!s.is_hidden());
        assert_eq!(s.kind, SessionKind::User);
        assert!(s.parent_session_id.is_none());
    }
}
