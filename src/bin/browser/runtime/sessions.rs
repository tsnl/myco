//! The registry of running session workers and saved-session listings.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chrono::Utc;
use myco::CancelToken;
use myco::session::inbox::{Inbox, State as InboxState};
use myco::session::{ActiveSession, ArchiveFilter, SessionListEntry, SessionWriteLock};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::{broadcast, mpsc};
use uuid::Uuid;

use super::{
    App, Block, Error, Live, Result, Snapshot, Status, UNTITLED, Update, blocking, pending_inboxes,
    service, timers, worker,
};
use crate::{Args, Config, Session, StartupPreflight, WorkflowEvent, boot_session};

pub(super) struct RunningSession {
    pub(super) app: Arc<App>,
    pub(super) session: ActiveSession,
    pub(super) worker: tokio::task::JoinHandle<()>,
}

struct Listing {
    entries: Vec<SessionListEntry>,
    loaded_at: Instant,
    generation: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CreateSession {
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

pub(crate) struct Sessions {
    args: Arc<Args>,
    config: Config,
    preflight: StartupPreflight,
    root_tools: Vec<Arc<dyn myco::ToolService>>,
    pub(super) running: tokio::sync::Mutex<HashMap<String, RunningSession>>,
    pub(crate) events: broadcast::Sender<Arc<Update>>,
    pub(crate) shutdown: CancelToken,
    generation: Arc<AtomicU64>,
    listing: tokio::sync::Mutex<[Option<Listing>; 2]>,
}

impl Sessions {
    pub(crate) fn new(
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

    pub(crate) async fn create(&self, request: CreateSession) -> Result<String> {
        // The request id is also the session id, so retries cannot create extra sessions.
        let id = request.request_id.as_simple().to_string();
        let mut sessions = self.running.lock().await;
        if sessions.contains_key(&id) {
            return Ok(id);
        }
        let model = self.config.model.clone();
        let session = blocking(move || request.session(&model)).await??;
        self.start_locked(&mut sessions, session).await?;
        Ok(id)
    }

    pub(crate) async fn start(&self, session: Session) -> Result<Arc<App>> {
        self.start_locked(&mut *self.running.lock().await, session)
            .await
    }

    pub(crate) async fn open(&self, id: &str) -> Result<Arc<App>> {
        let mut sessions = self.running.lock().await;
        if let Some(session) = sessions.get(id) {
            return Ok(session.app.clone());
        }
        let id = id.to_owned();
        let session = blocking(move || Session::load_by_id_or_prefix(&id))
            .await?
            .map_err(Error::NotFound)?;
        self.start_locked(&mut sessions, session).await
    }

    pub(crate) async fn restore_inboxes(&self) {
        let pending = tokio::task::spawn_blocking(|| {
            myco::core::myco_home().and_then(|home| pending_inboxes(home.join("inbox")))
        })
        .await;
        let ids = match pending {
            Ok(Ok(ids)) => ids,
            Ok(Err(error)) => {
                eprintln!("browser inbox: {error}");
                return;
            }
            Err(error) => {
                eprintln!("browser inbox: {error}");
                return;
            }
        };
        for id in ids {
            if self.shutdown.is_cancelled() {
                break;
            }
            if let Err(error) = self.open(&id).await {
                eprintln!("browser inbox {id}: {error}");
            }
        }
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
            myco::RuntimeRecord::saved_model_key(&session)
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
        let (mut boot, app) = boot_session(
            &self.args,
            self.config.clone(),
            catalog,
            self.preflight.clone(),
            session,
            |app: &Arc<App>| {
                let mut tools = self.root_tools.clone();
                tools.push(app.clone());
                tools
            },
            |config, _, session| {
                let session = session.snapshot();
                let mut inbox = Inbox::open(Inbox::path_for(&session.id)?, &session.id)?;
                inbox.settle(&session, InboxState::Interrupted, Utc::now())?;
                Ok(Arc::new(App {
                    clock: timers::Clock::new(),
                    live: Mutex::new(Live {
                        inbox,
                        generation_start: None,
                        retry_notice: None,
                        // Nothing observes the app before `sync_with` projects the session.
                        snapshot: Snapshot {
                            models: config
                                .models
                                .keys()
                                .into_iter()
                                .map(str::to_owned)
                                .collect(),
                            ..Default::default()
                        },
                        background: HashMap::new(),
                        accepted: HashMap::new(),
                        service: service::Receipts::default(),
                    }),
                    events: self.events.clone(),
                    work,
                    shutdown: CancelToken::new(),
                    generation: self.generation.clone(),
                }))
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
        app.sync_with(&boot, Status::Ready, |_| {});
        app.fire_timers();
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
            WorkflowEvent::Retrying { error, delay } => observer.retrying(error, delay),
            WorkflowEvent::CompactionProgress { .. } => {}
        }));
        let id = boot.session.id();
        let session = boot.session.clone();
        let worker = tokio::spawn(worker::worker(
            boot,
            app.clone(),
            receiver,
            self.args.clone(),
        ));
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

    pub(crate) async fn snapshots(&self) -> VecDeque<Update> {
        self.running
            .lock()
            .await
            .values()
            .map(|session| session.app.snapshot())
            .collect()
    }

    pub(crate) async fn stop(&self) {
        self.shutdown.cancel();
        for session in self.running.lock().await.values() {
            session.app.stop();
        }
    }

    pub(crate) async fn join(&self) -> Result<()> {
        let workers = std::mem::take(&mut *self.running.lock().await);
        for session in workers.into_values() {
            session
                .worker
                .await
                .map_err(|e| Error::Internal(format!("browser worker: {e}")))?;
        }
        Ok(())
    }

    /// An open session's live projection and the saved document behind it.
    async fn opened(&self, id: &str) -> Result<(Arc<App>, ActiveSession)> {
        let app = self.open(id).await?;
        let id = app.live.lock().unwrap().snapshot.meta.session_id.clone();
        let running = self.running.lock().await;
        let session = running
            .get(&id)
            .ok_or_else(|| Error::NotFound("Session closed".into()))?;
        Ok((app, session.session.clone()))
    }

    /// The saved document behind a session, for reads outside its live projection.
    pub(crate) async fn session(&self, id: &str) -> Result<ActiveSession> {
        Ok(self.opened(id).await?.1)
    }

    pub(crate) async fn set_auto_continue(&self, id: String, enabled: bool) -> Result<()> {
        let (app, session) = self.opened(&id).await?;
        let saved = session.clone();
        blocking(move || saved.set_auto_continue(enabled))
            .await?
            .map_err(Error::Internal)?;
        app.refresh_metadata(&session);
        Ok(())
    }

    /// Change what is saved about a session, whether or not it is open.
    async fn update_saved(
        &self,
        id: String,
        change: impl FnOnce(&ActiveSession) -> std::result::Result<(), String> + Send + 'static,
    ) -> Result<()> {
        // Hold the registry gate so opening a saved session cannot race its save.
        let running = self.running.lock().await;
        let entry = running.get(&id);
        let active = entry.map(|s| s.session.clone());
        blocking(move || {
            let (_lock, active) = if let Some(active) = active {
                (None, active)
            } else {
                let id = myco::session::resolve_session_id(&id).map_err(Error::NotFound)?;
                let lock =
                    SessionWriteLock::acquire(&id).map_err(|e| Error::Conflict(e.to_string()))?;
                let session = Session::load_by_id_or_prefix(&id).map_err(Error::NotFound)?;
                (Some(lock), ActiveSession::new(session))
            };
            change(&active).map_err(Error::Internal)
        })
        .await??;
        self.generation.fetch_add(1, Ordering::Relaxed);
        if let Some(entry) = entry {
            entry.app.refresh_metadata(&entry.session);
        }
        Ok(())
    }

    pub(crate) async fn rename(&self, id: String, title: String) -> Result<()> {
        let title = myco::session::normalize_title(&title).map_err(Error::Invalid)?;
        self.update_saved(id, move |session| session.rename(title))
            .await
    }

    pub(crate) async fn set_archived(&self, id: String, archived: bool) -> Result<()> {
        self.update_saved(id, move |session| session.set_archived(archived))
            .await
    }

    pub(crate) async fn list(&self, archived: bool) -> Result<Value> {
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
                    myco::session::list_sessions_with_filter(false, filter)
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
            // Listings add labels for idle work and unopened sessions.
            let status = live.as_ref().map_or(json!("Saved"), |live| {
                let snapshot = &live.snapshot;
                if !snapshot.meta.busy() && snapshot.blocks.iter().any(|block| matches!(block, Block::Tool { running: true, .. })) { json!(Status::Running) }
                else if !snapshot.meta.busy() && snapshot.meta.inbox.iter().any(|entry| entry.state.active()) { json!("Waiting") }
                else { json!(snapshot.meta.status) }
            });
            let title = s.title.unwrap_or_else(|| if s.snippet.is_empty() { UNTITLED.into() } else { s.snippet });
            json!({
                "id": s.id, "title": title,
                "model": live.as_ref().map_or(s.model.as_str(), |live| &live.snapshot.meta.model),
                "updated_at": s.updated_at, "archived": s.archived, "status": status,
                "busy": live.as_ref().is_some_and(|live| live.snapshot.meta.busy()),
            })
        }).collect()))
    }
}
