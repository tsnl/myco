//! Session workers own runners, writer locks, and live tools independently of browser tabs.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use myco::generative_model::{TokenUsage, ToolUse};
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

const MAX_QUEUED_MESSAGES: usize = 20;

#[derive(Clone, Serialize)]
struct QueuedMessage {
    request_id: Uuid,
    text: String,
    images: Vec<String>,
    accepted_at: DateTime<Utc>,
}

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
    snapshot: Snapshot,
    cancel: Option<CancelToken>,
    accepted: HashMap<Uuid, ActionRequest>,
}

#[derive(Clone, Serialize)]
pub(super) struct Update {
    session_id: String,
    revision: u64,
    change: Value,
}

pub(super) struct App {
    live: Mutex<Live>,
    events: broadcast::Sender<Arc<Update>>,
    work: mpsc::Sender<Work>,
    shutdown: CancelToken,
    generation: Arc<AtomicU64>,
}

impl App {
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

    fn system_message(&self, text: &str) {
        self.append_block(Block::Message {
            role: "system".into(),
            text: text.into(),
            images: vec![],
            time: Some(view::timestamp(&Utc::now())),
        });
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

    fn sync(&self, boot: &Boot, status: &str) {
        let session = boot.session.snapshot();
        let tasks = boot.runner.runtime().running_tool_summaries();
        let mut live = self.live.lock().unwrap();
        let snapshot = &mut live.snapshot;
        let mut blocks = view::history(session.active_thread());
        if snapshot.thread_id == session.active_thread().id {
            view::retain_tool_timers(&mut blocks, &snapshot.blocks);
        }
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

    fn delta(&self, role: &str, text: String) {
        let mut live = self.live.lock().unwrap();
        let snapshot = &mut live.snapshot;
        if let Some(Block::Message {
            role: last_role,
            text: last_text,
            ..
        }) = snapshot.blocks.last_mut()
            && last_role == role
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
            AgentEvent::TextDelta { text, context } if context.depth == 0 => {
                self.delta("assistant", text)
            }
            AgentEvent::ThinkingDelta { text, context } if context.depth == 0 => {
                self.delta("thinking", text)
            }
            AgentEvent::ToolStarted { tool_use, context } if context.depth == 0 => {
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
                let block = Block::tool(tool_use, Some(Instant::now()));
                live.snapshot.blocks.push(block.clone());
                self.publish(
                    &mut live.snapshot,
                    json!({"kind":"block", "index":index, "block":block}),
                );
            }
            AgentEvent::ToolFinished {
                tool_use,
                result,
                context,
            } if context.depth == 0 => {
                let mut live = self.live.lock().unwrap();
                let snapshot = &mut live.snapshot;
                if let Some(index) = snapshot.blocks.iter().position(|block| matches!(block, Block::Tool { tool, running: true, .. } if same_tool(tool, &tool_use))) {
                    snapshot.blocks[index].finish(&result);
                    let change = json!({"kind":"block", "index":index, "block":snapshot.blocks[index]});
                    self.publish(snapshot, change);
                }
            }
            AgentEvent::Failure {
                failure,
                retry_in,
                context,
                ..
            } if context.depth == 0 => self.notice(format!(
                "{}{}",
                failure.cause,
                if retry_in.is_some() {
                    " — retrying"
                } else {
                    ""
                }
            )),
            _ => {}
        }
    }
}

fn same_tool(a: &ToolUse, b: &ToolUse) -> bool {
    a.name == b.name && a.input == b.input
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
    running: tokio::sync::Mutex<HashMap<String, RunningSession>>,
    pub(super) events: broadcast::Sender<Arc<Update>>,
    pub(super) shutdown: CancelToken,
    generation: Arc<AtomicU64>,
    listing: tokio::sync::Mutex<[Option<Listing>; 2]>,
}

impl Sessions {
    pub(super) fn new(args: Args, config: Config, preflight: StartupPreflight) -> Self {
        Self {
            args: Arc::new(args),
            config,
            preflight,
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
        let catalog = self
            .config
            .models
            .get(&self.config.model)
            .map_err(Error::Internal)?
            .clone();
        let (work, receiver) = mpsc::channel(1);
        let image_limit = catalog.spec.max_image_base64_bytes;
        let context_window_tokens = catalog.spec.context_window_tokens;
        let (mut boot, app) = boot_session(
            &self.args,
            self.config.clone(),
            catalog,
            self.preflight.clone(),
            session,
            |config, _, session| {
                let session = session.snapshot();
                Arc::new(App {
                    live: Mutex::new(Live {
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
        if boot.preflight.has_problems() {
            app.notice(boot.preflight.warning_body());
        }
        let observer = app.clone();
        boot.runner.set_observer(Arc::new(move |event| match event {
            WorkflowEvent::Compacting { automatic, .. } => observer.system_message(if automatic {
                "Compacting automatically…"
            } else {
                "Compacting…"
            }),
            WorkflowEvent::Compacted(_) => observer.system_message("Compaction complete."),
            WorkflowEvent::Warning(text) => observer.notice(text),
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
                if !live.snapshot.busy && !live.snapshot.tasks.is_empty() { "Background tasks" }
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
    let mut tick = tokio::time::interval(std::time::Duration::from_secs(1));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        let work = tokio::select! {
            _ = app.shutdown.cancelled() => break,
            _ = tick.tick() => {
                app.refresh(&boot.session, boot.runner.runtime().running_tool_summaries());
                continue;
            }
            work = receiver.recv() => match work { Some(work) => work, None => break },
        };
        let result = {
            let runtime = boot.runner.runtime().clone();
            let operation = execute(&mut boot, &app, work, &args);
            tokio::pin!(operation);
            loop {
                tokio::select! {
                    result = &mut operation => break result,
                    _ = tick.tick() => app.refresh(runtime.session(), runtime.running_tool_summaries()),
                }
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
        let queued = self.live.lock().unwrap().snapshot.queued.clone();
        let mut delivered = false;
        for next in queued {
            if self.shutdown.is_cancelled()
                || self.live.lock().unwrap().snapshot.status == "Cancelling"
            {
                break;
            }
            let content = match attachments::content(&next.text, &next.images, image_limit) {
                Ok(content) => content,
                Err(error) => {
                    self.notice(format!("Queued message could not be sent: {error}"));
                    self.finish_followup(next.request_id, None);
                    continue;
                }
            };
            myco::chat::append_followup(agent, session, content.clone(), next.accepted_at)?;
            let block = Block::message("user", &content, Some(view::timestamp(&next.accepted_at)));
            self.finish_followup(next.request_id, Some(block));
            delivered = true;
        }
        Ok(delivered)
    }

    fn finish_followup(&self, request_id: Uuid, block: Option<Block>) {
        let mut live = self.live.lock().unwrap();
        let snapshot = &mut live.snapshot;
        if snapshot
            .queued
            .front()
            .is_some_and(|next| next.request_id == request_id)
        {
            snapshot.queued.pop_front();
        }
        if let Some(block) = block {
            let index = snapshot.blocks.len();
            snapshot.blocks.push(block.clone());
            self.publish(
                snapshot,
                json!({"kind":"block", "index":index, "block":block}),
            );
        }
        self.publish(snapshot, json!({"kind":"meta", "meta":snapshot.metadata()}));
    }

    pub(super) fn accept(&self, mut request: ActionRequest) -> Result<()> {
        if let Action::Submit { images, .. } = &mut request.action {
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
        if live.snapshot.busy && !matches!(request.action, Action::Submit { .. }) {
            return Err(Error::Conflict(
                "Wait for the current run or cancel it first.".into(),
            ));
        }
        if let Action::Submit { text, images } = &request.action {
            if text.trim().is_empty() && images.is_empty() {
                return Err(Error::Invalid("Enter a message or attach an image.".into()));
            }
            if !images.is_empty() {
                attachments::content(
                    text,
                    images,
                    live.snapshot.attachment_limits.max_image_base64_bytes,
                )
                .map_err(Error::Invalid)?;
            }
        }
        if live.snapshot.status == "Cancelling" {
            return Err(Error::Conflict("Wait for cancellation to finish.".into()));
        }
        let accepted_at = Utc::now();
        if live.snapshot.busy {
            if self.work.is_closed() {
                return Err(Error::Unavailable(
                    "The session worker is unavailable.".into(),
                ));
            }
            if live.snapshot.queued.len() >= MAX_QUEUED_MESSAGES {
                return Err(Error::Conflict(
                    "The message queue is full (20 messages).".into(),
                ));
            }
            let Action::Submit { text, images } = &request.action else {
                unreachable!()
            };
            live.snapshot.queued.push_back(QueuedMessage {
                request_id: request.request_id,
                text: text.clone(),
                images: images.clone(),
                accepted_at,
            });
        } else {
            self.start_work(&mut live, request.clone(), accepted_at)?;
        }
        live.accepted.insert(request.request_id, request);
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

    fn start_next(&self, live: &mut Live) -> Result<()> {
        live.cancel = None;
        live.snapshot.busy = false;
        if self.shutdown.is_cancelled() {
            return Ok(());
        }
        if let Some(next) = live.snapshot.queued.pop_front() {
            let request = live.accepted[&next.request_id].clone();
            if let Err(error) = self.start_work(live, request, next.accepted_at) {
                live.snapshot.queued.push_front(next);
                return Err(error);
            }
        }
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
