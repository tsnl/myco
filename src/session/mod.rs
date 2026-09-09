//! Conversation session persistence and metadata.
//!
//! Sessions live under `~/.myco/session/{shard}/{id}.json` (plus a sibling
//! `.history` for readline). Each document owns ordered threads and shared
//! metadata. Version 2 loads as one thread; saves use [`SESSION_FILE_VERSION`].
//!
//! Persistence only: how a conversation is stored, not how one is produced.
//! Compaction is split along that line — the document work
//! ([`compact_thread`], [`select_tail`]) is here, and the
//! agent run that writes the summary is [`crate::chat::run_compact_worker`].

mod attach;
mod compact;
mod console_log;
mod lock;
mod search;
mod thread;

pub use thread::Thread;

pub use attach::{MAX_MESSAGE_ATTACHMENT_BYTES, expand_image_attachments};
pub use compact::{CompactOutcome, compact_thread, select_tail};
pub use console_log::ConsoleLog;
pub use lock::{SessionLockError, SessionWriteLock};
pub use search::{SessionSearchReport, search_sessions};

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use uuid::Uuid;

use crate::core::{atomically_write, myco_home, uuid_simple_hex};
use crate::generative_model::{Message, TokenUsage};

/// Written schema version; versions 2 and 3 are also accepted on read.
pub const SESSION_FILE_VERSION: u32 = 4;
pub const RECENT_SESSION_LIMIT: usize = 10;
pub const SESSION_LIST_SNIPPET: usize = 48;
pub const MAX_TITLE_CHARS: usize = 120;
pub const MAX_SCRATCHPAD_BYTES: usize = 64 * 1024;

/// Why this session exists. Default [`SessionKind::User`] for interactive chats.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionKind {
    /// Interactive / user-visible conversation.
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
    /// Classify user sessions separately from hidden workers. Archive status
    /// filters visibility independently. Also omits the default kind on disk.
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
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub archived: bool,
    #[serde(deserialize_with = "thread::deserialize_threads")]
    threads: Vec<Thread>,
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
    /// Classification for filtering and UI; ordinary listings show unarchived
    /// [`SessionKind::User`] sessions.
    #[serde(default, skip_serializing_if = "SessionKind::is_user")]
    pub kind: SessionKind,
    /// Predecessor session from a legacy v2 compaction, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub predecessor_id: Option<String>,
    /// Successor session from a legacy v2 compaction, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub successor_id: Option<String>,
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
    writer: Arc<tokio::sync::Mutex<()>>,
}

impl ActiveSession {
    pub fn new(session: Session) -> Self {
        Self {
            inner: Arc::new(Mutex::new(session)),
            writer: Arc::new(tokio::sync::Mutex::new(())),
        }
    }

    pub async fn writer(&self) -> SessionWriter {
        SessionWriter {
            session: self.clone(),
            _guard: self.writer.clone().lock_owned().await,
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

    /// Archive only this session, preserving threads, lineage and live tools.
    pub fn set_archived(&self, archived: bool) -> Result<(), String> {
        let mut current = self.lock();
        let mut updated = current.clone();
        updated.archived = archived;
        updated.touch();
        updated.save()?;
        *current = updated;
        Ok(())
    }

    pub fn set_session_archived(&self, id: Option<&str>, archived: bool) -> Result<String, String> {
        let id = match id {
            Some(id) => resolve_session_id(id)?,
            None => self.id(),
        };
        if id == self.id() {
            self.set_archived(archived)?;
        } else {
            let _lock = SessionWriteLock::acquire(&id).map_err(|e| e.to_string())?;
            let session = Self::new(Session::load_by_id_or_prefix(&id)?);
            session.set_archived(archived)?;
        }
        Ok(id)
    }

    /// Persist messages + last usage when either changed (or `force`). A `None`
    /// usage keeps the stored value rather than clearing it.
    pub fn persist_messages(
        &self,
        messages: &[Message],
        last_usage: Option<TokenUsage>,
        force: bool,
    ) -> Result<(), String> {
        let thread_id = self.with(|session| session.active_thread().id.clone());
        self.persist_thread_messages(&thread_id, messages, last_usage, force)
    }

    pub fn persist_thread_messages(
        &self,
        thread_id: &str,
        messages: &[Message],
        last_usage: Option<TokenUsage>,
        force: bool,
    ) -> Result<(), String> {
        self.persist_thread_messages_at(thread_id, messages, last_usage, force, None)
    }

    pub(crate) fn persist_thread_messages_at(
        &self,
        thread_id: &str,
        messages: &[Message],
        last_usage: Option<TokenUsage>,
        force: bool,
        accepted: Option<(usize, DateTime<Utc>)>,
    ) -> Result<(), String> {
        let mut session = self.lock();
        if session.active_thread().id != thread_id {
            return Err(format!(
                "thread {thread_id} is no longer active; refusing a stale checkpoint"
            ));
        }
        let usage_changed =
            last_usage.is_some() && last_usage != session.active_thread().last_usage;
        if force || messages.len() != session.active_thread().messages.len() || usage_changed {
            session.active_thread_mut().messages = messages.to_vec();
            let times = &mut session.active_thread_mut().user_turn_timestamps;
            times.retain(|&index, _| {
                matches!(messages.get(index), Some(Message::UserMessage { .. }))
            });
            if let Some((index, time)) = accepted
                && matches!(messages.get(index), Some(Message::UserMessage { .. }))
            {
                times.entry(index).or_insert(time);
            }
            if last_usage.is_some() {
                session.active_thread_mut().last_usage = last_usage;
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

pub struct SessionWriter {
    session: ActiveSession,
    _guard: tokio::sync::OwnedMutexGuard<()>,
}

impl SessionWriter {
    pub fn commit_thread(&self, thread: Thread) -> Result<(), String> {
        let mut session = self.session.lock();
        if thread.predecessor_id.as_deref() != Some(&session.active_thread().id) {
            return Err("compaction predecessor is no longer the active thread".into());
        }
        let mut successor = session.clone();
        successor.threads.push(thread);
        thread::validate_threads(&successor.threads)?;
        successor.touch();
        successor.save()?;
        *session = successor;
        Ok(())
    }
}

impl Session {
    /// `model` is the catalog key from config.toml (recorded as metadata; a
    /// resumed session runs on whatever model the CLI selects).
    pub fn new(model: impl Into<String>) -> Self {
        Self::new_with_id(model, uuid_simple_hex(Uuid::new_v4()))
    }

    /// Create a session with an explicit id.
    pub fn new_with_id(model: impl Into<String>, id: impl Into<String>) -> Self {
        let now = Utc::now();
        Self {
            version: SESSION_FILE_VERSION,
            id: id.into(),
            created_at: now,
            updated_at: now,
            model: model.into(),
            archived: false,
            threads: vec![Thread::new()],
            title: None,
            links: Vec::new(),
            scratchpad: String::new(),
            parent_session_id: None,
            kind: SessionKind::User,
            predecessor_id: None,
            successor_id: None,
        }
    }

    /// Whether this session is omitted from default listings (derived from
    /// [`Self::kind`]).
    pub fn is_hidden(&self) -> bool {
        !self.kind.is_user()
    }

    /// The only thread accepting new messages; earlier threads are read-only.
    pub fn active_thread(&self) -> &Thread {
        self.threads
            .last()
            .expect("sessions contain at least one thread")
    }

    pub(crate) fn active_thread_mut(&mut self) -> &mut Thread {
        self.threads
            .last_mut()
            .expect("sessions contain at least one thread")
    }

    pub fn threads(&self) -> &[Thread] {
        &self.threads
    }

    pub fn find_thread(&self, id: &str) -> Result<&Thread, String> {
        self.threads
            .iter()
            .find(|thread| thread.id == id)
            .ok_or_else(|| format!("session {} has no thread {id}", self.id))
    }

    pub fn replace_context(&mut self, messages: Vec<Message>, usage: Option<TokenUsage>) {
        let thread = self.active_thread_mut();
        thread.messages = messages;
        thread.last_usage = usage;
        thread.user_turn_timestamps.clear();
    }

    pub fn summary_path(&self) -> PathBuf {
        self.thread_summary_path(&self.active_thread().id)
    }

    pub fn thread_summary_path(&self, thread_id: &str) -> PathBuf {
        session_file_path(&self.id, &format!("{thread_id}.summary.md"))
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
        child.active_thread_mut().messages = self.active_thread().messages.clone();
        child.active_thread_mut().last_usage = self.active_thread().last_usage;
        child.active_thread_mut().user_turn_timestamps =
            self.active_thread().user_turn_timestamps.clone();
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
        Self::from_json(&data).map_err(|e| format!("parse {}: {e}", path.display()))
    }

    pub fn from_json(data: &[u8]) -> Result<Self, String> {
        let mut value: serde_json::Value =
            serde_json::from_slice(data).map_err(|e| e.to_string())?;
        match value["version"].as_u64() {
            Some(2) => thread::upgrade_v2(&mut value)?,
            Some(3) => value["version"] = serde_json::json!(SESSION_FILE_VERSION),
            Some(4) => {}
            version => {
                return Err(format!(
                    "unsupported session version {version:?}; expected 2, 3 or {SESSION_FILE_VERSION}"
                ));
            }
        }
        let session: Self = serde_json::from_value(value).map_err(|e| e.to_string())?;
        if session.id.is_empty() {
            return Err("session has empty id".into());
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
    list_sessions_with_filter(limit, include_hidden, ArchiveFilter::Active)
}

pub fn list_sessions_with_filter(
    limit: usize,
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
fn collect_session_entries(
    root: &Path,
    include_hidden: bool,
    archive: ArchiveFilter,
) -> Result<SessionScan, String> {
    let mut metas = Vec::new();
    let mut skipped = Vec::new();
    for path in iter_session_json_files(root)? {
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
    list_sessions_with_filter(0, true, ArchiveFilter::All)
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

/// Load a session by id/prefix, or the most recent when `id_or_prefix` is `None`.
pub fn resolve_and_load_session(id_or_prefix: Option<&str>) -> Result<Session, String> {
    match id_or_prefix {
        Some(id) => Session::load_by_id_or_prefix(id),
        None => load_most_recent_session(),
    }
}

/// Most recent visible session without parsing every file: every `touch()` is
/// followed by an atomic `save()`, so file mtime tracks `updated_at`. Candidates
/// are parsed newest-first until one is readable, current-version, and visible —
/// hidden/corrupt/stale files cost a `stat` instead of a full-history parse.
fn load_most_recent_session() -> Result<Session, String> {
    let root = session_root()?;
    let mut candidates: Vec<(std::time::SystemTime, PathBuf)> = Vec::new();
    if root.exists() {
        for path in iter_session_json_files(&root)? {
            let mtime = fs::metadata(&path)
                .and_then(|m| m.modified())
                .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
            candidates.push((mtime, path));
        }
    }
    // Path breaks mtime ties for determinism on coarse-mtime filesystems.
    candidates.sort();
    for (_, path) in candidates.iter().rev() {
        // `load` already rejects unreadable and wrong-version files, so this
        // only has to add the visibility half.
        if let Ok(session) = Session::load(path)
            && !session.is_hidden()
            && !session.archived
        {
            return Ok(session);
        }
    }
    Err("no sessions found under ~/.myco/session".to_string())
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
    let mut label = session_label(entry);
    if entry.archived {
        label.push_str(" [archived]");
    }
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
    let rows: [(&str, Option<String>); 16] = [
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
        ("archived:  ", Some(session.archived.to_string())),
        ("thread:    ", Some(session.active_thread().id.clone())),
        ("threads:   ", Some(session.threads().len().to_string())),
        (
            "messages:  ",
            Some(
                session
                    .threads()
                    .iter()
                    .map(|thread| thread.messages.len())
                    .sum::<usize>()
                    .to_string(),
            ),
        ),
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
        session
            .active_thread_mut()
            .messages
            .push(user("hello\nworld"));
        session.save().unwrap();

        // Minified: no newlines outside JSON string escapes, no indentation.
        let raw = fs::read_to_string(session.json_path()).unwrap();
        assert!(!raw.contains('\n'), "expected single-line JSON: {raw:?}");

        let loaded = Session::load(&session.json_path()).unwrap();
        assert_eq!(loaded.id, session.id);
        assert_eq!(loaded.active_thread().messages.len(), 1);
    }

    fn set_mtime(path: &Path, t: std::time::SystemTime) {
        fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(t)
            .unwrap();
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
        session.active_thread_mut().messages = vec![user("hello")];
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
        assert_eq!(loaded.active_thread().messages.len(), 1);
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
        parent.active_thread_mut().messages = vec![user("hi")];
        parent.active_thread_mut().last_usage = Some(TokenUsage {
            input_tokens: 100,
            output_tokens: 10,
            cached_input_tokens: 50,
        });

        let child = parent.fork_child("othermodel");
        // Conversation + usage are inherited so the fork resumes the parent's
        // context (and its USER n/m headroom header) exactly.
        assert_eq!(child.active_thread().messages.len(), 1);
        assert_eq!(
            child.active_thread().last_usage,
            parent.active_thread().last_usage
        );
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
        session.active_thread_mut().last_usage = Some(TokenUsage {
            input_tokens: 12_345,
            output_tokens: 678,
            cached_input_tokens: 1_000,
        });
        let json = serde_json::to_vec_pretty(&session).unwrap();
        fs::write(&path, &json).unwrap();
        let loaded = Session::load(&path).unwrap();
        assert_eq!(
            loaded.active_thread().last_usage,
            session.active_thread().last_usage
        );
        assert_eq!(
            loaded.active_thread().last_usage.unwrap().context_tokens(),
            12_345
        );

        assert!(load_legacy_v2().active_thread().last_usage.is_none());
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
            Session::load_by_id_or_prefix(&id)
                .unwrap()
                .active_thread()
                .last_usage,
            Some(usage)
        );

        active
            .persist_messages(&[user("hi"), user("more")], None, true)
            .unwrap();
        assert_eq!(
            Session::load_by_id_or_prefix(&id)
                .unwrap()
                .active_thread()
                .last_usage,
            Some(usage)
        );
    }

    /// The on-disk schema is a contract, and it is spelled with Rust
    /// identifiers: `Message`, `Content`, `ToolUse`, `ToolResult` and
    /// Every v2 message variant and metadata field survives conversion to threads.
    #[test]
    fn v2_golden_fixture_preserves_messages_and_metadata_in_a_thread() {
        const FIXTURE: &str = include_str!("../../tests/fixtures/session_v2_all_variants.json");

        let session = Session::from_json(FIXTURE.as_bytes()).expect("fixture must load as v2");

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

        let usage = session.active_thread().last_usage.expect("last_usage");
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
        assert_eq!(session.active_thread().messages.len(), 6);
        match &session.active_thread().messages[0] {
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
        match &session.active_thread().messages[1] {
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
                assert_eq!(tool_uses[0].name, "bash");
                assert_eq!(tool_uses[0].input["command"], "echo hi");
                assert_eq!(*turn_end_reason, Some(TurnEndReason::ToolUse));
            }
            other => panic!("expected assistant message, got {other:?}"),
        }
        match &session.active_thread().messages[2] {
            Message::ToolResults { tool_use_results } => {
                assert_eq!(tool_use_results.len(), 2);
                assert!(!tool_use_results[0].is_error);
                assert!(tool_use_results[1].is_error);
            }
            other => panic!("expected tool results, got {other:?}"),
        }
        // The remaining turn-end reasons, including the stringly-typed arm.
        let reasons: Vec<_> = session.active_thread().messages[3..]
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

        let original: serde_json::Value = serde_json::from_str(FIXTURE).unwrap();
        let mut restored = serde_json::to_value(&session).unwrap();
        let thread = restored.as_object_mut().unwrap().remove("threads").unwrap();
        restored["messages"] = thread[0]["messages"].clone();
        restored["last_usage"] = thread[0]["last_usage"].clone();
        restored["version"] = 2.into();
        assert_eq!(restored, original);
        let encoded = serde_json::to_vec(&session).unwrap();
        let decoded = Session::from_json(&encoded).unwrap();
        assert_eq!(serde_json::to_vec(&decoded).unwrap(), encoded);
    }

    /// Sessions written before tool ids were cut carry `"id"` fields on
    /// tool_uses / tool_use_results. They must still load (ids ignored), and
    /// the next save drops them while upgrading the document to v3.
    #[test]
    fn v2_files_with_tool_ids_still_load_and_shed_them_on_save() {
        const FIXTURE: &str = include_str!("../../tests/fixtures/session_v2_all_variants.json");
        let mut v: serde_json::Value = serde_json::from_str(FIXTURE).unwrap();
        v["messages"][1]["AssistantMessage"]["tool_uses"][0]["id"] = "toolu_01".into();
        v["messages"][2]["ToolResults"]["tool_use_results"][0]["id"] = "toolu_01".into();
        let session = Session::from_json(&serde_json::to_vec(&v).unwrap())
            .expect("old-shape v2 file must load");
        match &session.active_thread().messages[1] {
            Message::AssistantMessage { tool_uses, .. } => {
                assert_eq!(tool_uses[0].name, "bash");
            }
            other => panic!("expected assistant message, got {other:?}"),
        }
        let rewritten = serde_json::to_string(&session).unwrap();
        assert!(!rewritten.contains("toolu_01"), "ids must be shed on save");
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
    fn archived_sessions_remain_searchable_and_restorable_with_lineage_intact() {
        let _home = temp_home("session-archive");
        let mut session = Session::new("test");
        session.replace_context(vec![user("archive needle")], None);
        session.set_scratchpad("keep this context".into()).unwrap();
        session.save().unwrap();
        let child = session.fork_child("test");
        child.save().unwrap();
        let id = session.id.clone();
        let active = ActiveSession::new(session);
        let _lock = SessionWriteLock::acquire(&id).unwrap();
        active.set_session_archived(None, true).unwrap();
        assert!(list_sessions(0).unwrap().is_empty());
        assert!(resolve_and_load_session(None).is_err());
        let archived = list_sessions_with_filter(0, false, ArchiveFilter::Archived).unwrap();
        assert_eq!(archived.len(), 1);
        assert_eq!(
            search_sessions(&archived, "needle", 20).unwrap().entries[0].id,
            id
        );
        let saved = Session::load_by_id_or_prefix(&id).unwrap();
        assert!(saved.archived);
        assert_eq!(saved.scratchpad, "keep this context");
        let child = Session::load_by_id_or_prefix(&child.id).unwrap();
        assert!(!child.archived);
        assert_eq!(child.parent_session_id.as_deref(), Some(id.as_str()));
        active.set_session_archived(None, false).unwrap();
        assert_eq!(list_sessions(0).unwrap()[0].id, id);
        assert!(
            list_sessions_with_filter(0, false, ArchiveFilter::Archived)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn archiving_another_live_session_respects_its_writer_lock() {
        let _home = temp_home("archive-writer");
        let mut other = Session::new("test");
        other.replace_context(vec![user("busy")], None);
        other.save().unwrap();
        let _lock = SessionWriteLock::acquire(&other.id).unwrap();
        let active = ActiveSession::new(Session::new("test"));
        assert!(active.set_session_archived(Some(&other.id), true).is_err());
        assert!(!Session::load_by_id_or_prefix(&other.id).unwrap().archived);
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

    #[test]
    fn bare_resume_picks_newest_mtime_and_skips_corrupt_files() {
        // Takes the myco-home lock, points MYCO_HOME at a temp dir, and undoes
        // both on drop — including when the test panics.
        let _home = temp_home("session-bare-resume");

        let older = Session::new("claude-haiku-4-5");
        older.save().unwrap();
        let newer = Session::new("claude-haiku-4-5");
        newer.save().unwrap();

        // Force distinct mtimes regardless of filesystem timestamp granularity.
        let base = std::time::SystemTime::now();
        set_mtime(
            &older.json_path(),
            base - std::time::Duration::from_secs(60),
        );
        set_mtime(&newer.json_path(), base);

        let resumed = resolve_and_load_session(None).unwrap();
        assert_eq!(resumed.id, newer.id);

        // A corrupt file with the newest mtime is skipped, not fatal.
        let corrupt = session_file_path("ffeeddccbbaa00112233445566778899", "json");
        fs::create_dir_all(corrupt.parent().unwrap()).unwrap();
        fs::write(&corrupt, b"{not json").unwrap();
        set_mtime(&corrupt, base + std::time::Duration::from_secs(60));
        let resumed = resolve_and_load_session(None).unwrap();
        assert_eq!(resumed.id, newer.id);
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
