//! Conversation session persistence and metadata.
//!
//! Sessions live under the selected profile's `session/{shard}/{id}.json`, or
//! `session/archived/{shard}/{id}.json`, with sibling histories and transcripts.
//! Each document owns ordered threads and shared
//! metadata. Version 2 loads as one thread; saves use [`SESSION_FILE_VERSION`].
//!
//! Persistence only: how a conversation is stored, not how one is produced.
//! Compaction is split along that line — the document work
//! ([`compact_thread`]) is here, and the
//! agent run that writes the summary is [`crate::chat::ModelCompactor`].

mod active;
mod archive;
mod attach;
mod compact;
pub mod inbox;
mod links;
mod listing;
mod lock;
mod search;
mod thread;

pub use thread::Thread;

pub use active::{ActiveSession, SessionWriter};
pub use archive::migrate_archived_sessions;
pub use attach::{MAX_MESSAGE_ATTACHMENT_BYTES, expand_image_attachments};
pub use compact::{CompactOutcome, compact_thread};
pub use links::{LinkCounts, SessionLink, normalize_pr_url, parse_pr_fields};
pub use listing::{
    ArchiveFilter, SessionListEntry, iter_session_json_files, list_sessions,
    list_sessions_with_filter, resolve_session_id, session_file_path, session_root,
};
pub use lock::{SessionLockError, SessionWriteLock};
pub use search::{SessionSearchReport, search_sessions};

use std::fs;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use uuid::Uuid;

use crate::core::{atomically_write, uuid_simple_hex};
use crate::generative_model::{Message, TokenUsage};

/// Written schema version; versions 2 through 5 are upgraded on read.
pub const SESSION_FILE_VERSION: u32 = 6;
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
    /// Nested agent run (server API `parent_session`; also written by the
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
    /// Opt-in continuation after a model finishes. Does not start work on load.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub auto_continue: bool,
    #[serde(deserialize_with = "thread::deserialize_threads")]
    threads: Vec<Thread>,
    /// Short human label; agent maintained.
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

impl Session {
    /// `model` is the catalog key from config.toml (recorded as metadata; a
    /// resumed session runs on whatever model the server selects).
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
            auto_continue: false,
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
        thread.context_tokens_estimate = None;
        thread.pending_operation = None;
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
        child.active_thread_mut().context_tokens_estimate =
            self.active_thread().context_tokens_estimate;
        child.active_thread_mut().pending_operation = self.active_thread().pending_operation;
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

    /// Legacy plain-text console mirror, retained for transcript search: `{id}.console`.
    pub fn console_path(&self) -> PathBuf {
        session_file_path(&self.id, "console")
    }

    pub fn externalize_images(&mut self) -> Result<(), String> {
        let store = crate::core::image_store::ImageStore::for_profile()?;
        for thread in &mut self.threads {
            store.externalize_messages(&mut thread.messages)?;
        }
        self.version = SESSION_FILE_VERSION;
        Ok(())
    }

    /// Write a copy with inline images moved to the profile's image store.
    pub fn save(&self) -> Result<(), String> {
        self.clone().save_externalized()
    }

    /// Move inline images to the image store in place, then write. The live
    /// session keeps the same references the file holds, so later saves only
    /// externalize new images; externalizing a reference only validates it.
    fn save_externalized(&mut self) -> Result<(), String> {
        self.externalize_images()?;
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
        let legacy = matches!(value["version"].as_u64(), Some(2..=4));
        match value["version"].as_u64() {
            Some(2) => thread::upgrade_v2(&mut value)?,
            Some(3..=5) => value["version"] = serde_json::json!(SESSION_FILE_VERSION),
            Some(6) => {}
            version => {
                return Err(format!(
                    "unsupported session version {version:?}; expected 2 through {SESSION_FILE_VERSION}"
                ));
            }
        }
        let mut session: Self = serde_json::from_value(value).map_err(|e| e.to_string())?;
        if legacy {
            thread::upgrade_system_parts(&mut session.threads);
        }
        if session.id.is_empty() {
            return Err("session has empty id".into());
        }
        Ok(session)
    }

    pub fn load_by_id_or_prefix(id_or_prefix: &str) -> Result<Self, String> {
        let id = resolve_session_id(id_or_prefix)?;
        let path = session_file_path(&id, "json");
        match Self::load(&path) {
            // Readers do not hold the writer lock; archive/restore may rename
            // the file between resolving its location and opening it.
            Err(_) if !path.exists() => Self::load(&session_file_path(&id, "json")),
            result => result,
        }
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
}

// ---------------------------------------------------------------------------
// Titles and labels
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
                Content::System {
                    kind: "session".into(),
                    data: serde_json::json!({}),
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
    fn scratchpad_cap() {
        let mut s = Session::new("claude-haiku-4-5");
        let big = "a".repeat(MAX_SCRATCHPAD_BYTES + 1);
        assert!(s.set_scratchpad(big).is_err());
        s.set_scratchpad("ok".into()).unwrap();
        assert_eq!(s.scratchpad, "ok");
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
