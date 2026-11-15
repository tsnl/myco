//! The live session shared by a runner and its tools. Changes are saved to a
//! staged copy first, so a failed save leaves the live session unchanged.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};

use super::{Session, SessionWriteLock, Thread, archive, auto_title_from_text, resolve_session_id};
use crate::generative_model::{Message, TokenUsage};

/// Shared handle so the server and `session_meta` tool mutate the same live session.
#[derive(Clone)]
pub struct ActiveSession {
    inner: Arc<Mutex<Session>>,
    writer: Arc<tokio::sync::Mutex<()>>,
    checkpoint_epoch: Arc<AtomicU64>,
}

impl ActiveSession {
    pub fn new(session: Session) -> Self {
        Self {
            inner: Arc::new(Mutex::new(session)),
            writer: Arc::new(tokio::sync::Mutex::new(())),
            checkpoint_epoch: Arc::new(AtomicU64::new(0)),
        }
    }

    pub async fn writer(&self) -> SessionWriter {
        SessionWriter {
            session: self.clone(),
            _guard: self.writer.clone().lock_owned().await,
        }
    }

    pub(crate) fn begin_checkpoints(&self) -> u64 {
        self.checkpoint_epoch.fetch_add(1, Ordering::SeqCst) + 1
    }

    pub(crate) fn checkpoint_is_current(&self, epoch: u64) -> bool {
        self.checkpoint_epoch.load(Ordering::SeqCst) == epoch
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

    /// Edit the live session without saving it. The next checkpoint persists
    /// the edit; durable changes go through [`Self::update`].
    pub fn with_mut<R>(&self, f: impl FnOnce(&mut Session) -> R) -> R {
        f(&mut self.lock())
    }

    /// Apply `f` to a copy and save it before it replaces the live session. A
    /// failed edit or save leaves the live session unchanged, so no later
    /// checkpoint can persist a change its caller reported as failed.
    pub fn update<T>(
        &self,
        f: impl FnOnce(&mut Session) -> Result<T, String>,
    ) -> Result<T, String> {
        let mut current = self.lock();
        let mut updated = current.clone();
        let value = f(&mut updated)?;
        store(&mut current, updated)?;
        Ok(value)
    }

    /// Persist before exposing a mode change to a running session.
    pub fn set_auto_continue(&self, enabled: bool) -> Result<(), String> {
        self.update(|session| {
            session.auto_continue = enabled;
            Ok(())
        })
    }

    /// Save a title without replacing concurrent conversation or tool updates.
    pub fn rename(&self, title: String) -> Result<(), String> {
        self.update(|session| session.set_title(Some(title)))
    }

    /// Record the selected catalog key. A session without a file keeps it in
    /// memory until its first save: choosing a model does not create a session.
    pub fn set_model_key(&self, key: &str) -> Result<(), String> {
        let mut current = self.lock();
        if current.model == key {
            return Ok(());
        }
        if !current.json_path().exists() {
            current.model = key.into();
            return Ok(());
        }
        let mut updated = current.clone();
        updated.model = key.into();
        store(&mut current, updated)
    }

    /// Archive only this session, preserving threads, lineage and live tools.
    pub fn set_archived(&self, archived: bool) -> Result<(), String> {
        let mut current = self.lock();
        let relocation = archive::Relocation::prepare(&current.id, archived)?;
        let mut updated = current.clone();
        updated.archived = archived;
        updated.touch();
        // Keep the archived flag until restore reaches the active directory.
        // A failed or interrupted move must remain visible in archived listings.
        if !archived {
            relocation.apply()?;
            updated.save_externalized()?;
            *current = updated;
            return Ok(());
        }
        updated.save_externalized()?;
        *current = updated;
        relocation.apply().map_err(|error| {
            format!("Archive status saved, but files could not all be moved; retry /archive {}: {error}", current.id)
        })
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

    /// Persist an agent checkpoint to `thread_id`, which must still be active.
    /// Missing usage explicitly invalidates the old usage estimate, as after a
    /// rewind or context replacement; `accepted` records when a user turn
    /// arrived.
    pub fn persist_agent_state(
        &self,
        thread_id: &str,
        state: &crate::agent::AgentState,
        force: bool,
        accepted: Option<(usize, DateTime<Utc>)>,
    ) -> Result<(), String> {
        let messages = state.history();
        let last_usage = state.last_usage();
        let pending = state.pending_operation();
        let mut session = self.lock();
        if session.active_thread().id != thread_id {
            return Err(format!(
                "thread {thread_id} is no longer active; refusing a stale checkpoint"
            ));
        }
        let usage_changed = last_usage != session.active_thread().last_usage;
        if force
            || messages != session.active_thread().messages
            || usage_changed
            || pending != session.active_thread().pending_operation
        {
            crate::agent::validate_checkpoint(messages, pending)
                .map_err(|error| error.to_string())?;
            let mut updated = session.clone();
            updated.active_thread_mut().messages = messages.to_vec();
            updated.active_thread_mut().pending_operation = pending;
            let times = &mut updated.active_thread_mut().user_turn_timestamps;
            times.retain(|&index, _| messages.get(index).is_some_and(Message::is_user_turn));
            if let Some((index, time)) = accepted
                && messages.get(index).is_some_and(Message::is_user_turn)
            {
                times.entry(index).or_insert(time);
            }
            let previous = session.active_thread();
            updated.active_thread_mut().context_tokens_estimate =
                if last_usage.is_none() && messages.len() >= previous.messages.len() {
                    previous
                        .last_usage
                        .map(TokenUsage::context_tokens)
                        .or(previous.context_tokens_estimate)
                } else {
                    None
                };
            updated.active_thread_mut().last_usage = last_usage;
            store(&mut session, updated)?;
        }
        Ok(())
    }

    /// Set title if currently unset, from the first user line. Returns true if set.
    pub fn maybe_auto_title_from_user_text(&self, text: &str) -> Result<bool, String> {
        // Every accepted submission calls this; only an untitled session saves.
        if self.with(|session| session.title.is_some()) {
            return Ok(false);
        }
        let Some(title) = auto_title_from_text(text) else {
            return Ok(false);
        };
        self.update(|session| {
            let untitled = session.title.is_none();
            if untitled {
                session.title = Some(title);
            }
            Ok(untitled)
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Session> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// Save a changed copy before it replaces the live session, so a failed save
/// leaves the session as it was.
fn store(current: &mut Session, mut updated: Session) -> Result<(), String> {
    updated.touch();
    updated.save_externalized()?;
    *current = updated;
    Ok(())
}

pub struct SessionWriter {
    session: ActiveSession,
    _guard: tokio::sync::OwnedMutexGuard<()>,
}

impl SessionWriter {
    pub fn commit_thread(&self, thread: Thread) -> Result<(), String> {
        crate::agent::validate_context(&thread.messages).map_err(|error| error.to_string())?;
        if thread.pending_operation.is_some() {
            return Err("cannot install a successor with an outstanding operation".into());
        }
        let mut session = self.session.lock();
        if thread.predecessor_id.as_deref() != Some(&session.active_thread().id) {
            return Err("compaction predecessor is no longer the active thread".into());
        }
        let mut successor = session.clone();
        successor.threads.push(thread);
        super::thread::validate_threads(&successor.threads)?;
        store(&mut session, successor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{persist_messages, temp_home, user};

    #[test]
    fn context_persistence_records_equal_length_changes_and_invalidates_usage() {
        let _home = temp_home("session-persist");

        let usage = TokenUsage {
            input_tokens: 5_000,
            output_tokens: 100,
            cached_input_tokens: 0,
        };
        let active = ActiveSession::new(Session::new("claude-haiku-4-5"));
        let id = active.id();
        let thread = active.with(|session| session.active_thread().id.clone());

        persist_messages(&active, &thread, &[user("hi")], Some(usage), true).unwrap();
        assert_eq!(
            Session::load_by_id_or_prefix(&id)
                .unwrap()
                .active_thread()
                .last_usage,
            Some(usage)
        );

        persist_messages(&active, &thread, &[user("replaced")], None, false).unwrap();
        assert_eq!(
            Session::load_by_id_or_prefix(&id)
                .unwrap()
                .active_thread()
                .last_usage,
            None
        );
        assert_eq!(
            active.snapshot().active_thread().messages,
            vec![user("replaced")]
        );
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
}
