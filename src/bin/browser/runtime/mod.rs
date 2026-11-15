//! Session workers own runners, writer locks, and live tools independently of browser tabs.
//!
//! This module defines the live state shared by its children: `app` publishes
//! the projection, `sessions` registers workers, `worker` runs and admits
//! actions, and `queue`, `timers`, and `service` own their parts of admission.

use std::borrow::Cow;
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use myco::CancelToken;
use myco::generative_model::TokenUsage;
use myco::session::inbox::{Entry as InboxEntry, Inbox};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::{broadcast, mpsc};
use uuid::Uuid;

use super::attachments;
use super::view::{self, Block};
use crate::Session;

mod app;
mod queue;
mod service;
mod sessions;
mod timers;
mod worker;

#[cfg(test)]
mod inbox_tests;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod timers_tests;

use queue::{QueueUpdate, QueuedMessage};
pub(super) use sessions::{CreateSession, Sessions};
pub(super) use timers::pending_inboxes;

#[derive(Debug)]
pub(super) enum Error {
    Invalid(String),
    NotFound(String),
    Gone(String),
    Conflict(String),
    Unavailable(String),
    Internal(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (Self::Invalid(message)
        | Self::NotFound(message)
        | Self::Gone(message)
        | Self::Conflict(message)
        | Self::Unavailable(message)
        | Self::Internal(message)) = self;
        f.write_str(message)
    }
}

type Result<T> = std::result::Result<T, Error>;

/// Run blocking work off the async runtime.
pub(super) async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> T + Send + 'static,
) -> Result<T> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|error| Error::Internal(error.to_string()))
}

/// The worker's state as the browser labels it; variant names are the wire strings.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
enum Status {
    #[default]
    Ready,
    Running,
    Compacting,
    Retrying,
    Cancelling,
    Stopped,
}

/// Untitled sessions show this until they are renamed.
const UNTITLED: &str = "New session";

/// What the saved session document says about itself.
#[derive(Default, PartialEq, Serialize)]
struct SessionFacts {
    title: String,
    archived: bool,
    auto_continue: bool,
    usage: Option<TokenUsage>,
}

impl From<&Session> for SessionFacts {
    fn from(session: &Session) -> Self {
        Self {
            title: session.title.clone().unwrap_or_else(|| UNTITLED.into()),
            archived: session.archived,
            auto_continue: session.auto_continue,
            usage: session.active_thread().last_usage,
        }
    }
}

/// The full browser projection: `meta` changes alone publish as `meta` updates.
#[derive(Default, Serialize)]
struct Snapshot {
    #[serde(skip)]
    revision: u64,
    #[serde(flatten)]
    meta: Metadata,
    earlier_threads: Vec<String>,
    models: Vec<String>,
    blocks: Vec<Block>,
}

#[derive(Default, Serialize)]
struct Metadata {
    session_id: String,
    thread_id: String,
    #[serde(flatten)]
    facts: SessionFacts,
    model: String,
    attachment_limits: attachments::Limits,
    context_window_tokens: u64,
    /// The accepted run's token: present exactly while the worker is busy.
    #[serde(rename = "busy", serialize_with = "serialize_busy")]
    cancel: Option<CancelToken>,
    status: Status,
    queued: VecDeque<QueuedMessage>,
    timers: Vec<timers::Timer>,
    inbox: Vec<InboxEntry>,
    down_hosts: Vec<DownHost>,
}

/// A remote host whose last connection attempt or connection failed. Its
/// next tool call retries; idle hosts are not down.
#[derive(Clone, PartialEq, Serialize)]
struct DownHost {
    name: String,
    since: DateTime<Utc>,
    error: String,
}

fn serialize_busy<S: serde::Serializer>(
    cancel: &Option<CancelToken>,
    serializer: S,
) -> std::result::Result<S::Ok, S::Error> {
    serializer.serialize_bool(cancel.is_some())
}

impl Metadata {
    fn busy(&self) -> bool {
        self.cancel.is_some()
    }

    fn cancelling(&self) -> bool {
        self.status == Status::Cancelling
    }

    /// Progress events cannot relabel a cancelled run: `Cancelling` holds
    /// until the run settles and [`App::sync_with`] records its outcome.
    fn progress(&mut self, status: Status) -> bool {
        if self.cancelling() || self.status == status {
            return false;
        }
        self.status = status;
        true
    }
}

struct Live {
    inbox: Inbox,
    // Only message blocks from the current, unvalidated generation are replaceable.
    generation_start: Option<usize>,
    retry_notice: Option<usize>,
    background: HashMap<Uuid, CancelToken>,
    snapshot: Snapshot,
    accepted: HashMap<Uuid, ActionRequest>,
    service: service::Receipts,
}

#[derive(Clone, Serialize)]
pub(super) struct Update {
    session_id: String,
    revision: u64,
    pub(super) change: Value,
}

impl Update {
    // The browser fetches histories only for subscribed sessions. Relaying full
    // snapshots of every running session can make lag recovery itself lag. A
    // bounded splice rides along; tabs that cannot apply it refetch as before.
    pub(super) fn live_update(&self) -> Cow<'_, Self> {
        if self.change["kind"] != "snapshot" {
            return Cow::Borrowed(self);
        }
        let mut change = json!({"kind":"refresh"});
        if let Some(splice) = self.change.get("splice") {
            change["splice"] = splice.clone();
        }
        Cow::Owned(Self {
            session_id: self.session_id.clone(),
            revision: self.revision,
            change,
        })
    }
}

pub(super) struct App {
    clock: timers::Clock,
    live: Mutex<Live>,
    events: broadcast::Sender<Arc<Update>>,
    work: mpsc::Sender<Work>,
    shutdown: CancelToken,
    generation: Arc<AtomicU64>,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum Action {
    Submit {
        #[serde(default)]
        text: String,
        #[serde(default)]
        images: Vec<String>,
    },
    UpdateQueued {
        message_id: Uuid,
        revision: u64,
        update: QueueUpdate,
    },
    CancelTimer {
        timer_id: Uuid,
    },
    #[serde(skip)]
    Timer {
        message: QueuedMessage,
    },
    #[serde(skip)]
    ServiceSubmit {
        text: String,
        images: Vec<String>,
    },
    Compact,
    SelectModel {
        key: String,
    },
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub(super) struct ActionRequest {
    request_id: Uuid,
    pub(super) session_id: String,
    action: Action,
}

struct Work {
    request: ActionRequest,
    cancel: CancelToken,
    accepted_at: DateTime<Utc>,
}
