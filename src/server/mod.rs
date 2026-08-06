//! The session runtime: [`Server`] is the object that does the work — live
//! sessions with queued commands, per-session event feeds, compaction, the
//! `subagent` tool. It implements [`myco_api::MycoApi`], the same async
//! interface `client::HttpClient` speaks over HTTP: frontends hold either
//! and cannot tell the difference. The Rocket adapter (`web`) and the CLI
//! (`cli`) are both thin layers over this.

//! The Rocket API server: v1 agent semantics behind `/api`, one live agent
//! task per session, concurrent across sessions.
//!
//! Each live session owns a per-session [`Harness`] and an [`Agent`] driven
//! by a dedicated tokio task. Messages posted over the API are accepted into
//! the session's [`Room`] inbox (the client never blocks on a running turn):
//! while a turn is in flight the agent folds the inbox at every well-formed
//! boundary — a message that names the agent pre-empts the turn and is
//! answered by it, one between people rides along as context — and whatever
//! the turn does not fold, the task drains afterwards as its own turns and
//! notes. Transcript reads go straight to the persisted session, so clients
//! see exactly what a resume would see; mid-turn checkpoints keep that fresh
//! at every completed tool round. Live output streams over SSE
//! (`/sessions/<id>/events`): the agent's [`EventSink`] feeds a per-session
//! broadcast channel, so streaming is a projection of the existing event
//! seam, not a second pipeline.
//!
//! Sharing one harness (one ssh connection per machine) across sessions is a
//! planned follow-up; today each live session attaches its own, exactly like
//! one v1 process per conversation.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tokio::sync::{Mutex, broadcast, mpsc, watch};

use crate::agent::{Agent, AgentEvent, CompactWorkerError, EventSink, run_compact_worker};
use crate::auth::AuthStore;
use crate::config::Config;
use crate::machines::harness::Harness;
use crate::machines::tool_services::{PreludeTool, SessionMetaTool, ToolService};
use crate::models::{
    BackendConfig, CatalogModel, Effort, GenerativeModel, GenerativeModelConfig, Recovery,
};
use crate::session::{
    ActiveSession, CompactOutcome, Session, SessionWriteLock, expand_image_attachments,
};
use myco_api::Content;
use myco_api::{Author, Entry, EntryBody};

use crate::core::CancelToken;
use crate::subagent::SubagentTool;
use myco_api as api;

mod observer;
mod room;
mod user_api;

use room::Room;
pub use user_api::UserApi;

const SYSTEM_PROMPT_PROLOGUE: &str = r#"
You are a helpful assistant running in an agentic harness with unfettered computer access.
"#;

/// One event on a session's live feed: the agent's own events verbatim, plus
/// task-lifecycle markers. Frontends project this — the web adapter
/// downsamples to `myco_api::StreamEvent`, the CLI renders it as terminal
/// output.
#[derive(Debug, Clone)]
pub enum SessionEvent {
    Agent(AgentEvent),
    /// A message was accepted into the session. Broadcast at post time, ahead
    /// of the queue, so watchers see people talking to each other immediately
    /// — including while the agent is mid-turn.
    Message {
        entry: Entry,
        /// Whether this message is what wakes the agent.
        wakes_agent: bool,
    },
    TurnStarted,
    TurnFailed {
        message: String,
    },
    TurnFinished,
    /// Compaction replaced this session with a successor — follow
    /// `outcome.successor_id` (the wire projection keeps only the id pair;
    /// the CLI renders the full COMPACTED banner from the outcome).
    Compacted {
        outcome: CompactOutcome,
    },
}

/// Per-session event fan-out capacity; slow receivers lag and skip, they never
/// block the agent.
const EVENT_BUFFER: usize = 1024;

// ---------------------------------------------------------------------------
// Server
// ---------------------------------------------------------------------------

/// What a [`ModelFactory`] is building a model for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelPurpose {
    /// The session's own agent.
    Agent,
    /// A compaction worker summarizing the named session.
    Compactor,
}

/// The seam between the server and model construction: tests inject scripted
/// models here; production uses [`Server::new`]'s real factory.
pub type ModelFactory = Box<
    dyn Fn(ModelPurpose, &str, &CatalogModel, &Harness) -> Result<Arc<dyn GenerativeModel>, String>
        + Send
        + Sync,
>;

/// Owns the live-session table. Shared by the HTTP routes and the `subagent`
/// tool (which spawns children through it).
pub struct Server {
    config: Config,
    /// Credentials and live access tokens. Seeded from the roster at
    /// construction, mutated only through the admin interface.
    auth: Arc<AuthStore>,
    live: Mutex<HashMap<String, Arc<Live>>>,
    model_factory: ModelFactory,
    /// Self-reference for agent tasks and per-session tools (set at
    /// construction; the server is only ever held as an `Arc`).
    me: std::sync::Weak<Server>,
}

impl Server {
    pub fn new(config: Config) -> Arc<Self> {
        Self::with_model_factory(config, Box::new(default_model_factory))
    }

    /// Credentials and sessions.
    pub fn auth(&self) -> &Arc<AuthStore> {
        &self.auth
    }

    /// Test/embedder constructor: models come from `factory` instead of the
    /// provider backends.
    pub fn with_model_factory(config: Config, factory: ModelFactory) -> Arc<Self> {
        let opened =
            AuthStore::default_path().and_then(|p| AuthStore::open(p).map_err(|e| e.to_string()));
        let auth = match opened {
            Ok(store) => store,
            Err(e) => {
                // A store we cannot read must not silently become an empty
                // one that anybody could then be added to.
                eprintln!("myco: cannot open the credential store: {e}");
                std::process::exit(1);
            }
        };
        Self::with_model_factory_and_auth(config, factory, Arc::new(auth))
    }

    /// Construct with an explicit credential store (tests use an in-memory
    /// one so they never touch `$MYCO_HOME/v2/auth.json`).
    pub fn with_model_factory_and_auth(
        config: Config,
        factory: ModelFactory,
        auth: Arc<AuthStore>,
    ) -> Arc<Self> {
        // The roster declares who exists; the store holds what they know.
        // Reconciling here means adding a name to `server.toml` is enough to
        // make `myco auth passwd <id>` work, with no second registration step.
        for user in config.roster.users() {
            if auth.get(&user.id).is_none() {
                let _ = auth.add_user(&user.id, user.display_name());
            }
        }
        Arc::new_cyclic(|me| Self {
            config,
            auth,
            live: Mutex::new(HashMap::new()),
            model_factory: factory,
            me: me.clone(),
        })
    }
}

fn default_model_factory(
    purpose: ModelPurpose,
    _session_id: &str,
    catalog_model: &CatalogModel,
    harness: &Harness,
) -> Result<Arc<dyn GenerativeModel>, String> {
    match purpose {
        ModelPurpose::Agent => build_model(catalog_model, harness),
        ModelPurpose::Compactor => crate::models::new(GenerativeModelConfig {
            model: catalog_model.spec.clone(),
            tools: harness.tool_specs(),
            system_prompt: crate::agent::compactor_system_prompt(catalog_model),
            backend_config: catalog_model.backend.clone(),
        })
        .map_err(|e| format!("failed to create compactor model: {e}")),
    }
}

/// One queued unit of work for a session's agent task.
pub enum Cmd {
    /// A message addressed to the agent: record it and answer it as its own
    /// turn. The direct path for in-process callers (CLI, `subagent`) — web
    /// messages go through the room inbox instead, so they can pre-empt a
    /// running turn.
    User { entry: Entry },
    /// A message was accepted into the room inbox; drain whatever the running
    /// turn has not already folded. Carries nothing: the inbox is the record.
    Poke,
    /// Compact into a successor session.
    Compact {
        /// Queued by the auto-compact trigger rather than a person. An
        /// automatic compaction that fails disables the trigger for this
        /// session (the latch on [`AgentTask`]) — retrying it after every
        /// turn would fail the same way, loudly, forever. A manual compact
        /// still works and re-arms the trigger on success.
        automatic: bool,
    },
    /// The session document's model key or effort override changed
    /// (`PATCH /sessions`); rebuild the agent's model to match. Carries
    /// nothing: the session document is the source of truth. Queued like any
    /// command, so a running turn finishes on the model it started with and
    /// the change applies from the next turn onward.
    Reconfigure,
}

/// One resident conversation: its agent task's input queue and shared handles.
pub struct Live {
    pub session: ActiveSession,
    pub tx: mpsc::UnboundedSender<Cmd>,
    /// Who has posted, and what is accepted but not yet folded (see [`Room`]).
    room: Arc<std::sync::Mutex<Room>>,
    busy: Arc<AtomicBool>,
    cancel: Mutex<CancelToken>,
    /// Completed-turn counter; `subagent` (and turn-awaiting CLIs) wait on it.
    pub turns: watch::Receiver<u64>,
    /// SSE feed (see [`BroadcastSink`]).
    events: broadcast::Sender<SessionEvent>,
    /// Why the most recent turn produced nothing; cleared at turn start.
    last_error: std::sync::Mutex<Option<String>>,
    /// Held for the lifetime of the live session; swapped on compaction;
    /// `None` when flock is unavailable on this filesystem.
    lock: std::sync::Mutex<Option<SessionWriteLock>>,
    /// The agent's harness, shared for status displays.
    harness: Arc<Harness>,
    /// The agent task's id — what its host state (bash sessions) is owned
    /// under, and what a terminal opened *for* it must be owned by too.
    agent_id: uuid::Uuid,
    /// The subagent keyboard — meaningful on child sessions only. True while
    /// a person holds this child, which makes the parent's `subagent` calls
    /// to it fail politely (see [`crate::subagent::SubagentTool`]). In-memory
    /// like a shell's lock: the hold ends with the agent task.
    user_hold: AtomicBool,
    /// What has been typed into each user-held shell (keyed `host/shell`)
    /// since its keyboard was taken. Keystrokes stream raw and would spam
    /// the transcript one note apiece, so they accumulate here and flush as
    /// a single attributed note when the keyboard is handed back — the
    /// boundary where the agent's writes resume and it needs the story.
    typed: std::sync::Mutex<HashMap<String, String>>,
}

impl Live {
    pub async fn cancel_turn(&self) {
        self.cancel.lock().await.cancel();
    }

    /// Subscribe to this session's live feed.
    pub fn subscribe(&self) -> broadcast::Receiver<SessionEvent> {
        self.events.subscribe()
    }

    pub fn is_busy(&self) -> bool {
        self.busy.load(Ordering::Relaxed)
    }

    /// The harness this session's agent drives (host status, running tools).
    pub fn harness(&self) -> Arc<Harness> {
        self.harness.clone()
    }

    /// Does a person hold this (child) session's keyboard?
    pub fn user_holds(&self) -> bool {
        self.user_hold.load(Ordering::Relaxed)
    }

    /// Point the keyboard; returns whether a person held it before.
    fn set_user_hold(&self, held: bool) -> bool {
        self.user_hold.swap(held, Ordering::Relaxed)
    }

    fn set_error(&self, msg: Option<String>) {
        *self.last_error.lock().unwrap_or_else(|e| e.into_inner()) = msg;
    }

    pub fn error(&self) -> Option<String> {
        self.last_error
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

impl Server {
    /// The live handle for `id`, booting the agent task if needed.
    /// `fresh` supplies a brand-new session to boot instead of loading.
    pub async fn ensure_live(&self, id: &str, fresh: Option<Session>) -> Result<Arc<Live>, String> {
        let mut live = self.live.lock().await;
        if let Some(l) = live.get(id) {
            return Ok(l.clone());
        }

        let session = match fresh {
            Some(s) => s,
            None => Session::load_by_id_or_prefix(id)?,
        };
        let id = session.id.clone();

        let lock = match SessionWriteLock::acquire(&id) {
            Ok(lock) => Some(lock),
            Err(crate::session::SessionLockError::Busy { path }) => {
                return Err(format!(
                    "session {id} is open in another myco process (lock: {})",
                    path.display()
                ));
            }
            Err(e @ crate::session::SessionLockError::Unavailable(_)) => {
                eprintln!("warning: {e}; continuing without a single-writer guard");
                None
            }
        };

        let model_key = session.model.clone();
        let catalog_model =
            catalog_model_with_effort(&self.config, &model_key, session.effort.as_deref())?;

        // A session becomes durable when it has content, not when it is
        // opened: the first turn's checkpoint writes the file. Opening one
        // and walking away therefore leaves nothing on disk.
        let active = ActiveSession::new(session);

        let session_tool = Arc::new(SessionMetaTool::new(active.clone())) as Arc<dyn ToolService>;
        let subagent_tool =
            Arc::new(SubagentTool::new(self.me.clone(), id.clone())) as Arc<dyn ToolService>;
        let prelude_tool =
            Arc::new(PreludeTool::new(self.config.max_prelude_bytes)) as Arc<dyn ToolService>;
        let harness = Harness::attach_with_root_services(
            self.config.harness.clone(),
            vec![session_tool, subagent_tool, prelude_tool],
        )
        .await?;

        let (events, _) = broadcast::channel::<SessionEvent>(EVENT_BUFFER);
        let sink = Arc::new(BroadcastSink { tx: events.clone() }) as Arc<dyn EventSink>;

        let model = (self.model_factory)(ModelPurpose::Agent, &id, &catalog_model, &harness)?;
        let mut agent = Agent::new(model, harness.clone(), sink);
        agent.set_context_window_tokens(catalog_model.spec.context_window_tokens);
        agent.set_max_truncated_resumes(catalog_model.spec.max_truncated_resumes);
        agent.set_model_key(catalog_model.spec.key.clone());
        let restored = active.snapshot();
        let room = Arc::new(std::sync::Mutex::new(Room::seeded(&restored.entries)));
        agent.set_history(restored.entries.clone());
        agent.set_last_usage(restored.last_usage);
        wire_checkpoint(&mut agent, &active);
        // Messages accepted while a turn runs pre-empt it: the agent drains
        // the room inbox at every well-formed boundary, so a direct message
        // is answered by the turn in flight and a message between people
        // lands in history where the room actually saw it.
        {
            let room = room.clone();
            agent.set_pending_input(Box::new(move || {
                let mut room = room.lock().unwrap_or_else(|e| e.into_inner());
                room.inbox.drain(..).map(|a| a.entry).collect()
            }));
        }

        let (tx, rx) = mpsc::unbounded_channel::<Cmd>();
        let (turn_tx, turns) = watch::channel(0u64);
        let busy = Arc::new(AtomicBool::new(false));
        let handle = Arc::new(Live {
            session: active.clone(),
            tx,
            room,
            busy: busy.clone(),
            cancel: Mutex::new(CancelToken::new()),
            turns,
            events: events.clone(),
            last_error: std::sync::Mutex::new(None),
            lock: std::sync::Mutex::new(lock),
            harness: harness.clone(),
            agent_id: agent.agent_id(),
            user_hold: AtomicBool::new(false),
            typed: std::sync::Mutex::new(HashMap::new()),
        });
        live.insert(id.clone(), handle.clone());

        tokio::spawn(run_agent_task(AgentTask {
            supervisor: self.me.clone(),
            agent,
            active,
            rx,
            busy,
            handle: handle.clone(),
            turn_tx,
            events,
            catalog_model: catalog_model.clone(),
            harness,
            auto_compact_failed: false,
        }));
        Ok(handle)
    }

    /// Build (but do not boot) a hidden child session of `parent`.
    pub fn new_child(
        &self,
        parent: &str,
        model: Option<String>,
        fork: bool,
    ) -> Result<Session, String> {
        let model_key = model.unwrap_or_else(|| self.config.model.clone());
        self.config
            .models
            .get(&model_key)
            .map_err(|e| format!("model {model_key:?}: {e}"))?;
        if fork {
            let parent_session = Session::load_by_id_or_prefix(parent)?;
            Ok(parent_session.fork_child(model_key))
        } else {
            let mut fresh = Session::new(model_key);
            fresh.kind = crate::session::SessionKind::Subagent;
            fresh.parent_session_id = Some(parent.to_string());
            Ok(fresh)
        }
    }

    pub async fn get_live(&self, id: &str) -> Option<Arc<Live>> {
        self.live.lock().await.get(id).cloned()
    }

    /// Retire a live session's agent task (session stays on disk).
    pub async fn retire_live(&self, id: &str) -> Option<Arc<Live>> {
        let removed = self.live.lock().await.remove(id);
        if let Some(l) = &removed {
            l.cancel_turn().await;
        }
        removed
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    /// Build the entry for a message, expanding `@path` image mentions under
    /// the limits of the model that will read them.
    ///
    /// Composed once, by whoever accepts the message, so the record shown live
    /// and the record written to disk are the same one — clients match them by
    /// timestamp rather than guessing.
    pub fn compose(&self, author: Author, text: &str, model_key: &str) -> Entry {
        let max_image_bytes = self
            .config
            .models
            .get(model_key)
            .map(|m| m.spec.max_image_base64_bytes)
            .unwrap_or(0);
        let content = expand_image_attachments(text, max_image_bytes).unwrap_or_else(|_| {
            vec![Content::Text {
                text: text.to_string(),
            }]
        });
        Entry::user(author, content)
    }

    pub async fn live_flags(&self, id: &str) -> (bool, bool) {
        match self.live.lock().await.get(id) {
            Some(l) => (true, l.busy.load(Ordering::Relaxed)),
            None => (false, false),
        }
    }
}

/// [`EventSink`] → per-session broadcast. Sending to zero receivers is fine
/// (nobody watching); slow receivers lag and skip.
struct BroadcastSink {
    tx: broadcast::Sender<SessionEvent>,
}

impl EventSink for BroadcastSink {
    fn emit(&self, event: AgentEvent) {
        let _ = self.tx.send(SessionEvent::Agent(event));
    }
}

/// Everything a session's agent task owns.
struct AgentTask {
    supervisor: std::sync::Weak<Server>,
    agent: Agent,
    active: ActiveSession,
    rx: mpsc::UnboundedReceiver<Cmd>,
    busy: Arc<AtomicBool>,
    handle: Arc<Live>,
    turn_tx: watch::Sender<u64>,
    events: broadcast::Sender<SessionEvent>,
    catalog_model: CatalogModel,
    harness: Arc<Harness>,
    /// Latched when an *automatic* compaction fails, so the trigger does not
    /// queue the same failure after every turn. A successful compact (manual
    /// or automatic) re-arms it; the session swap makes the next one a fresh
    /// question.
    auto_compact_failed: bool,
}

/// What one turn runs: a user message or a compaction.
enum TurnWork {
    User(Entry),
    Compact { automatic: bool },
}

/// The per-session agent task: pop queued commands, run one turn each.
async fn run_agent_task(mut t: AgentTask) {
    while let Some(cmd) = t.rx.recv().await {
        match cmd {
            // A poke follows every room acceptance. Anything a running turn
            // already folded is gone from the inbox by now; this drains the
            // rest — messages that arrived after the turn's last boundary,
            // or while the session was idle.
            Cmd::Poke => drain_inbox(&mut t).await,
            Cmd::User { entry } => run_turn(&mut t, TurnWork::User(entry)).await,
            Cmd::Compact { automatic } => run_turn(&mut t, TurnWork::Compact { automatic }).await,
            Cmd::Reconfigure => reconfigure(&mut t),
        }
    }
}

/// Rebuild the agent's model from the session document's current model key
/// and effort override — the runtime half of a `PATCH /sessions` model or
/// effort edit. Not a turn: no busy flag, no turn counter, no turn events.
/// Failure (a catalog key removed since validation, a backend that will not
/// build) leaves the old model driving and surfaces like a failed turn would.
fn reconfigure(t: &mut AgentTask) {
    let Some(sup) = t.supervisor.upgrade() else {
        return;
    };
    let (model_key, effort) = t.active.with(|s| (s.model.clone(), s.effort.clone()));
    let rebuilt = catalog_model_with_effort(sup.config(), &model_key, effort.as_deref()).and_then(
        |catalog_model| {
            let model = (sup.model_factory)(
                ModelPurpose::Agent,
                &t.active.id(),
                &catalog_model,
                &t.harness,
            )?;
            Ok((catalog_model, model))
        },
    );
    match rebuilt {
        Ok((catalog_model, model)) => {
            t.agent.set_model(model);
            t.agent.set_model_key(catalog_model.spec.key.clone());
            t.agent
                .set_context_window_tokens(catalog_model.spec.context_window_tokens);
            t.agent
                .set_max_truncated_resumes(catalog_model.spec.max_truncated_resumes);
            t.catalog_model = catalog_model;
            // A model switch changes the auto-compact threshold; give the
            // trigger a fresh chance rather than carrying the old latch.
            t.auto_compact_failed = false;
        }
        Err(e) => {
            let msg = format!("reconfigure: {e} (still on {:?})", t.catalog_model.spec.key);
            eprintln!("[{}] {msg}", t.active.id());
            t.handle.set_error(Some(msg.clone()));
            let _ = t.events.send(SessionEvent::TurnFailed { message: msg });
        }
    }
}

/// Fold the room inbox in acceptance order: a message that wakes the agent
/// runs as its own turn, one between people is recorded without costing one —
/// no busy flag, no turn counter, no turn events, which a client would read
/// as "the agent is answering me".
async fn drain_inbox(t: &mut AgentTask) {
    loop {
        let next = {
            let mut room = t.handle.room.lock().unwrap_or_else(|e| e.into_inner());
            room.inbox.pop_front()
        };
        let Some(accepted) = next else { break };
        if accepted.wakes_agent {
            run_turn(t, TurnWork::User(accepted.entry)).await;
        } else {
            run_note(t, accepted.entry);
        }
    }
}

/// One turn, with its lifecycle bookkeeping: busy flag, turn events, a fresh
/// cancel token, the completed-turn counter. `TurnFinished` goes out last —
/// after the turn's entries are persisted — so it is the wire's one signal
/// that a refetch will read the finished turn.
async fn run_turn(t: &mut AgentTask, work: TurnWork) {
    t.busy.store(true, Ordering::Relaxed);
    t.handle.set_error(None);
    let _ = t.events.send(SessionEvent::TurnStarted);

    // Fresh token per turn so one cancel doesn't poison the next turn.
    let cancel = CancelToken::new();
    *t.handle.cancel.lock().await = cancel.clone();

    match work {
        TurnWork::User(entry) => run_user_turn(t, entry, cancel).await,
        TurnWork::Compact { automatic } => run_compact(t, automatic, cancel).await,
    }

    t.busy.store(false, Ordering::Relaxed);
    t.turn_tx.send_modify(|n| *n += 1);
    let _ = t.events.send(SessionEvent::TurnFinished);
}

/// Fold a person-to-person message into the history and persist, so a reload
/// and a live view agree and the agent has the context next time it is
/// addressed.
fn run_note(t: &mut AgentTask, entry: Entry) {
    let _ = t
        .active
        .maybe_auto_title_from_user_text(&entry_text(&entry));
    t.agent.note(entry);
    if let Err(e) = t
        .active
        .persist_entries(t.agent.history(), t.agent.last_usage(), true)
    {
        eprintln!("[{}] session save failed: {e}", t.active.id());
    }
}

/// The prose of an entry, for titling and logging.
pub(crate) fn entry_text(entry: &Entry) -> String {
    match &entry.body {
        EntryBody::User { content } | EntryBody::Agent { content, .. } => {
            api::content_text(content)
        }
        EntryBody::ToolResults { .. } => String::new(),
    }
}

async fn run_user_turn(t: &mut AgentTask, entry: Entry, cancel: CancelToken) {
    let text = entry_text(&entry);
    let _ = t.active.maybe_auto_title_from_user_text(&text);
    let author = entry.author.clone();
    let mut content = match entry.body {
        EntryBody::User { content } => content,
        _ => vec![Content::Text { text: text.clone() }],
    };
    let at = entry.at;

    // One retry for provider blips; other failures surface immediately.
    let mut retried = false;
    let error = loop {
        let entry = Entry {
            author: author.clone(),
            at,
            body: EntryBody::User {
                content: content.clone(),
            },
        };
        match t.agent.interact_entry(entry, cancel.clone()).await {
            Ok(_) => break None,
            Err(crate::agent::AgentInteractionError::Cancelled) => {
                break Some("(turn cancelled)".to_string());
            }
            Err(e) => match e.recovery() {
                Recovery::Retry if !retried && !cancel.is_cancelled() => {
                    // Take the user turn back out so the retry resubmits it.
                    if let Some(dropped) = t.agent.rewind_last_user_turn() {
                        content = dropped;
                    }
                    retried = true;
                    continue;
                }
                Recovery::OmitLastMessage => {
                    // Too-large request: every later turn resends it, so drop
                    // it rather than leave the session unable to continue.
                    t.agent.rewind_last_user_turn();
                    break Some(format!(
                        "{e}

The last message was removed from the conversation so the                          session can continue."
                    ));
                }
                _ => break Some(e.to_string()),
            },
        }
    };

    if let Some(msg) = error {
        eprintln!("[{}] agent turn error: {msg}", t.active.id());
        t.handle.set_error(Some(msg.clone()));
        let _ = t.events.send(SessionEvent::TurnFailed { message: msg });
    }
    if let Err(e) = t
        .active
        .persist_entries(t.agent.history(), t.agent.last_usage(), true)
    {
        eprintln!("[{}] session save failed: {e}", t.active.id());
    }

    // Auto-compact: queue it when the measured prompt crossed this model's
    // resolved threshold, so the next thing this task does is shrink the
    // session. `last_usage` is the provider's own count for the request just
    // sent — a measured size, not a guess at the next one.
    if let Some(usage) = t.agent.last_usage()
        && !t.auto_compact_failed
        && usage.context_tokens() >= t.catalog_model.spec.auto_compact_at_tokens
    {
        let _ = t.handle.tx.send(Cmd::Compact { automatic: true });
    }
}

/// `Cmd::Compact`: run the worker, then swap this task's session, history,
/// lock, and live-table entry over to the successor (the v1 `/compact`
/// lifecycle, server-side).
async fn run_compact(t: &mut AgentTask, automatic: bool, cancel: CancelToken) {
    let fail = |t: &mut AgentTask, msg: String| {
        let mut msg = format!("compact: {msg}");
        if automatic {
            t.auto_compact_failed = true;
            msg.push_str(
                "\nauto-compact is disabled for this session after this failure; \
                 compact manually to retry",
            );
        }
        eprintln!("[{}] {msg}", t.active.id());
        t.handle.set_error(Some(msg.clone()));
        let _ = t.events.send(SessionEvent::TurnFailed { message: msg });
    };

    let Some(sup) = t.supervisor.upgrade() else {
        return;
    };
    if let Err(e) = t
        .active
        .persist_entries(t.agent.history(), t.agent.last_usage(), true)
    {
        fail(t, format!("failed to persist current session: {e}"));
        return;
    }
    let predecessor = t.active.snapshot();
    if predecessor.entries.is_empty() {
        fail(t, "session is empty".into());
        return;
    }

    let model = match (sup.model_factory)(
        ModelPurpose::Compactor,
        &predecessor.id,
        &t.catalog_model,
        &t.harness,
    ) {
        Ok(m) => m,
        Err(e) => {
            fail(t, e);
            return;
        }
    };
    let (successor, outcome) = match run_compact_worker(
        &predecessor,
        &t.catalog_model,
        t.harness.clone(),
        model,
        cancel,
    )
    .await
    {
        Ok(v) => v,
        Err(CompactWorkerError::Cancelled) => {
            fail(t, "cancelled (session unchanged)".into());
            return;
        }
        Err(CompactWorkerError::Failed(reason)) => {
            fail(t, reason);
            return;
        }
    };

    // Relock under the successor's id, then switch this task over.
    let new_lock = match SessionWriteLock::acquire(&successor.id) {
        Ok(lock) => Some(lock),
        Err(crate::session::SessionLockError::Busy { path }) => {
            fail(
                t,
                format!("successor is locked elsewhere ({})", path.display()),
            );
            return;
        }
        Err(crate::session::SessionLockError::Unavailable(_)) => None,
    };
    t.active.replace(successor.clone());
    t.agent.set_history(successor.entries.clone());
    t.agent.set_last_usage(successor.last_usage);
    *t.handle.lock.lock().unwrap_or_else(|e| e.into_inner()) = new_lock;
    // Reseed who has posted from the successor's tail; accepted-but-unfolded
    // messages stay queued and fold into the successor.
    {
        let mut room = t.handle.room.lock().unwrap_or_else(|e| e.into_inner());
        room.participants = Room::participants_of(&successor.entries);
    }

    // Re-key the live table: the conversation now answers to the new id.
    {
        let mut live = sup.live.lock().await;
        if let Some(entry) = live.remove(&outcome.predecessor_id) {
            live.insert(outcome.successor_id.clone(), entry);
        }
    }
    t.auto_compact_failed = false;
    let _ = t.events.send(SessionEvent::Compacted { outcome });
}

/// Persist agent history at replayable mid-turn boundaries (after the user
/// message, after each completed tool round).
fn wire_checkpoint(agent: &mut Agent, active_session: &ActiveSession) {
    let checkpoint_session = active_session.clone();
    agent.set_checkpoint(Box::new(move |messages, last_usage| {
        if let Err(e) = checkpoint_session.persist_entries(messages, last_usage, false) {
            eprintln!("warning: mid-turn session save failed: {e}");
        }
    }));
}

fn build_model(
    catalog_model: &CatalogModel,
    harness: &Harness,
) -> Result<Arc<dyn GenerativeModel>, String> {
    crate::models::new(GenerativeModelConfig {
        model: catalog_model.spec.clone(),
        tools: harness.tool_specs(),
        system_prompt: [
            SYSTEM_PROMPT_PROLOGUE.to_string(),
            crate::prompts::agent_prompt_epilogue(),
            crate::prompts::model_stamp(&catalog_model.spec.key),
        ]
        .join("\n"),
        backend_config: catalog_model.backend.clone(),
    })
    .map_err(|e| format!("failed to create model: {e}"))
}

/// Resolve `model_key` from the catalog with the session's effort override
/// laid over the backend — what model factories actually build from.
///
/// `effort` is the stored wire string. An unparseable value (a hand-edited or
/// future-version session file) falls back to the configured effort rather
/// than refusing to open the session.
fn catalog_model_with_effort(
    config: &Config,
    model_key: &str,
    effort: Option<&str>,
) -> Result<CatalogModel, String> {
    let mut m = config
        .models
        .get(model_key)
        .map_err(|e| format!("model {model_key:?}: {e}"))?
        .clone();
    if let Some(s) = effort {
        match s.parse::<Effort>() {
            Ok(effort) => match &mut m.backend {
                BackendConfig::Anthropic(c) => c.effort = Some(effort),
                BackendConfig::OpenAIResponses(c) | BackendConfig::OpenAICompletions(c) => {
                    c.effort = Some(effort)
                }
            },
            Err(e) => eprintln!("warning: ignoring session effort override: {e}"),
        }
    }
    Ok(m)
}

/// The author for turns this process drives locally.
///
/// Read off the roster resolved at startup, never derived here: `Config` has
/// already refused to build if `server.toml` was missing or did not name us,
/// which is what keeps an unregistered identity out of a stored session.
pub fn local_author(config: &Config) -> Author {
    config.roster.local_author()
}

/// The final agent prose of the last turn, for `subagent` results.
pub fn last_answer(entries: &[Entry]) -> Option<String> {
    match &entries.last()?.body {
        EntryBody::Agent { content, .. } => {
            let text: Vec<String> = content
                .iter()
                .filter_map(|c| match c {
                    Content::Text { text } => Some(text.clone()),
                    _ => None,
                })
                .collect();
            (!text.is_empty()).then(|| text.join("\n"))
        }
        _ => None,
    }
}
