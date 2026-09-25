//! Session workers own runners, writer locks, and live tools independently of browser tabs.

use std::borrow::Cow;
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use myco::generative_model::TokenUsage;
use myco::session::{ActiveSession, ArchiveFilter, SessionListEntry, SessionWriteLock};
use myco::{AgentEvent, CancelToken, EventSink};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::{broadcast, mpsc};
use uuid::Uuid;

use super::super::{
    Args, Boot, Config, Session, StartupPreflight, WorkflowEvent, boot_session, persist_session,
};
use super::attachments;
use super::view::{self, Block};

#[path = "queue.rs"]
mod queue;
use queue::{MAX_QUEUED_MESSAGES, QueueUpdate, QueuedMessage};

#[cfg(test)]
#[path = "tests.rs"]
mod tests;

#[derive(Debug)]
pub(super) enum Error {
    Invalid(String),
    NotFound(String),
    Conflict(String),
    Unavailable(String),
    Internal(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (Self::Invalid(message)
        | Self::NotFound(message)
        | Self::Conflict(message)
        | Self::Unavailable(message)
        | Self::Internal(message)) = self;
        f.write_str(message)
    }
}

type Result<T> = std::result::Result<T, Error>;

#[derive(Clone, Serialize)]
struct Snapshot {
    #[serde(skip)]
    revision: u64,
    session_id: String,
    thread_id: String,
    title: String,
    model: String,
    models: Vec<String>,
    attachment_limits: attachments::Limits,
    usage: Option<TokenUsage>,
    context_window_tokens: u64,
    busy: bool,
    status: String,
    tasks: Vec<String>,
    queued: VecDeque<QueuedMessage>,
    blocks: Vec<Block>,
}

impl Snapshot {
    fn metadata(&self) -> Value {
        json!({
            "session_id": self.session_id, "thread_id": self.thread_id,
            "title": self.title, "model": self.model,
            "busy": self.busy, "status": self.status, "queued": self.queued,
            "attachment_limits": self.attachment_limits,
            "usage": self.usage, "context_window_tokens": self.context_window_tokens,
        })
    }
}

struct Live {
    // Only message blocks from the current, unvalidated generation are replaceable.
    generation_start: Option<usize>,
    background: HashMap<Uuid, CancelToken>,
    snapshot: Snapshot,
    cancel: Option<CancelToken>,
    accepted: HashMap<Uuid, ActionRequest>,
}

#[derive(Clone, Serialize)]
pub(super) struct Update {
    session_id: String,
    revision: u64,
    pub(super) change: Value,
}

impl Update {
    // The browser fetches histories only for subscribed sessions. Relaying full
    // snapshots of every running session can make lag recovery itself lag.
    pub(super) fn live_update(&self) -> Cow<'_, Self> {
        if self.change["kind"] != "snapshot" {
            return Cow::Borrowed(self);
        }
        Cow::Owned(Self {
            session_id: self.session_id.clone(),
            revision: self.revision,
            change: json!({"kind":"refresh"}),
        })
    }
}

pub(super) struct App {
    live: Mutex<Live>,
    events: broadcast::Sender<Arc<Update>>,
    work: mpsc::Sender<Work>,
    shutdown: CancelToken,
    generation: Arc<AtomicU64>,
}

impl App {
    pub(super) fn background(&self, session_id: &str, call_id: Uuid) -> Result<()> {
        let mut live = self.live.lock().unwrap();
        if live.snapshot.session_id != session_id || live.snapshot.status == "Cancelling" {
            return Err(Error::Conflict(
                "The session changed or is cancelling.".into(),
            ));
        }
        let token = live.background.get(&call_id).ok_or_else(|| {
            Error::Conflict("This tool call is no longer running or cannot be backgrounded.".into())
        })?;
        token.cancel();
        if let Some(index) = live.snapshot.blocks.iter().position(|block| matches!(block, Block::Tool { call_id: id, running: true, .. } if *id == call_id)) {
            if let Block::Tool { background_id, status, .. } = &mut live.snapshot.blocks[index] {
                *background_id = None;
                *status = "backgrounding".into();
            }
            let change = json!({"kind":"block", "index":index, "block":live.snapshot.blocks[index]});
            self.publish(&mut live.snapshot, change);
        }
        Ok(())
    }

    fn stop(&self) {
        self.shutdown.cancel();
        if let Some(cancel) = &self.live.lock().unwrap().cancel {
            cancel.cancel();
        }
    }

    fn publish(&self, snapshot: &mut Snapshot, change: Value) {
        snapshot.revision += 1;
        let _ = self.events.send(Arc::new(Update {
            session_id: snapshot.session_id.clone(),
            revision: snapshot.revision,
            change,
        }));
    }

    pub(super) fn snapshot(&self) -> Update {
        let live = self.live.lock().unwrap();
        Update {
            session_id: live.snapshot.session_id.clone(),
            revision: live.snapshot.revision,
            change: json!({"kind":"snapshot", "snapshot":live.snapshot}),
        }
    }

    fn notice(&self, text: impl Into<String>) {
        self.append_block(Block::Notice { text: text.into() });
    }

    fn compacting(&self) {
        self.status("Compacting");
    }

    fn finish_compaction(&self) {
        self.append_block(Block::Boundary {
            time: view::timestamp(&Utc::now()),
        });
        self.status("Running");
    }

    fn warning(&self, text: String) {
        let compacting = self.live.lock().unwrap().snapshot.status == "Compacting";
        if compacting {
            self.status("Running");
        }
        self.notice(text);
    }

    fn status(&self, status: &str) {
        let mut live = self.live.lock().unwrap();
        if live.snapshot.status != "Cancelling" && live.snapshot.status != status {
            live.snapshot.status = status.into();
            let change = json!({"kind":"meta", "meta":live.snapshot.metadata()});
            self.publish(&mut live.snapshot, change);
        }
    }

    fn append_block(&self, block: Block) {
        let mut live = self.live.lock().unwrap();
        let index = live.snapshot.blocks.len();
        live.snapshot.blocks.push(block.clone());
        self.publish(
            &mut live.snapshot,
            json!({"kind":"block", "index":index, "block":block}),
        );
    }

    fn discard_generation(&self) {
        let mut live = self.live.lock().unwrap();
        let Some(start) = live.generation_start.take() else {
            return;
        };
        let previous_len = live.snapshot.blocks.len();
        let mut index = 0;
        // Resource refreshes may append live tool cards while a draft is streaming.
        live.snapshot.blocks.retain(|block| {
            let keep = index < start
                || !matches!(block, Block::Message { role, .. } if role == "assistant" || role == "thinking");
            index += 1;
            keep
        });
        if live.snapshot.blocks.len() == previous_len {
            return;
        }
        let change = json!({"kind":"snapshot", "snapshot":live.snapshot});
        self.publish(&mut live.snapshot, change);
    }

    fn sync(&self, boot: &Boot, status: &str) {
        let session = boot.session.snapshot();
        let tasks = boot.runner.runtime().running_tool_summaries();
        let mut live = self.live.lock().unwrap();
        live.generation_start = None;
        let snapshot = &mut live.snapshot;
        let mut blocks = view::history(session.active_thread());
        if snapshot.thread_id == session.active_thread().id {
            view::retain_tool_timers(&mut blocks, &snapshot.blocks);
        }
        view::retain_processes(&mut blocks, &snapshot.blocks);
        snapshot.session_id = session.id.clone();
        snapshot.thread_id = session.active_thread().id.clone();
        snapshot.title = session
            .title
            .clone()
            .unwrap_or_else(|| "New session".into());
        snapshot.model = boot.catalog_model.spec.key.clone();
        snapshot.attachment_limits =
            attachments::Limits::new(boot.catalog_model.spec.max_image_base64_bytes);
        snapshot.usage = session.active_thread().last_usage;
        snapshot.context_window_tokens = boot.catalog_model.spec.context_window_tokens;
        snapshot.blocks = blocks;
        snapshot.status = status.into();
        snapshot.tasks = tasks;
        if let Err(error) = self.start_next(&mut live) {
            live.snapshot.blocks.push(Block::Notice {
                text: error.to_string(),
            });
        }
        let change = json!({"kind":"snapshot", "snapshot":live.snapshot});
        self.publish(&mut live.snapshot, change);
        self.generation.fetch_add(1, Ordering::Relaxed);
    }

    fn refresh(&self, session: &ActiveSession, tasks: Vec<String>) {
        self.tasks(tasks);
        let (title, usage) = session.with(|session| {
            (
                session
                    .title
                    .clone()
                    .unwrap_or_else(|| "New session".into()),
                session.active_thread().last_usage,
            )
        });
        let mut live = self.live.lock().unwrap();
        let title_changed = live.snapshot.title != title;
        if title_changed || live.snapshot.usage != usage {
            live.snapshot.title = title;
            live.snapshot.usage = usage;
            let change = json!({"kind":"meta", "meta":live.snapshot.metadata()});
            self.publish(&mut live.snapshot, change);
        }
        if title_changed {
            self.generation.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn tasks(&self, tasks: Vec<String>) {
        let mut live = self.live.lock().unwrap();
        if live.snapshot.tasks != tasks {
            live.snapshot.tasks = tasks;
            let change = json!({"kind":"tasks", "tasks":live.snapshot.tasks});
            self.publish(&mut live.snapshot, change);
        }
    }

    fn resources(&self, resources: Vec<myco::core::HostResources>) {
        let mut live = self.live.lock().unwrap();
        let changed = view::refresh_processes(&mut live.snapshot.blocks, &resources);
        if !changed.is_empty() {
            // Catalog subscribers do not consume transcript blocks.
            let change = json!({"kind":"meta", "meta":live.snapshot.metadata()});
            self.publish(&mut live.snapshot, change);
        }
        for index in changed {
            let change =
                json!({"kind":"block", "index":index, "block":live.snapshot.blocks[index]});
            self.publish(&mut live.snapshot, change);
        }
    }

    fn delta(&self, role: &str, text: String) {
        let mut live = self.live.lock().unwrap();
        let can_append = live
            .generation_start
            .is_none_or(|start| live.snapshot.blocks.len() > start);
        let snapshot = &mut live.snapshot;
        if let Some(Block::Message {
            role: last_role,
            text: last_text,
            ..
        }) = snapshot.blocks.last_mut()
            && last_role == role
            && can_append
        {
            last_text.push_str(&text);
            let index = snapshot.blocks.len() - 1;
            self.publish(
                snapshot,
                json!({"kind":"append", "index":index, "text":text}),
            );
        } else {
            let time = snapshot.blocks.iter().rev().find_map(|block| match block {
                Block::Message { role, time, .. } if role == "user" => time.clone(),
                Block::Boundary { time } => Some(time.clone()),
                _ => None,
            });
            let block = Block::Message {
                role: role.into(),
                text,
                images: vec![],
                time,
            };
            let index = snapshot.blocks.len();
            snapshot.blocks.push(block.clone());
            self.publish(
                snapshot,
                json!({"kind":"block", "index":index, "block":block}),
            );
        }
    }
}

impl EventSink for App {
    fn emit(&self, event: AgentEvent) {
        match event {
            AgentEvent::GenerationStarted { context } if context.depth == 0 => {
                let mut live = self.live.lock().unwrap();
                live.generation_start = Some(live.snapshot.blocks.len());
                drop(live);
                self.status("Running");
            }
            AgentEvent::GenerationFinished { context } if context.depth == 0 => {
                self.live.lock().unwrap().generation_start = None;
            }
            AgentEvent::TextDelta { text, context } if context.depth == 0 => {
                self.delta("assistant", text)
            }
            AgentEvent::ThinkingDelta { text, context } if context.depth == 0 => {
                self.delta("thinking", text)
            }
            AgentEvent::ToolStarted {
                call_id,
                tool_use,
                background,
                context,
            } if context.depth == 0 => {
                let mut live = self.live.lock().unwrap();
                if let Some(heading) = view::assistant_heading(&live.snapshot.blocks) {
                    let index = live.snapshot.blocks.len();
                    live.snapshot.blocks.push(heading.clone());
                    self.publish(
                        &mut live.snapshot,
                        json!({"kind":"block", "index":index, "block":heading}),
                    );
                }
                let index = live.snapshot.blocks.len();
                let can_background = tool_use.name == "bash"
                    && matches!(
                        tool_use.input.get("action").and_then(Value::as_str),
                        None | Some("exec" | "start" | "read" | "write" | "signal")
                    );
                let mut block = Block::tool(tool_use, Some(Instant::now()));
                if let Block::Tool {
                    call_id: id,
                    background_id,
                    ..
                } = &mut block
                {
                    *id = call_id;
                    if can_background {
                        *background_id = Some(call_id);
                        live.background.insert(call_id, background);
                    }
                }
                live.snapshot.blocks.push(block.clone());
                self.publish(
                    &mut live.snapshot,
                    json!({"kind":"block", "index":index, "block":block}),
                );
            }
            AgentEvent::ToolFinished {
                call_id,
                result,
                context,
                ..
            } if context.depth == 0 => {
                let mut live = self.live.lock().unwrap();
                live.background.remove(&call_id);
                let snapshot = &mut live.snapshot;
                if let Some(index) = snapshot.blocks.iter().position(|block| matches!(block, Block::Tool { call_id: id, running: true, .. } if *id == call_id)) {
                    snapshot.blocks[index].finish(&result);
                    snapshot.blocks[index].continue_process();
                    let change = json!({"kind":"block", "index":index, "block":snapshot.blocks[index]});
                    self.publish(snapshot, change);
                }
            }
            AgentEvent::Failure {
                failure,
                retry_in,
                attempt,
                max_attempts,
                context,
            } if context.depth == 0 => {
                let retry = if let Some(delay) = retry_in {
                    self.discard_generation();
                    self.status("Retrying");
                    format!(
                        " — retrying {}/{} in {:.1}s",
                        attempt + 1,
                        max_attempts,
                        delay.as_secs_f64()
                    )
                } else {
                    String::new()
                };
                self.notice(format!("{}{retry}", failure.cause));
            }
            _ => {}
        }
    }
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

struct RunningSession {
    app: Arc<App>,
    session: ActiveSession,
    worker: tokio::task::JoinHandle<()>,
}

struct Listing {
    entries: Vec<SessionListEntry>,
    loaded_at: Instant,
    generation: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct CreateSession {
    request_id: Uuid,
    parent_session: Option<String>,
    #[serde(default)]
    fork: bool,
}

impl CreateSession {
    fn session(&self, model: &str) -> Result<Session> {
        if self.fork && self.parent_session.is_none() {
            return Err(Error::Invalid(
                "A context fork requires parent_session.".into(),
            ));
        }
        let id = self.request_id.as_simple().to_string();
        let mut session = Session::new_with_id(model, &id);
        // A retried creation uses the original durable identity and context.
        if session.json_path().exists() {
            return Session::load(&session.json_path()).map_err(Error::Internal);
        }
        if let Some(parent) = &self.parent_session {
            if parent.trim().is_empty() {
                return Err(Error::Invalid(
                    "parent_session must name a saved session.".into(),
                ));
            }
            let parent = Session::load_by_id_or_prefix(parent).map_err(Error::NotFound)?;
            session = if self.fork {
                parent.fork_child(model)
            } else {
                Session::new_hidden(model, &id, myco::SessionKind::Subagent, Some(parent.id))
            };
            session.id = id;
        }
        Ok(session)
    }
}

pub(super) struct Sessions {
    args: Arc<Args>,
    config: Config,
    preflight: StartupPreflight,
    root_tools: Vec<Arc<dyn myco::ToolService>>,
    running: tokio::sync::Mutex<HashMap<String, RunningSession>>,
    pub(super) events: broadcast::Sender<Arc<Update>>,
    pub(super) shutdown: CancelToken,
    generation: Arc<AtomicU64>,
    listing: tokio::sync::Mutex<[Option<Listing>; 2]>,
}

impl Sessions {
    pub(super) fn new(
        args: Args,
        config: Config,
        preflight: StartupPreflight,
        root_tools: Vec<Arc<dyn myco::ToolService>>,
    ) -> Self {
        Self {
            args: Arc::new(args),
            config,
            preflight,
            root_tools,
            running: tokio::sync::Mutex::new(HashMap::new()),
            events: broadcast::channel(256).0,
            shutdown: CancelToken::new(),
            generation: Arc::new(AtomicU64::new(0)),
            listing: tokio::sync::Mutex::new([None, None]),
        }
    }

    pub(super) async fn create(&self, request: CreateSession) -> Result<String> {
        // The request id is also the session id, so retries cannot create extra sessions.
        let id = request.request_id.as_simple().to_string();
        let mut sessions = self.running.lock().await;
        if sessions.contains_key(&id) {
            return Ok(id);
        }
        let model = self.config.model.clone();
        let session = tokio::task::spawn_blocking(move || request.session(&model))
            .await
            .map_err(|e| Error::Internal(e.to_string()))??;
        self.start_locked(&mut sessions, session).await?;
        Ok(id)
    }

    pub(super) async fn start(&self, session: Session) -> Result<Arc<App>> {
        self.start_locked(&mut *self.running.lock().await, session)
            .await
    }

    pub(super) async fn open(&self, id: &str) -> Result<Arc<App>> {
        let mut sessions = self.running.lock().await;
        if let Some(session) = sessions.get(id) {
            return Ok(session.app.clone());
        }
        let id = id.to_owned();
        let session = tokio::task::spawn_blocking(move || Session::load_by_id_or_prefix(&id))
            .await
            .map_err(|e| Error::Internal(e.to_string()))?
            .map_err(Error::NotFound)?;
        self.start_locked(&mut sessions, session).await
    }

    async fn start_locked(
        &self,
        sessions: &mut HashMap<String, RunningSession>,
        session: Session,
    ) -> Result<Arc<App>> {
        if self.shutdown.is_cancelled() {
            return Err(Error::Unavailable("The server is stopping.".into()));
        }
        if let Some(running) = sessions.get(&session.id) {
            return Ok(running.app.clone());
        }
        let saved_model = if session.json_path().exists() {
            myco::RuntimeRecord::latest(&session.active_thread().messages)
                .map_or_else(|| session.model.clone(), |record| record.model.key)
        } else {
            self.config.model.clone()
        };
        let catalog = self
            .config
            .models
            .get(&saved_model)
            .or_else(|_| self.config.models.get(&self.config.model))
            .map_err(Error::Internal)?
            .clone();
        let model_fallback = (catalog.spec.key != saved_model).then(|| {
            format!(
                "Saved model {saved_model:?} is no longer configured; using {:?}.",
                catalog.spec.key
            )
        });
        let (work, receiver) = mpsc::channel(1);
        let image_limit = catalog.spec.max_image_base64_bytes;
        let context_window_tokens = catalog.spec.context_window_tokens;
        let (mut boot, app) = boot_session(
            &self.args,
            self.config.clone(),
            catalog,
            self.preflight.clone(),
            session,
            self.root_tools.clone(),
            |config, _, session| {
                let session = session.snapshot();
                Arc::new(App {
                    live: Mutex::new(Live {
                        generation_start: None,
                        snapshot: Snapshot {
                            revision: 0,
                            session_id: session.id.clone(),
                            thread_id: session.active_thread().id.clone(),
                            title: session
                                .title
                                .clone()
                                .unwrap_or_else(|| "New session".into()),
                            model: config.model.clone(),
                            models: config
                                .models
                                .keys()
                                .into_iter()
                                .map(str::to_owned)
                                .collect(),
                            attachment_limits: attachments::Limits::new(image_limit),
                            usage: session.active_thread().last_usage,
                            context_window_tokens,
                            busy: false,
                            status: "Ready".into(),
                            tasks: vec![],
                            queued: VecDeque::new(),
                            blocks: view::history(session.active_thread()),
                        },
                        cancel: None,
                        background: HashMap::new(),
                        accepted: HashMap::new(),
                    }),
                    events: self.events.clone(),
                    work,
                    shutdown: CancelToken::new(),
                    generation: self.generation.clone(),
                })
            },
        )
        .await
        .map_err(Error::Conflict)?;
        // New browser sessions need a durable URL before their first message.
        let session = boot.session.snapshot();
        boot.session
            .persist_agent_state(
                &session.active_thread().id,
                boot.runner.agent().state(),
                !session.json_path().exists(),
                None,
            )
            .map_err(Error::Internal)?;
        app.sync(&boot, "Ready");
        if let Some(notice) = model_fallback {
            app.notice(notice);
        }
        if boot.preflight.has_problems() {
            app.notice(boot.preflight.warning_body());
        }
        let observer = app.clone();
        boot.runner.set_observer(Arc::new(move |event| match event {
            WorkflowEvent::Compacting { .. } => observer.compacting(),
            WorkflowEvent::Compacted(_) => {
                observer.finish_compaction();
            }
            WorkflowEvent::Warning(text) => observer.warning(text),
            WorkflowEvent::CompactionProgress { .. } => {}
        }));
        let id = boot.session.id();
        let session = boot.session.clone();
        let worker = tokio::spawn(worker(boot, app.clone(), receiver, self.args.clone()));
        sessions.insert(
            id,
            RunningSession {
                app: app.clone(),
                session,
                worker,
            },
        );
        Ok(app)
    }

    pub(super) async fn snapshots(&self) -> VecDeque<Update> {
        self.running
            .lock()
            .await
            .values()
            .map(|session| session.app.snapshot())
            .collect()
    }

    pub(super) async fn stop(&self) {
        self.shutdown.cancel();
        for session in self.running.lock().await.values() {
            session.app.stop();
        }
    }

    pub(super) async fn join(&self) -> Result<()> {
        let workers = std::mem::take(&mut *self.running.lock().await);
        for session in workers.into_values() {
            session
                .worker
                .await
                .map_err(|e| Error::Internal(format!("browser worker: {e}")))?;
        }
        Ok(())
    }

    pub(super) async fn set_archived(&self, id: String, archived: bool) -> Result<()> {
        let running = self.running.lock().await;
        let active = running.get(&id).map(|s| s.session.clone());
        tokio::task::spawn_blocking(move || {
            if let Some(active) = active {
                active.set_archived(archived).map_err(Error::Internal)
            } else {
                let id = myco::session::resolve_session_id(&id).map_err(Error::NotFound)?;
                let _lock =
                    SessionWriteLock::acquire(&id).map_err(|e| Error::Conflict(e.to_string()))?;
                let session = Session::load_by_id_or_prefix(&id).map_err(Error::NotFound)?;
                ActiveSession::new(session)
                    .set_archived(archived)
                    .map_err(Error::Internal)
            }
        })
        .await
        .map_err(|e| Error::Internal(e.to_string()))??;
        self.generation.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    pub(super) async fn list(&self, archived: bool) -> Result<Value> {
        let entries = {
            let mut listings = self.listing.lock().await;
            let cache = &mut listings[usize::from(archived)];
            let generation = self.generation.load(Ordering::Relaxed);
            // Changes from other processes remain visible on the next polling interval.
            if cache.as_ref().is_none_or(|c| {
                c.generation != generation || c.loaded_at.elapsed() >= Duration::from_secs(5)
            }) {
                let entries = tokio::task::spawn_blocking(move || {
                    let filter = if archived {
                        ArchiveFilter::Archived
                    } else {
                        ArchiveFilter::Active
                    };
                    myco::session::list_sessions_with_filter(0, false, filter)
                })
                .await
                .map_err(|e| Error::Internal(e.to_string()))?
                .map_err(Error::Internal)?;
                *cache = Some(Listing {
                    entries,
                    loaded_at: Instant::now(),
                    generation,
                });
            }
            cache.as_ref().unwrap().entries.clone()
        };
        let running = self.running.lock().await;
        Ok(Value::Array(entries.into_iter().filter(|s| s.archived == archived).map(|s| {
            let live = running.get(&s.id).map(|session| session.app.live.lock().unwrap());
            let status = live.as_ref().map_or("Saved", |live| {
                if !live.snapshot.busy && live.snapshot.blocks.iter().any(|block| matches!(block, Block::Tool { running: true, .. })) { "Running" }
                else { live.snapshot.status.as_str() }
            });
            let title = s.title.unwrap_or_else(|| if s.snippet.is_empty() { "New session".into() } else { s.snippet });
            json!({
                "id": s.id, "title": title,
                "model": live.as_ref().map_or(s.model.as_str(), |live| &live.snapshot.model),
                "updated_at": s.updated_at, "archived": s.archived, "status": status,
                "busy": live.as_ref().is_some_and(|live| live.snapshot.busy),
            })
        }).collect()))
    }
}

async fn worker(
    mut boot: Boot,
    app: Arc<App>,
    mut receiver: mpsc::Receiver<Work>,
    args: Arc<Args>,
) {
    let runtime = boot.runner.runtime().clone();
    let observations = async {
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tick.tick().await;
            app.refresh(runtime.session(), runtime.running_tool_summaries());
            app.resources(runtime.resources().await);
        }
    };
    tokio::pin!(observations);
    loop {
        let work = tokio::select! {
            _ = app.shutdown.cancelled() => break,
            _ = &mut observations => unreachable!("resource observation loop ended"),
            work = receiver.recv() => match work { Some(work) => work, None => break },
        };
        let result = {
            let operation = execute(&mut boot, &app, work, &args);
            tokio::pin!(operation);
            tokio::select! {
                result = &mut operation => result,
                _ = &mut observations => unreachable!("resource observation loop ended"),
            }
        };
        app.sync(&boot, if result.is_ok() { "Ready" } else { "Stopped" });
        if let Err(error) = result {
            app.notice(error);
        }
    }
    if let Err(error) = persist_session(boot.runner.agent(), &boot.session, false) {
        eprintln!("browser: {error}");
    }
}

async fn execute(
    boot: &mut Boot,
    app: &Arc<App>,
    work: Work,
    args: &Args,
) -> std::result::Result<(), String> {
    match work.request.action {
        Action::UpdateQueued { .. } => unreachable!("queue mutations do not create worker actions"),
        Action::Submit { text, images } => match attachments::content(
            &text,
            &images,
            boot.catalog_model.spec.max_image_base64_bytes,
        ) {
            Err(error) => Err(error),
            Ok(content) => {
                let time = work.accepted_at;
                {
                    let mut live = app.live.lock().unwrap();
                    let block = Block::message("user", &content, Some(view::timestamp(&time)));
                    let index = live.snapshot.blocks.len();
                    live.snapshot.blocks.push(block.clone());
                    app.publish(
                        &mut live.snapshot,
                        json!({"kind":"block", "index":index, "block":block}),
                    );
                }
                let followups = app.clone();
                let image_limit = boot.catalog_model.spec.max_image_base64_bytes;
                boot.runner
                    .set_followup_handler(Some(Arc::new(move |agent, session| {
                        followups.deliver_followups(agent, session, image_limit)
                    })));
                let outcome = boot.runner.submit(content, time, work.cancel).await;
                boot.runner.set_followup_handler(None);
                outcome.result.map(|_| ()).map_err(|e| e.to_string())
            }
        },
        Action::Compact => boot
            .runner
            .compact(work.cancel)
            .await
            .map(|_| ())
            .map_err(|e| e.to_string()),
        Action::SelectModel { key } => {
            let catalog = boot.app_config.models.get(&key)?.clone();
            super::super::select_runner_model(
                &mut boot.runner,
                &boot.harness,
                &catalog,
                args.effort,
                args.debug_dump_api_requests,
                boot.app_config.compaction_max_requests,
            )
            .await?;
            boot.catalog_model = catalog;
            Ok(())
        }
    }
}

impl App {
    fn deliver_followups(
        &self,
        agent: &mut myco::agent::Agent,
        session: &ActiveSession,
        image_limit: u64,
    ) -> std::result::Result<bool, myco::agent::AgentInteractionError> {
        let mut delivered = false;
        for _ in 0..MAX_QUEUED_MESSAGES {
            let Some(next) = self.claim_followup() else {
                break;
            };
            let content = match attachments::content(&next.text, &next.images, image_limit) {
                Ok(content) => content,
                Err(error) => {
                    self.notice(format!("Queued message could not be sent: {error}"));
                    self.finish_followup(next.request_id, None);
                    continue;
                }
            };
            if let Err(error) =
                myco::chat::append_followup(agent, session, content.clone(), next.accepted_at)
            {
                self.restore_followup(next.request_id);
                return Err(error);
            }
            let block = Block::message("user", &content, Some(view::timestamp(&next.accepted_at)));
            self.finish_followup(next.request_id, Some(block));
            delivered = true;
        }
        Ok(delivered)
    }

    pub(super) fn accept(&self, mut request: ActionRequest) -> Result<()> {
        if let Action::Submit { images, .. }
        | Action::UpdateQueued {
            update: QueueUpdate::Save { images, .. },
            ..
        } = &mut request.action
        {
            attachments::externalize(images).map_err(Error::Invalid)?;
        }
        let mut live = self.live.lock().unwrap();
        if self.shutdown.is_cancelled() {
            return Err(Error::Unavailable("The server is stopping.".into()));
        }
        if let Some(accepted) = live.accepted.get(&request.request_id) {
            return if accepted == &request {
                Ok(())
            } else {
                Err(Error::Conflict(
                    "Request id already used for another action.".into(),
                ))
            };
        }
        if request.session_id != live.snapshot.session_id {
            return Err(Error::Conflict(
                "The request belongs to a different session.".into(),
            ));
        }
        let queue_update = matches!(request.action, Action::UpdateQueued { .. });
        if live.snapshot.busy
            && !matches!(
                request.action,
                Action::Submit { .. } | Action::UpdateQueued { .. }
            )
        {
            return Err(Error::Conflict(
                "Wait for the current run or cancel it first.".into(),
            ));
        }
        if live.snapshot.status == "Cancelling" && !queue_update {
            return Err(Error::Conflict("Wait for cancellation to finish.".into()));
        }
        let accepted_at = Utc::now();
        if self.work.is_closed() {
            return Err(Error::Unavailable(
                "The session worker is unavailable.".into(),
            ));
        }
        match &request.action {
            Action::Submit { .. } => self.enqueue(&mut live, &request, accepted_at)?,
            Action::UpdateQueued {
                message_id,
                revision,
                update,
            } => self.update_queued(&mut live, *message_id, *revision, update)?,
            _ => self.start_work(&mut live, request.clone(), accepted_at)?,
        }
        live.accepted.insert(request.request_id, request);
        if let Err(error) = self.drain_queue(&mut live) {
            let block = Block::Notice {
                text: error.to_string(),
            };
            let index = live.snapshot.blocks.len();
            live.snapshot.blocks.push(block.clone());
            self.publish(
                &mut live.snapshot,
                json!({"kind":"block", "index":index, "block":block}),
            );
        }
        let change = json!({"kind":"meta", "meta":live.snapshot.metadata()});
        self.publish(&mut live.snapshot, change);
        Ok(())
    }

    fn start_work(
        &self,
        live: &mut Live,
        request: ActionRequest,
        accepted_at: DateTime<Utc>,
    ) -> Result<()> {
        let cancel = CancelToken::new();
        self.work
            .try_send(Work {
                request,
                cancel: cancel.clone(),
                accepted_at,
            })
            .map_err(|_| Error::Unavailable("The session worker is unavailable.".into()))?;
        live.cancel = Some(cancel);
        live.snapshot.busy = true;
        live.snapshot.status = "Running".into();
        Ok(())
    }

    pub(super) fn cancel(&self, session_id: &str) -> Result<()> {
        let mut live = self.live.lock().unwrap();
        if session_id != live.snapshot.session_id {
            return Err(Error::Conflict(
                "The request belongs to a different session.".into(),
            ));
        }
        if let Some(cancel) = &live.cancel {
            cancel.cancel();
            live.snapshot.status = "Cancelling".into();
        }
        let change = json!({"kind":"meta", "meta":live.snapshot.metadata()});
        self.publish(&mut live.snapshot, change);
        Ok(())
    }
}
