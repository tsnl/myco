//! Durable, bounded event inboxes. The session writer owns each inbox file.
//!
//! Delivery is a two-document handoff: claim before appending the input, then
//! acknowledge its durable transcript marker. Recovery reconciles that marker
//! across every thread before retrying an unacknowledged claim.

use std::collections::HashSet;
use std::io::Read;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::Session;
use crate::core::atomically_write;
use crate::generative_model::Content;

pub const MAX_PENDING: usize = 100;
pub const MAX_RECENT: usize = 100;
const MAX_FILE_BYTES: u64 = 16 * 1024 * 1024;

//
// Event contracts
//

#[derive(
    Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum Priority {
    #[default]
    Normal,
    Urgent,
    TimeSensitive,
}

impl Priority {
    pub fn interrupts(self) -> bool {
        self != Self::Normal
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    Timer,
    Trigger,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum State {
    Pending,
    Delivering,
    Running,
    Completed,
    Cancelled,
    Expired,
    Interrupted,
    Failed,
}

impl State {
    pub fn active(self) -> bool {
        matches!(self, Self::Pending | Self::Delivering | Self::Running)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EventRequest {
    /// Stable caller-generated UUID. Retrying the same request is idempotent.
    #[schemars(with = "String")]
    pub event_id: Uuid,
    pub message: String,
    #[serde(default)]
    pub priority: Priority,
    /// Omit for immediate delivery; otherwise use an RFC 3339 timestamp.
    #[schemars(with = "Option<String>")]
    pub at: Option<DateTime<Utc>>,
    /// Drop this event if delivery has not started by this timestamp.
    #[schemars(with = "Option<String>")]
    pub expires_at: Option<DateTime<Utc>>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Entry {
    pub request: EventRequest,
    /// Editable delivery text; the original request remains an idempotency key.
    pub message: String,
    pub source: Source,
    pub accepted_at: DateTime<Utc>,
    pub due_at: DateTime<Utc>,
    pub state: State,
    pub held: bool,
    pub finished_at: Option<DateTime<Utc>>,
}

impl Entry {
    pub fn ready(&self, now: DateTime<Utc>) -> bool {
        self.state == State::Pending && !self.held && self.due_at <= now
    }
}

#[derive(Clone, Deserialize, Serialize)]
struct Document {
    version: u32,
    session_id: String,
    entries: Vec<Entry>,
}

pub struct Inbox {
    path: PathBuf,
    document: Document,
}

//
// Storage and atomic mutations
//

impl Inbox {
    pub fn path_for(session_id: &str) -> Result<PathBuf, String> {
        Uuid::parse_str(session_id).map_err(|_| "Invalid inbox session ID".to_owned())?;
        Ok(crate::core::myco_home()?
            .join("inbox")
            .join(format!("{session_id}.json")))
    }

    pub fn open(path: PathBuf, session_id: &str) -> Result<Self, String> {
        let document = match std::fs::File::open(&path) {
            Ok(file) => read_document(file, &path, session_id)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Document {
                version: 1,
                session_id: session_id.into(),
                entries: vec![],
            },
            Err(error) => return Err(format!("Read inbox {}: {error}", path.display())),
        };
        Ok(Self { path, document })
    }

    pub fn entries(&self) -> &[Entry] {
        &self.document.entries
    }

    pub fn get(&self, id: Uuid) -> Option<&Entry> {
        self.entries()
            .iter()
            .find(|entry| entry.request.event_id == id)
    }

    fn update<T>(
        &mut self,
        change: impl FnOnce(&mut Vec<Entry>) -> Result<T, String>,
    ) -> Result<T, String> {
        let mut next = self.document.clone();
        let result = change(&mut next.entries)?;
        if next.entries != self.document.entries {
            prune(&mut next.entries);
            let data = serde_json::to_vec(&next).map_err(|e| e.to_string())?;
            if let Some(parent) = self.path.parent() {
                std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
            }
            atomically_write(&self.path, &data)?;
            self.document = next;
        }
        Ok(result)
    }

    pub fn create(
        &mut self,
        request: EventRequest,
        source: Source,
        now: DateTime<Utc>,
    ) -> Result<Entry, String> {
        if let Some(entry) = self.get(request.event_id) {
            return if entry.request == request && entry.source == source {
                Ok(entry.clone())
            } else {
                Err("Event ID already belongs to a different request.".into())
            };
        }
        validate_request(&request, now)?;
        self.update(|entries| {
            if entries.iter().filter(|e| e.state.active()).count() >= MAX_PENDING {
                return Err("The inbox is full (100 pending or running events).".into());
            }
            if source == Source::Timer
                && entries
                    .iter()
                    .filter(|e| e.source == Source::Timer && e.state.active())
                    .count()
                    >= 20
            {
                return Err("This session already has 20 pending timers.".into());
            }
            let entry = Entry {
                due_at: request.at.unwrap_or(now),
                message: request.message.clone(),
                request,
                source,
                accepted_at: now,
                state: State::Pending,
                held: false,
                finished_at: None,
            };
            entries.push(entry.clone());
            Ok(entry)
        })
    }

    pub fn cancel(&mut self, id: Uuid, now: DateTime<Utc>) -> Result<(), String> {
        self.update(|entries| {
            let entry = find(entries, id)?;
            if entry.state == State::Cancelled {
                return Ok(());
            }
            if entry.state != State::Pending {
                return Err("This event is already being delivered or has finished.".into());
            }
            finish(entry, State::Cancelled, now);
            Ok(())
        })
    }

    pub fn edit(&mut self, id: Uuid, message: Option<&str>, held: bool) -> Result<(), String> {
        self.update(|entries| {
            let entry = find(entries, id)?;
            if entry.state != State::Pending {
                return Err("This event is already being delivered or has finished.".into());
            }
            if let Some(message) = message {
                validate_message(message)?;
                entry.message = message.into();
            }
            entry.held = held;
            Ok(())
        })
    }

    pub fn expire(&mut self, now: DateTime<Utc>) -> Result<(), String> {
        self.update(|entries| {
            for entry in entries {
                if entry.state == State::Pending
                    && entry.request.expires_at.is_some_and(|at| at <= now)
                {
                    finish(entry, State::Expired, now);
                }
            }
            Ok(())
        })
    }

    pub fn claim(&mut self, id: Uuid, now: DateTime<Utc>) -> Result<(), String> {
        self.update(|entries| {
            let entry = find(entries, id)?;
            if !entry.ready(now) || entry.request.expires_at.is_some_and(|at| at <= now) {
                return Err("Event is not ready for delivery.".into());
            }
            entry.state = State::Delivering;
            Ok(())
        })
    }

    pub fn acknowledge(&mut self, id: Uuid) -> Result<(), String> {
        self.update(|entries| {
            let entry = find(entries, id)?;
            if entry.state != State::Delivering {
                return Err("Event has no outstanding delivery claim.".into());
            }
            entry.state = State::Running;
            Ok(())
        })
    }

    /// Reconcile claims against durable input, never against an in-memory draft.
    /// Call on startup with Interrupted, and at turn settlement with its outcome.
    pub fn settle(
        &mut self,
        session: &Session,
        outcome: State,
        now: DateTime<Utc>,
    ) -> Result<(), String> {
        if outcome.active() {
            return Err("Inbox settlement requires a terminal outcome.".into());
        }
        let delivered = delivered_ids(session);
        self.update(|entries| {
            for entry in entries {
                match entry.state {
                    State::Running => finish(entry, outcome, now),
                    State::Delivering | State::Pending
                        if delivered.contains(&entry.request.event_id) =>
                    {
                        finish(entry, outcome, now)
                    }
                    State::Delivering => entry.state = State::Pending,
                    _ => {}
                }
            }
            Ok(())
        })
    }

    pub fn release(&mut self, id: Uuid) -> Result<(), String> {
        self.update(|entries| {
            let entry = find(entries, id)?;
            if entry.state == State::Delivering {
                entry.state = State::Pending;
            }
            Ok(())
        })
    }
}

//
// Validation and reconciliation
//

fn validate_message(message: &str) -> Result<(), String> {
    if message.trim().is_empty() || message.chars().count() > 8000 {
        return Err("Event message must contain 1–8000 characters.".into());
    }
    Ok(())
}

fn validate_request(request: &EventRequest, now: DateTime<Utc>) -> Result<(), String> {
    validate_message(&request.message)?;
    let due = request.at.unwrap_or(now);
    if due > now + chrono::Duration::days(30) {
        return Err("Event must be due within 30 days.".into());
    }
    if request
        .expires_at
        .is_some_and(|expiry| expiry <= due.max(now))
    {
        return Err("expires_at must be after the due time and the current time.".into());
    }
    Ok(())
}

fn find(entries: &mut [Entry], id: Uuid) -> Result<&mut Entry, String> {
    entries
        .iter_mut()
        .find(|e| e.request.event_id == id)
        .ok_or_else(|| "Event does not belong to this session or is no longer retained.".into())
}

fn finish(entry: &mut Entry, state: State, now: DateTime<Utc>) {
    entry.state = state;
    entry.finished_at = Some(now);
}

fn prune(entries: &mut Vec<Entry>) {
    let mut finished: Vec<_> = entries
        .iter()
        .filter(|entry| !entry.state.active())
        .collect();
    let excess = finished.len().saturating_sub(MAX_RECENT);
    finished.sort_by_key(|entry| entry.finished_at.unwrap_or(entry.accepted_at));
    let expired: HashSet<_> = finished
        .into_iter()
        .take(excess)
        .map(|entry| entry.request.event_id)
        .collect();
    entries.retain(|entry| !expired.contains(&entry.request.event_id));
}

fn delivered_ids(session: &Session) -> HashSet<Uuid> {
    session
        .threads()
        .iter()
        .flat_map(|thread| &thread.messages)
        .flat_map(|message| message.content())
        .filter_map(|part| match part {
            Content::System { kind, data, .. } if kind == "timer" || kind == "inbox_event" => {
                data["id"].as_str().and_then(|id| Uuid::parse_str(id).ok())
            }
            _ => None,
        })
        .collect()
}

fn read_document(file: std::fs::File, path: &Path, session_id: &str) -> Result<Document, String> {
    let mut bytes = Vec::new();
    file.take(MAX_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| format!("Read inbox {}: {e}", path.display()))?;
    if bytes.len() as u64 > MAX_FILE_BYTES {
        return Err("Inbox file exceeds 16 MiB.".into());
    }
    let doc: Document = serde_json::from_slice(&bytes)
        .map_err(|e| format!("Parse inbox {}: {e}", path.display()))?;
    if doc.version != 1 || doc.session_id != session_id {
        return Err("Unsupported inbox version or mismatched session ID.".into());
    }
    let mut ids = HashSet::new();
    for entry in &doc.entries {
        validate_message(&entry.request.message)?;
        validate_message(&entry.message)?;
        if !ids.insert(entry.request.event_id) {
            return Err("Inbox contains duplicate event IDs.".into());
        }
    }
    if doc.entries.len() > MAX_PENDING + MAX_RECENT
        || doc.entries.iter().filter(|e| e.state.active()).count() > MAX_PENDING
    {
        return Err("Inbox exceeds its entry limit.".into());
    }
    Ok(doc)
}

#[cfg(test)]
#[path = "inbox/tests.rs"]
mod tests;
