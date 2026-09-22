//! Session workers own runners, writer locks, and live tools independently of browser tabs.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use myco::generative_model::ToolUse;
use myco::session::{ActiveSession, ArchiveFilter, SessionListEntry, SessionWriteLock};
use myco::{AgentEvent, CancelToken, EventSink};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::{broadcast, mpsc};
use uuid::Uuid;

use super::super::{
    Args, Boot, Config, Session, StartupPreflight, WorkflowEvent, boot_session, persist_session,
};
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
    busy: bool,
    status: String,
    tasks: Vec<String>,
    queued: VecDeque<QueuedMessage>,
    blocks: Vec<Block>,
}

impl Snapshot {
    fn metadata(&self) -> Value {
        json!({"session_id":self.session_id, "thread_id":self.thread_id, "title":self.title, "model":self.model, "busy":self.busy, "status":self.status, "queued":self.queued})
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
        let mut live = self.live.lock().unwrap();
        let block = Block::Notice { text: text.into() };
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
        snapshot.session_id = session.id.clone();
        snapshot.thread_id = session.active_thread().id.clone();
        snapshot.title = session
            .title
            .clone()
            .unwrap_or_else(|| "New session".into());
        snapshot.model = boot.catalog_model.spec.key.clone();
        snapshot.blocks = view::history(session.active_thread());
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
        let title = session.with(|session| {
            session
                .title
                .clone()
                .unwrap_or_else(|| "New session".into())
        });
        let mut live = self.live.lock().unwrap();
        if live.snapshot.title != title {
            live.snapshot.title = title;
            let change = json!({"kind":"meta", "meta":live.snapshot.metadata()});
            self.publish(&mut live.snapshot, change);
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
                let index = live.snapshot.blocks.len();
                let block = Block::tool(tool_use);
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
    Submit { text: String },
    Compact,
    SelectModel { key: String },
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

    pub(super) async fn create(&self, id: Uuid) -> Result<String> {
        // The request id is also the session id, so retries cannot create extra sessions.
        let session = Session::new_with_id(&self.config.model, id.as_simple().to_string());
        let id = session.id.clone();
        self.start(session).await?;
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
            WorkflowEvent::Compacting { automatic, .. } => observer.notice(if automatic {
                "Compacting automatically…"
            } else {
                "Compacting…"
            }),
            WorkflowEvent::Compacted(_) => observer.notice("Compaction complete."),
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
                if live.snapshot.busy { "Running" }
                else if !live.snapshot.tasks.is_empty() { "Background tasks" }
                else { "Ready" }
            });
            let title = s.title.unwrap_or_else(|| if s.snippet.is_empty() { "New session".into() } else { s.snippet });
            json!({
                "id": s.id, "title": title,
                "model": live.as_ref().map_or(s.model.as_str(), |live| &live.snapshot.model),
                "updated_at": s.updated_at, "archived": s.archived, "status": status,
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
    app: &App,
    work: Work,
    args: &Args,
) -> std::result::Result<(), String> {
    match work.request.action {
        Action::Submit { text } => match super::super::expand_image_attachments(
            &text,
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
                let outcome = boot.runner.submit(content, time, work.cancel).await;
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
    pub(super) fn accept(&self, request: ActionRequest) -> Result<()> {
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
        if matches!(&request.action, Action::Submit { text } if text.trim().is_empty()) {
            return Err(Error::Invalid("Enter a message.".into()));
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
            let Action::Submit { text } = &request.action else {
                unreachable!()
            };
            live.snapshot.queued.push_back(QueuedMessage {
                request_id: request.request_id,
                text: text.clone(),
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
        live.snapshot.queued.clear();
        let change = json!({"kind":"meta", "meta":live.snapshot.metadata()});
        self.publish(&mut live.snapshot, change);
        Ok(())
    }
}
