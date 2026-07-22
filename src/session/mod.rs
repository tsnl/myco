//! Conversation session persistence and metadata.
//!
//! Sessions live under `~/.myco/session/{shard}/{id}.json` (plus a sibling
//! `.history` for readline). Schema is intentionally breaking vs earlier WIP
//! files: only [`SESSION_FILE_VERSION`] is accepted.

mod agent;
mod compact;
mod console_log;
mod markdown;
mod transcript;

pub use agent::{
    Agent, AgentEvent, AgentInteractionError, EventSink, NullEventSink, TraceContext,
    uuid_simple_hex,
};
pub use compact::{
    CompactOptions, CompactOutcome, compact_session, compact_subagent_prompt, link_compact_pair,
    select_tail,
};
pub use console_log::ConsoleLog;
pub use markdown::{MarkdownRenderer, render_block};
pub use transcript::{
    BANNER_RULE, Palette, SECTION_RULE, TOOL_DISPLAY_STRING_MAX, USER_RULE, banner_rule,
    ensure_assistant, format_tool_invocation, print_session_history, section_rule,
    truncate_display_string, truncate_json_strings, user_rule, write_assistant_open, write_block,
    write_error_open, write_error_section, write_session_history, write_warning_open,
};

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use uuid::Uuid;

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
    /// Nested agent run (historical `subagent` tool, since removed; the
    /// variant stays so old session files load). Hidden by default in listings.
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
    /// Serde skip helper: omit `kind` on disk when it is the default.
    pub fn is_user(&self) -> bool {
        matches!(self, SessionKind::User)
    }

    /// Visible in default `/sessions` / bare `--resume` / `session_meta list`.
    ///
    /// Visibility is derived from kind (not a separate stored flag): only
    /// [`SessionKind::User`] sessions are visible.
    pub fn is_visible(self) -> bool {
        matches!(self, SessionKind::User)
    }

    pub fn is_hidden(self) -> bool {
        !self.is_visible()
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
    /// [`SessionKind::is_visible`] (only [`SessionKind::User`] is listed by default).
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

    /// Whether this session appears in default listings (derived from [`Self::kind`]).
    pub fn is_visible(&self) -> bool {
        self.kind.is_visible()
    }

    pub fn is_hidden(&self) -> bool {
        self.kind.is_hidden()
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
            kind.is_hidden(),
            "new_hidden requires a non-user SessionKind"
        );
        let mut s = Self::new_with_id(model, id);
        s.kind = kind;
        s.parent_session_id = parent_session_id;
        s
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
        let json = serde_json::to_vec_pretty(self).map_err(|e| e.to_string())?;
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
pub fn list_sessions_filtered(
    limit: usize,
    include_hidden: bool,
) -> Result<Vec<SessionListEntry>, String> {
    let root = session_root()?;
    if !root.exists() {
        return Ok(Vec::new());
    }

    let mut metas = Vec::new();
    for path in iter_session_json_files(&root)? {
        match session_list_entry_from_path(&path) {
            Ok(entry) => {
                if include_hidden || entry.kind.is_visible() {
                    metas.push(entry);
                }
            }
            Err(_) => continue, // skip corrupt / wrong-version files
        }
    }

    metas.sort_by_key(|m| std::cmp::Reverse(m.updated_at));
    if limit > 0 {
        metas.truncate(limit);
    }
    Ok(metas)
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

pub fn first_user_text_from_messages(messages: &[Message]) -> Option<String> {
    for msg in messages {
        if let Message::UserMessage { content } = msg {
            let text: String = content
                .iter()
                .filter_map(|c| match c {
                    crate::generative_model::Content::Text { text } => Some(text.as_str()),
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

pub fn format_session_list_line(index: usize, entry: &SessionListEntry) -> String {
    let label = entry
        .title
        .as_deref()
        .filter(|t| !t.is_empty())
        .map(|t| t.to_string())
        .unwrap_or_else(|| truncate_snippet(&entry.snippet, SESSION_LIST_SNIPPET));
    let label = if label.is_empty() {
        "(untitled)".to_string()
    } else {
        label
    };
    let links = if entry.link_counts.is_empty() {
        String::new()
    } else {
        format!(
            "  pr:{} wt:{}",
            entry.link_counts.prs, entry.link_counts.worktrees
        )
    };
    let hidden = if entry.kind.is_hidden() {
        format!("  [{}]", entry.kind)
    } else {
        String::new()
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
    let mut out = String::new();
    out.push_str(&format!("id:        {}\n", session.id));
    out.push_str(&format!("path:      {}\n", session.json_path().display()));
    let console = session.console_path();
    if console.exists() {
        out.push_str(&format!("console:   {}\n", console.display()));
    }
    out.push_str(&format!("created:   {}\n", session.created_at.to_rfc3339()));
    out.push_str(&format!("updated:   {}\n", session.updated_at.to_rfc3339()));
    out.push_str(&format!("model:     {}\n", session.model));
    out.push_str(&format!("messages:  {}\n", session.messages.len()));
    out.push_str(&format!("kind:      {}\n", session.kind));
    out.push_str(&format!(
        "hidden:    {}\n",
        if session.is_hidden() { "true" } else { "false" }
    ));
    if let Some(parent) = session.parent_session_id.as_deref() {
        out.push_str(&format!("parent:    {parent}\n"));
    }
    if let Some(id) = session.predecessor_id.as_deref() {
        out.push_str(&format!("predecessor: {id}\n"));
    }
    if let Some(id) = session.successor_id.as_deref() {
        out.push_str(&format!("successor:   {id}\n"));
    }
    out.push_str(&format!(
        "title:     {}\n",
        session
            .title
            .as_deref()
            .filter(|t| !t.is_empty())
            .unwrap_or("(none)")
    ));
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
    use std::time::Duration;

    fn myco_home_lock() -> std::sync::MutexGuard<'static, ()> {
        lock_myco_home_for_test()
    }

    fn temp_session_root() -> PathBuf {
        std::env::temp_dir().join(format!(
            "myco-session-unit-{}",
            uuid_simple_hex(Uuid::new_v4())
        ))
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
        let dir = temp_session_root();
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("sess.json");

        let mut session = Session {
            version: SESSION_FILE_VERSION,
            id: "aabbccddeeff00112233445566778899".into(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
            model: "claude-opus-4-8".into(),
            messages: vec![Message::UserMessage {
                content: vec![Content::Text {
                    text: "hello".into(),
                }],
            }],
            title: Some("hello session".into()),
            links: vec![SessionLink::Worktree {
                host: "local".into(),
                path: "/tmp/x".into(),
                branch: None,
                note: None,
            }],
            scratchpad: "notes".into(),
            parent_session_id: None,
            kind: SessionKind::User,
            predecessor_id: None,
            successor_id: None,
            last_usage: None,
        };
        session.updated_at = session.created_at + Duration::from_secs(1);

        let json = serde_json::to_vec_pretty(&session).unwrap();
        fs::write(&path, &json).unwrap();

        let loaded = Session::load(&path).unwrap();
        assert_eq!(loaded.id, session.id);
        assert_eq!(loaded.title.as_deref(), Some("hello session"));
        assert_eq!(loaded.scratchpad, "notes");
        assert_eq!(loaded.links.len(), 1);
        assert_eq!(loaded.messages.len(), 1);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn last_usage_persists_and_old_sessions_default_none() {
        let dir = temp_session_root();
        fs::create_dir_all(&dir).unwrap();

        let path = dir.join("with_usage.json");
        let mut session =
            Session::new_with_id("claude-opus-4-8", "aa00bb11cc22dd33ee44ff5566778899");
        session.last_usage = Some(TokenUsage {
            input_tokens: 12_345,
            output_tokens: 678,
            cached_input_tokens: 1_000,
            cached_output_tokens: 0,
        });
        let json = serde_json::to_vec_pretty(&session).unwrap();
        fs::write(&path, &json).unwrap();
        let loaded = Session::load(&path).unwrap();
        assert_eq!(loaded.last_usage, session.last_usage);
        assert_eq!(loaded.last_usage.unwrap().context_tokens(), 12_345);

        let old = dir.join("old.json");
        fs::write(
            &old,
            br#"{"version":2,"id":"ccddeeff00112233445566778899aabb","created_at":"2020-01-01T00:00:00Z","updated_at":"2020-01-01T00:00:00Z","model":"x","messages":[]}"#,
        )
        .unwrap();
        assert!(Session::load(&old).unwrap().last_usage.is_none());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn persist_messages_records_usage_and_none_keeps_last() {
        let _guard = myco_home_lock();
        let dir = temp_session_root();
        // SAFETY: test-only env override; held under myco_home_lock.
        unsafe {
            std::env::set_var("MYCO_HOME", &dir);
        }

        let msg = |t: &str| Message::UserMessage {
            content: vec![Content::Text { text: t.into() }],
        };
        let usage = TokenUsage {
            input_tokens: 5_000,
            output_tokens: 100,
            cached_input_tokens: 0,
            cached_output_tokens: 0,
        };
        let active = ActiveSession::new(Session::new("claude-haiku-4-5"));
        let id = active.id();

        active
            .persist_messages(&[msg("hi")], Some(usage), true)
            .unwrap();
        assert_eq!(
            Session::load_by_id_or_prefix(&id).unwrap().last_usage,
            Some(usage)
        );

        active
            .persist_messages(&[msg("hi"), msg("more")], None, true)
            .unwrap();
        assert_eq!(
            Session::load_by_id_or_prefix(&id).unwrap().last_usage,
            Some(usage)
        );

        let _ = fs::remove_dir_all(&dir);
        unsafe {
            std::env::remove_var("MYCO_HOME");
        }
    }

    #[test]
    fn reject_wrong_version() {
        let dir = temp_session_root();
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("old.json");
        fs::write(
            &path,
            br#"{"version":1,"id":"aa","created_at":"2020-01-01T00:00:00Z","updated_at":"2020-01-01T00:00:00Z","model":"x","messages":[]}"#,
        )
        .unwrap();
        let err = Session::load(&path).unwrap_err();
        assert!(err.contains("unsupported session version"), "{err}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn active_session_auto_title_once() {
        let _guard = myco_home_lock();
        let dir = temp_session_root();
        // SAFETY: test-only env override; held under myco_home_lock.
        unsafe {
            std::env::set_var("MYCO_HOME", &dir);
        }
        let s = ActiveSession::new(Session::new("claude-haiku-4-5"));
        assert!(
            s.maybe_auto_title_from_user_text("First line\n\nmore")
                .unwrap()
        );
        assert_eq!(s.snapshot().title.as_deref(), Some("First line"));
        assert!(!s.maybe_auto_title_from_user_text("Second").unwrap());
        assert_eq!(s.snapshot().title.as_deref(), Some("First line"));
        let _ = fs::remove_dir_all(&dir);
        unsafe {
            std::env::remove_var("MYCO_HOME");
        }
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
        let _guard = myco_home_lock();
        let dir = temp_session_root();
        // SAFETY: test-only env override; held under myco_home_lock.
        unsafe {
            std::env::set_var("MYCO_HOME", &dir);
        }

        let mut visible = Session::new("claude-haiku-4-5");
        visible.messages.push(Message::UserMessage {
            content: vec![Content::Text {
                text: "visible".into(),
            }],
        });
        visible.save().unwrap();

        let mut hidden = Session::new_hidden(
            "claude-haiku-4-5",
            "bbccddeeff00112233445566778899aa",
            SessionKind::Subagent,
            Some(visible.id.clone()),
        );
        hidden.messages.push(Message::UserMessage {
            content: vec![Content::Text {
                text: "hidden subagent".into(),
            }],
        });
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
        assert!(all.iter().any(|e| e.id == hidden.id && e.kind.is_hidden()));

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

        let _ = fs::remove_dir_all(&dir);
        unsafe {
            std::env::remove_var("MYCO_HOME");
        }
    }

    #[test]
    fn old_session_json_defaults_kind_user() {
        let dir = temp_session_root();
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("legacy.json");
        // No kind/parent fields — serde defaults to user (visible).
        fs::write(
            &path,
            br#"{"version":2,"id":"ccddeeff00112233445566778899aabb","created_at":"2020-01-01T00:00:00Z","updated_at":"2020-01-01T00:00:00Z","model":"x","messages":[]}"#,
        )
        .unwrap();
        let s = Session::load(&path).unwrap();
        assert!(!s.is_hidden());
        assert_eq!(s.kind, SessionKind::User);
        assert!(s.parent_session_id.is_none());
        let _ = fs::remove_dir_all(&dir);
    }
}
