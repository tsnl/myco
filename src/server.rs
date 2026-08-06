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

/// A message the room has accepted but the history has not absorbed yet.
struct Accepted {
    entry: Entry,
    wakes_agent: bool,
}

/// Post-acceptance state for one live session: who has posted here, and what
/// has been accepted but not yet folded into history. One lock covers the
/// wake decision, the feed broadcast, and the inbox push, so acceptance order
/// is feed order is fold order — and the room rule is never computed from a
/// stale snapshot while messages sit queued behind a running turn.
struct Room {
    /// User ids that have posted. Agent and system entries do not make a room
    /// — only other people do.
    participants: std::collections::HashSet<String>,
    /// Accepted messages the agent task (or the running turn's boundary
    /// drain) has yet to fold, in acceptance order.
    inbox: std::collections::VecDeque<Accepted>,
}

impl Room {
    fn seeded(entries: &[Entry]) -> Self {
        Self {
            participants: Self::participants_of(entries),
            inbox: std::collections::VecDeque::new(),
        }
    }

    fn participants_of(entries: &[Entry]) -> std::collections::HashSet<String> {
        entries
            .iter()
            .filter(|e| matches!(&e.body, EntryBody::User { .. }))
            .filter_map(|e| match &e.author {
                Author::User { id, .. } => Some(id.clone()),
                _ => None,
            })
            .collect()
    }

    /// Should this message wake the agent?
    ///
    /// The rule is explicit address, with one carve-out: a session nobody else
    /// has posted in is a private line, and everything said there is said to
    /// the agent. The moment a second person joins, the room becomes a room —
    /// the agent answers when it is called (`@myco`, `@agent`, `@assistant`,
    /// or the model key it is running as) and otherwise listens.
    ///
    /// Deliberately not a judgement call about intent. In a room, an agent
    /// that guesses wrong talks over people; one that waits to be named never
    /// does.
    fn wakes_agent(&self, text: &str, model: &str, author: &Author) -> bool {
        if api::mention::addresses_agent(text, model) {
            return true;
        }
        !self.is_shared(author)
    }

    /// Has anyone other than `author` posted here?
    fn is_shared(&self, author: &Author) -> bool {
        match author {
            Author::User { id, .. } => self.participants.iter().any(|p| p != id),
            // A non-human poster (subagent plumbing) never turns a session
            // shared, and is never gated by one.
            _ => false,
        }
    }
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

// ---------------------------------------------------------------------------
// MycoApi: the server object speaks the same interface as the HTTP client
// ---------------------------------------------------------------------------

impl From<SessionEvent> for api::StreamEvent {
    fn from(ev: SessionEvent) -> Self {
        match ev {
            SessionEvent::Agent(a) => match a {
                AgentEvent::TextDelta { text, .. } => api::StreamEvent::TextDelta { text },
                AgentEvent::ThinkingDelta { text, .. } => api::StreamEvent::ThinkingDelta { text },
                AgentEvent::ToolStarted { tool_use, .. } => api::StreamEvent::ToolStarted {
                    id: tool_use.id,
                    name: tool_use.name,
                    input: tool_use.input,
                },
                AgentEvent::ToolFinished { result, .. } => {
                    api::StreamEvent::ToolFinished { result }
                }
                // Not sent: `events()` filters this variant so the wire
                // carries exactly one TurnFinished per turn — the task-level
                // one, emitted after the turn's entries are persisted.
                AgentEvent::TurnFinished { .. } => api::StreamEvent::TurnFinished,
            },
            SessionEvent::Message { entry, wakes_agent } => {
                api::StreamEvent::Message { entry, wakes_agent }
            }
            SessionEvent::TurnStarted => api::StreamEvent::TurnStarted,
            SessionEvent::TurnFailed { message } => api::StreamEvent::TurnFailed { message },
            SessionEvent::TurnFinished => api::StreamEvent::TurnFinished,
            SessionEvent::Compacted { outcome } => api::StreamEvent::Compacted {
                predecessor: outcome.predecessor_id,
                successor: outcome.successor_id,
            },
        }
    }
}

/// Sessions shown in the browser list (most recent first).
const SESSION_LIST_LIMIT: usize = 200;

use myco_api::{ApiError, ErrorKind, MycoApi};

fn internal(e: String) -> ApiError {
    ApiError::new(ErrorKind::Internal, e)
}

fn lock_of(lock: crate::machines::tool_services::ShellLock) -> api::ShellLockMode {
    match lock {
        crate::machines::tool_services::ShellLock::Assistant => api::ShellLockMode::Assistant,
        crate::machines::tool_services::ShellLock::User => api::ShellLockMode::User,
    }
}

fn shell_of(host: String, s: crate::machines::tool_services::ShellOverview) -> api::Shell {
    api::Shell {
        host,
        id: s.id,
        title: s.title,
        cmdline: s.cmdline,
        running: s.running,
        exit_code: s.exit_code,
        lock: lock_of(s.lock),
        end_offset: s.end_offset,
        pty: s.pty,
    }
}

/// Keystrokes as a person (or the agent) reads them back: printable text
/// untouched, Enter as a newline, everything else in caret notation (`^C`,
/// `^[[A`) — a transcript note is for reading, not replaying.
fn readable_keys(data: &str) -> String {
    let mut out = String::with_capacity(data.len());
    for c in data.chars() {
        match c {
            '\n' | '\r' => out.push('\n'),
            '\t' => out.push('\t'),
            '\x7f' => out.push_str("^?"),
            c if (c as u32) < 0x20 => {
                out.push('^');
                out.push(char::from((c as u8) + 0x40));
            }
            c => out.push(c),
        }
    }
    out
}

/// A live child as the rail lists it. Metadata reads only — no snapshot.
fn subagent_of(l: &Live) -> api::Subagent {
    let (id, model) = l.session.with(|s| (s.id.clone(), s.model.clone()));
    api::Subagent {
        id,
        model,
        busy: l.is_busy(),
        lock: if l.user_holds() {
            api::ShellLockMode::User
        } else {
            api::ShellLockMode::Assistant
        },
    }
}

fn summary_of(config: &Config, s: &Session, live: bool, busy: bool) -> api::SessionSummary {
    api::SessionSummary {
        archived: s.archived,
        id: s.id.clone(),
        title: s.title.clone(),
        model: s.model.clone(),
        created_at: s.created_at.to_rfc3339(),
        updated_at: s.updated_at.to_rfc3339(),
        message_count: s.entries.len(),
        snippet: String::new(),
        effort: s.effort.clone(),
        context_tokens: s.last_usage.map(|u| u.context_tokens()),
        context_window: config
            .models
            .get(&s.model)
            .ok()
            .map(|m| m.spec.context_window_tokens),
        live,
        busy,
    }
}

impl Server {
    /// The named session's current on-disk truth: live snapshot when
    /// resident, plain load otherwise.
    async fn load_or_snapshot(&self, id: &str) -> Result<Session, ApiError> {
        match self.get_live(id).await {
            Some(l) => Ok(l.session.snapshot()),
            None => {
                Session::load_by_id_or_prefix(id).map_err(|e| ApiError::new(ErrorKind::NotFound, e))
            }
        }
    }
}

/// The API surface, minus identity. [`UserApi`] binds a caller to these and
/// is what implements [`MycoApi`] — so no route can reach the runtime without
/// naming who is asking.
impl Server {
    pub(crate) async fn list_sessions(
        &self,
        include_archived: bool,
    ) -> Result<Vec<api::SessionSummary>, ApiError> {
        let entries =
            crate::session::list_sessions_with(SESSION_LIST_LIMIT, false, include_archived)
                .map_err(internal)?;
        let mut out = Vec::with_capacity(entries.len());
        for e in entries {
            let (live, busy) = self.live_flags(&e.id).await;
            out.push(api::SessionSummary {
                id: e.id,
                title: e.title,
                model: e.model,
                created_at: e.created_at.to_rfc3339(),
                updated_at: e.updated_at.to_rfc3339(),
                message_count: e.message_count,
                snippet: e.snippet,
                archived: e.archived,
                effort: e.effort,
                // The browser list shows no context meter; the conversation
                // view gets these from its own detail fetch.
                context_tokens: None,
                context_window: None,
                live,
                busy,
            });
        }
        Ok(out)
    }

    pub(crate) async fn create_session(
        &self,
        req: api::CreateSession,
    ) -> Result<api::SessionSummary, ApiError> {
        let session = match req.parent_session.as_deref().map(str::trim) {
            None | Some("") => {
                let model_key = req
                    .model
                    .clone()
                    .unwrap_or_else(|| self.config().model.clone());
                self.config().models.get(&model_key).map_err(|e| {
                    ApiError::new(ErrorKind::BadRequest, format!("model {model_key:?}: {e}"))
                })?;
                Session::new(model_key)
            }
            Some(parent) => self
                .new_child(parent, req.model.clone(), req.fork)
                .map_err(|e| ApiError::new(ErrorKind::BadRequest, e))?,
        };
        let id = session.id.clone();
        let live = self
            .ensure_live(&id, Some(session))
            .await
            .map_err(internal)?;
        Ok(summary_of(
            &self.config,
            &live.session.snapshot(),
            true,
            false,
        ))
    }

    pub(crate) async fn session_detail(&self, id: &str) -> Result<api::SessionDetail, ApiError> {
        let session = self.load_or_snapshot(id).await?;
        let (live, busy) = self.live_flags(&session.id).await;
        Ok(api::SessionDetail {
            entries: session.entries.clone(),
            summary: summary_of(&self.config, &session, live, busy),
        })
    }

    pub(crate) async fn update_session(
        &self,
        id: &str,
        req: api::UpdateSession,
    ) -> Result<api::SessionSummary, ApiError> {
        // Validate up front so a bad key or effort is a 400 with the session
        // untouched — the agent task's rebuild should only ever see values
        // that resolved here.
        if let Some(model) = req.model.as_deref() {
            self.config.models.get(model).map_err(|e| {
                ApiError::new(ErrorKind::BadRequest, format!("model {model:?}: {e}"))
            })?;
        }
        // `""` clears the override; anything else must parse. Stored
        // canonically (`"MED"` → `"medium"`) so the document and the summary
        // read the same as the wire.
        let effort = match req.effort.as_deref() {
            None => None,
            Some("") => Some(None),
            Some(s) => Some(Some(
                s.parse::<Effort>()
                    .map_err(|e| ApiError::new(ErrorKind::BadRequest, e))?
                    .as_str()
                    .to_string(),
            )),
        };
        let apply = |s: &mut Session| {
            if let Some(title) = req.title.clone() {
                s.title = Some(title);
            }
            if let Some(archived) = req.archived {
                s.archived = archived;
            }
            if let Some(model) = req.model.clone() {
                s.model = model;
            }
            if let Some(effort) = effort.clone() {
                s.effort = effort;
            }
            s.touch();
            s.save()
        };
        let reconfigures = req.model.is_some() || effort.is_some();

        // Edit the live copy when there is one, so a viewer sees it at once;
        // otherwise edit on disk.
        let session = match self.get_live(id).await {
            Some(l) => {
                l.session.with_mut(apply).map_err(internal)?;
                if reconfigures {
                    // The document changed; tell the agent task to catch up.
                    // Queued behind whatever is running, so the change lands
                    // at the next turn boundary.
                    let _ = l.tx.send(Cmd::Reconfigure);
                }
                l.session.snapshot()
            }
            None => {
                let mut s = Session::load_by_id_or_prefix(id)
                    .map_err(|e| ApiError::new(ErrorKind::NotFound, e))?;
                apply(&mut s).map_err(internal)?;
                s
            }
        };
        let (live, busy) = self.live_flags(&session.id).await;
        Ok(summary_of(&self.config, &session, live, busy))
    }

    pub(crate) async fn post_message(
        &self,
        author: &Author,
        id: &str,
        req: api::PostMessage,
    ) -> Result<api::Poll, ApiError> {
        if req.text.trim().is_empty() {
            return Err(ApiError::new(ErrorKind::BadRequest, "empty message"));
        }
        let live = self
            .ensure_live(id, None)
            .await
            .map_err(|e| ApiError::new(ErrorKind::Conflict, e))?;
        let snapshot = live.session.snapshot();
        let entry = self.compose(author.clone(), &req.text, &snapshot.model);

        // Acceptance is atomic under the room lock: the wake decision reads
        // the room as it is *now* — participants include everyone whose
        // message is still queued behind a running turn, not just what has
        // reached disk — and the feed broadcast and inbox push happen in the
        // same order for every message. The broadcast goes out ahead of any
        // folding, so a message shows up for everyone the instant it is
        // accepted, even mid-turn.
        let wakes_agent = {
            let mut room = live.room.lock().unwrap_or_else(|e| e.into_inner());
            let wakes_agent = room.wakes_agent(&req.text, &snapshot.model, author);
            if let Author::User { id, .. } = author {
                room.participants.insert(id.clone());
            }
            let _ = live.events.send(SessionEvent::Message {
                entry: entry.clone(),
                wakes_agent,
            });
            room.inbox.push_back(Accepted { entry, wakes_agent });
            live.tx
                .send(Cmd::Poke)
                .map_err(|_| internal("agent task gone".into()))?;
            wakes_agent
        };
        Ok(api::Poll {
            // Whether a reply is coming *or already underway* — a room note
            // posted mid-turn must not tell the client the agent went idle.
            busy: wakes_agent || live.is_busy(),
            context_tokens: snapshot.last_usage.map(|u| u.context_tokens()),
            total: snapshot.entries.len(),
            entries: Vec::new(),
            last_error: None,
        })
    }

    /// Record a never-waking message through the room — shell keystrokes and
    /// keyboard-lock transitions. Same acceptance path as `post_message`
    /// (broadcast, inbox, poke, participants), so watchers see it at once and
    /// the agent reads it at its next boundary; forced non-waking because the
    /// user acted on the shell, not on the agent.
    fn accept_room_note(&self, live: &Arc<Live>, author: &Author, text: &str) {
        let entry = Entry::user(
            author.clone(),
            vec![Content::Text {
                text: text.to_string(),
            }],
        );
        let mut room = live.room.lock().unwrap_or_else(|e| e.into_inner());
        if let Author::User { id, .. } = author {
            room.participants.insert(id.clone());
        }
        let _ = live.events.send(SessionEvent::Message {
            entry: entry.clone(),
            wakes_agent: false,
        });
        room.inbox.push_back(Accepted {
            entry,
            wakes_agent: false,
        });
        let _ = live.tx.send(Cmd::Poke);
    }

    /// The live handle, or NotFound: the shell and subagent surfaces have
    /// nothing to say about a session whose agent task (and with it every
    /// bash session) is gone.
    async fn live_for_shells(&self, id: &str) -> Result<Arc<Live>, ApiError> {
        self.get_live(id)
            .await
            .ok_or_else(|| ApiError::new(ErrorKind::NotFound, "session not live"))
    }

    pub(crate) async fn shells(&self, id: &str) -> Result<api::Shells, ApiError> {
        let shells = match self.get_live(id).await {
            Some(live) => live
                .harness()
                .shell_overviews()
                .await
                .into_iter()
                .map(|(host, s)| shell_of(host, s))
                .collect(),
            // Not resident is a fact, not a fault: the rail shows nothing.
            None => Vec::new(),
        };
        Ok(api::Shells { shells })
    }

    /// The controller for `host` on a live session's harness — every one-shell
    /// observer call resolves through this.
    async fn shell_host(
        &self,
        id: &str,
        host: &str,
    ) -> Result<(Arc<Live>, Arc<crate::machines::harness::HostController>), ApiError> {
        let live = self.live_for_shells(id).await?;
        let ctl = live
            .harness()
            .observer_host(host)
            .map_err(|e| ApiError::new(ErrorKind::NotFound, e))?;
        Ok((live, ctl))
    }

    pub(crate) async fn shell_tail(
        &self,
        id: &str,
        host: &str,
        shell: &str,
        from: u64,
    ) -> Result<api::ShellTailChunk, ApiError> {
        /// One tail response's byte budget; a viewer further behind is
        /// skipped forward (the terminal wants the present).
        const TAIL_BUDGET: usize = 64 * 1024;
        let (_, ctl) = self.shell_host(id, host).await?;
        let tail = ctl
            .shell_tail(shell, from, TAIL_BUDGET)
            .await
            .map_err(|e| ApiError::new(ErrorKind::NotFound, e))?;
        Ok(api::ShellTailChunk {
            from: tail.from,
            end: tail.end,
            data: String::from_utf8_lossy(&tail.data).into_owned(),
            running: tail.running,
            lock: lock_of(tail.lock),
        })
    }

    pub(crate) async fn shell_screen(
        &self,
        id: &str,
        host: &str,
        shell: &str,
    ) -> Result<api::ShellScreen, ApiError> {
        let (_, ctl) = self.shell_host(id, host).await?;
        let s = ctl
            .shell_screen(shell)
            .await
            .map_err(|e| ApiError::new(ErrorKind::NotFound, e))?;
        Ok(api::ShellScreen {
            cols: s.cols,
            rows: s.rows,
            cursor_row: s.cursor_row,
            cursor_col: s.cursor_col,
            text: s.text,
            runs: s
                .runs
                .into_iter()
                .map(|r| api::ScreenRun {
                    row: r.row,
                    text: r.text,
                    fg: r.fg,
                    bg: r.bg,
                    bold: r.bold,
                    italic: r.italic,
                    underline: r.underline,
                    inverse: r.inverse,
                    cursor: r.cursor,
                })
                .collect(),
            cursor_hidden: s.cursor_hidden,
            application_cursor: s.application_cursor,
        })
    }

    /// Wait for the shell's output to move past `seen` (bounded by
    /// `timeout`); the shell WebSocket's pusher blocks here. Not on
    /// [`MycoApi`]: the trait is the request/response surface, and this is
    /// the socket's private half.
    pub(crate) async fn shell_wait_change(
        &self,
        id: &str,
        host: &str,
        shell: &str,
        seen: u64,
        timeout: std::time::Duration,
    ) -> Result<u64, ApiError> {
        let (_, ctl) = self.shell_host(id, host).await?;
        ctl.shell_wait_change(shell, seen, timeout)
            .await
            .map_err(|e| ApiError::new(ErrorKind::NotFound, e))
    }

    pub(crate) async fn shell_resize(
        &self,
        id: &str,
        host: &str,
        shell: &str,
        cols: u16,
        rows: u16,
    ) -> Result<api::Shell, ApiError> {
        let (_, ctl) = self.shell_host(id, host).await?;
        let overview = ctl
            .shell_resize(shell, cols, rows)
            .await
            .map_err(|e| ApiError::new(ErrorKind::Conflict, e))?;
        Ok(shell_of(host.to_string(), overview))
    }

    /// Open a terminal for the user on `host`: a real bash session owned by
    /// this session's agent — its bash tool can write/read/signal/close it —
    /// starting user-held, because whoever opened it is the one typing.
    pub(crate) async fn shell_start(
        &self,
        author: &Author,
        id: &str,
        host: &str,
        req: api::CreateShell,
    ) -> Result<api::Shell, ApiError> {
        let (live, ctl) = self.shell_host(id, host).await?;
        let overview = ctl
            .shell_start(
                req.shell.as_deref(),
                live.agent_id,
                req.command.as_deref(),
                req.pty,
                req.cols,
                req.rows,
            )
            .await
            .map_err(|e| ApiError::new(ErrorKind::Conflict, e))?;
        // Announced like a keyboard grab: the agent can address this session
        // by name from here on, and must not wonder where it came from.
        self.accept_room_note(
            &live,
            author,
            &format!("[opened shell {:?} on {host} (user-held)]", overview.id),
        );
        Ok(shell_of(host.to_string(), overview))
    }

    /// Set or clear a shell's display name. The id stays the address; the
    /// note keeps the agent's mental map current.
    pub(crate) async fn shell_rename(
        &self,
        author: &Author,
        id: &str,
        host: &str,
        shell: &str,
        title: Option<String>,
    ) -> Result<api::Shell, ApiError> {
        let (live, ctl) = self.shell_host(id, host).await?;
        let overview = ctl
            .shell_rename(shell, title.as_deref())
            .await
            .map_err(|e| ApiError::new(ErrorKind::NotFound, e))?;
        let text = match &overview.title {
            Some(t) => format!("[named shell {shell:?} on {host} {t:?}]"),
            None => format!("[cleared shell {shell:?}'s name on {host}]"),
        };
        self.accept_room_note(&live, author, &text);
        Ok(shell_of(host.to_string(), overview))
    }

    pub(crate) async fn shell_input(
        &self,
        _author: &Author,
        id: &str,
        host: &str,
        shell: &str,
        data: String,
    ) -> Result<api::Shell, ApiError> {
        let (live, ctl) = self.shell_host(id, host).await?;
        let overview = ctl
            .shell_input(shell, &data)
            .await
            .map_err(|e| ApiError::new(ErrorKind::Conflict, e))?;
        // The keystrokes become part of the conversation — the agent must not
        // discover a mutated shell with no explanation in its history — but
        // they stream one at a time now, so they accumulate per hold and
        // flush as a single note when the keyboard is handed back.
        live.typed
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(format!("{host}/{shell}"))
            .or_default()
            .push_str(&data);
        Ok(shell_of(host.to_string(), overview))
    }

    pub(crate) async fn shell_lock(
        &self,
        author: &Author,
        id: &str,
        host: &str,
        shell: &str,
        lock: api::ShellLockMode,
    ) -> Result<api::Shell, ApiError> {
        let (live, ctl) = self.shell_host(id, host).await?;
        let wanted = match lock {
            api::ShellLockMode::Assistant => crate::machines::tool_services::ShellLock::Assistant,
            api::ShellLockMode::User => crate::machines::tool_services::ShellLock::User,
        };
        let (previous, overview) = ctl
            .shell_lock(shell, wanted)
            .await
            .map_err(|e| ApiError::new(ErrorKind::NotFound, e))?;
        // Announce transitions only: a double-click that re-takes the
        // keyboard is not news.
        if previous != wanted {
            match lock {
                api::ShellLockMode::User => {
                    // A fresh hold starts a fresh story.
                    live.typed
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .remove(&format!("{host}/{shell}"));
                    self.accept_room_note(
                        &live,
                        author,
                        &format!("[took the keyboard for shell {shell:?} on {host}]"),
                    );
                }
                api::ShellLockMode::Assistant => {
                    let typed = live
                        .typed
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .remove(&format!("{host}/{shell}"));
                    if let Some(data) = typed.filter(|d| !d.is_empty()) {
                        self.accept_room_note(
                            &live,
                            author,
                            &format!(
                                "[typed into shell {shell:?} on {host}]\n{}",
                                readable_keys(&data)
                            ),
                        );
                    }
                    self.accept_room_note(
                        &live,
                        author,
                        &format!("[returned the keyboard for shell {shell:?} on {host}]"),
                    );
                }
            }
        }
        Ok(shell_of(host.to_string(), overview))
    }

    /// The live children of `parent` — subagent sessions whose agent tasks
    /// exist right now. The rail's list, so it must stay cheap: metadata
    /// reads under the session lock, never a snapshot of the entries.
    async fn live_children(&self, parent: &str) -> Vec<Arc<Live>> {
        let map = self.live.lock().await;
        let mut children: Vec<Arc<Live>> = map
            .values()
            .filter(|l| {
                l.session.with(|s| {
                    s.kind == crate::session::SessionKind::Subagent
                        && s.parent_session_id.as_deref() == Some(parent)
                })
            })
            .cloned()
            .collect();
        children.sort_by_key(|l| l.session.with(|s| s.created_at));
        children
    }

    /// One live child of `parent`, or NotFound — the subagent surface has
    /// nothing to say about children whose agent tasks are gone.
    async fn live_child(&self, parent: &str, child: &str) -> Result<Arc<Live>, ApiError> {
        self.live_children(parent)
            .await
            .into_iter()
            .find(|l| l.session.with(|s| s.id == child))
            .ok_or_else(|| ApiError::new(ErrorKind::NotFound, "no such live subagent"))
    }

    pub(crate) async fn subagents(&self, id: &str) -> Result<api::Subagents, ApiError> {
        // Not resident is a fact, not a fault, same as the shells rail — and
        // children die with their own tasks, not the parent's.
        let subagents = self
            .live_children(id)
            .await
            .iter()
            .map(|l| subagent_of(l))
            .collect();
        Ok(api::Subagents { subagents })
    }

    pub(crate) async fn subagent_lock(
        &self,
        author: &Author,
        id: &str,
        child: &str,
        lock: api::ShellLockMode,
    ) -> Result<api::Subagent, ApiError> {
        let parent = self.live_for_shells(id).await?;
        let child_live = self.live_child(id, child).await?;
        let wanted = lock == api::ShellLockMode::User;
        let previous = child_live.set_user_hold(wanted);
        // Announce transitions only, in the parent's transcript — the room
        // where the agent that owns this child lives.
        if previous != wanted {
            let short = &child[..child.len().min(8)];
            let text = if wanted {
                format!("[took subagent {short}]")
            } else {
                format!("[handed subagent {short} back to the agent]")
            };
            self.accept_room_note(&parent, author, &text);
        }
        Ok(subagent_of(&child_live))
    }

    pub(crate) async fn subagent_input(
        &self,
        author: &Author,
        id: &str,
        child: &str,
        text: String,
    ) -> Result<api::Subagent, ApiError> {
        let parent = self.live_for_shells(id).await?;
        let child_live = self.live_child(id, child).await?;
        if !child_live.user_holds() {
            return Err(ApiError::new(
                ErrorKind::Conflict,
                "subagent is agent-held; take it before posting into it",
            ));
        }
        let child_id = child_live.session.id();
        self.post_message(author, &child_id, api::PostMessage { text: text.clone() })
            .await?;
        // Mirror shell keystrokes: what was said to the child is part of the
        // parent's conversation too — the parent agent must not continue a
        // child that answered someone it never saw ask.
        let short = &child[..child.len().min(8)];
        self.accept_room_note(
            &parent,
            author,
            &format!("[posted to subagent {short}]\n{text}"),
        );
        Ok(subagent_of(&child_live))
    }

    pub(crate) async fn poll(&self, id: &str, since: usize) -> Result<api::Poll, ApiError> {
        let session = self.load_or_snapshot(id).await?;
        let (_, busy) = self.live_flags(&session.id).await;
        let last_error = match self.get_live(&session.id).await {
            Some(l) => l.error(),
            None => None,
        };
        let all = session.entries.clone();
        let since = since.min(all.len());
        Ok(api::Poll {
            busy,
            context_tokens: session.last_usage.map(|u| u.context_tokens()),
            total: all.len(),
            entries: all[since..].to_vec(),
            last_error,
        })
    }

    pub(crate) async fn events(&self, id: &str) -> Result<api::EventStream, ApiError> {
        let live = self
            .ensure_live(id, None)
            .await
            .map_err(|e| ApiError::new(ErrorKind::Conflict, e))?;
        let rx = live.subscribe();
        let stream = futures::stream::unfold(rx, |mut rx| async move {
            loop {
                match rx.recv().await {
                    // The agent emits its own TurnFinished *before* the task
                    // persists the turn's entries; a client that refetched on
                    // it would briefly read a transcript missing the answer.
                    // The wire carries only the task-level one, sent after
                    // persistence.
                    Ok(SessionEvent::Agent(AgentEvent::TurnFinished { .. })) => continue,
                    Ok(ev) => return Some((api::StreamEvent::from(ev), rx)),
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => return None,
                }
            }
        });
        Ok(Box::pin(stream))
    }

    pub(crate) async fn cancel(&self, id: &str) -> Result<api::Poll, ApiError> {
        let live = self
            .get_live(id)
            .await
            .ok_or_else(|| ApiError::new(ErrorKind::NotFound, "session not live"))?;
        live.cancel_turn().await;
        let snapshot = live.session.snapshot();
        Ok(api::Poll {
            busy: live.is_busy(),
            context_tokens: snapshot.last_usage.map(|u| u.context_tokens()),
            total: snapshot.entries.len(),
            entries: Vec::new(),
            last_error: live.error(),
        })
    }

    pub(crate) async fn compact(&self, id: &str) -> Result<api::Poll, ApiError> {
        let live = self
            .ensure_live(id, None)
            .await
            .map_err(|e| ApiError::new(ErrorKind::Conflict, e))?;
        live.tx
            .send(Cmd::Compact { automatic: false })
            .map_err(|_| internal("agent task gone".into()))?;
        Ok(api::Poll {
            busy: true,
            context_tokens: None,
            total: 0,
            entries: Vec::new(),
            last_error: None,
        })
    }

    pub(crate) async fn retire(&self, id: &str) -> Result<api::Poll, ApiError> {
        match self.retire_live(id).await {
            Some(_) => Ok(api::Poll {
                busy: false,
                context_tokens: None,
                total: 0,
                entries: Vec::new(),
                last_error: None,
            }),
            None => Err(ApiError::new(ErrorKind::NotFound, "session not live")),
        }
    }

    pub(crate) async fn models(&self) -> Result<api::Models, ApiError> {
        Ok(api::Models {
            models: self
                .config()
                .models
                .keys()
                .into_iter()
                .map(String::from)
                .collect(),
            default_model: self.config().model.clone(),
        })
    }
}

// ---------------------------------------------------------------------------
// UserApi
// ---------------------------------------------------------------------------

/// [`Server`] bound to one caller: the [`MycoApi`] implementation frontends
/// actually hold.
///
/// Identity lives on the handle rather than on each call, which is what lets
/// the trait stay identical across the in-process server and `HttpClient`.
/// The web adapter mints one per authenticated request; the CLI mints one for
/// the roster's local user at startup.
#[derive(Clone)]
pub struct UserApi {
    server: Arc<Server>,
    author: Author,
}

impl UserApi {
    pub fn author(&self) -> &Author {
        &self.author
    }

    pub fn server(&self) -> &Arc<Server> {
        &self.server
    }
}

impl Server {
    /// A handle acting as `author`.
    pub fn as_user(self: &Arc<Self>, author: Author) -> UserApi {
        UserApi {
            server: self.clone(),
            author,
        }
    }

    /// A handle acting as the roster's local user — the CLI, and any
    /// in-process caller that is simply this machine's operator.
    pub fn as_local(self: &Arc<Self>) -> UserApi {
        let author = local_author(&self.config);
        self.as_user(author)
    }
}

#[async_trait::async_trait]
impl MycoApi for UserApi {
    async fn list_sessions(
        &self,
        include_archived: bool,
    ) -> Result<Vec<api::SessionSummary>, ApiError> {
        self.server.list_sessions(include_archived).await
    }

    async fn create_session(
        &self,
        req: api::CreateSession,
    ) -> Result<api::SessionSummary, ApiError> {
        self.server.create_session(req).await
    }

    async fn session_detail(&self, id: &str) -> Result<api::SessionDetail, ApiError> {
        self.server.session_detail(id).await
    }

    async fn update_session(
        &self,
        id: &str,
        req: api::UpdateSession,
    ) -> Result<api::SessionSummary, ApiError> {
        self.server.update_session(id, req).await
    }

    async fn post_message(&self, id: &str, req: api::PostMessage) -> Result<api::Poll, ApiError> {
        self.server.post_message(&self.author, id, req).await
    }

    async fn poll(&self, id: &str, since: usize) -> Result<api::Poll, ApiError> {
        self.server.poll(id, since).await
    }

    async fn events(&self, id: &str) -> Result<api::EventStream, ApiError> {
        self.server.events(id).await
    }

    async fn cancel(&self, id: &str) -> Result<api::Poll, ApiError> {
        self.server.cancel(id).await
    }

    async fn compact(&self, id: &str) -> Result<api::Poll, ApiError> {
        self.server.compact(id).await
    }

    async fn retire(&self, id: &str) -> Result<api::Poll, ApiError> {
        self.server.retire(id).await
    }

    async fn models(&self) -> Result<api::Models, ApiError> {
        self.server.models().await
    }

    async fn whoami(&self) -> Result<api::Identity, ApiError> {
        match &self.author {
            Author::User { id, name } => Ok(api::Identity {
                id: id.clone(),
                name: name.clone(),
            }),
            other => Err(internal(format!("handle is not a user: {other:?}"))),
        }
    }

    async fn shells(&self, id: &str) -> Result<api::Shells, ApiError> {
        self.server.shells(id).await
    }

    async fn shell_tail(
        &self,
        id: &str,
        host: &str,
        shell: &str,
        from: u64,
    ) -> Result<api::ShellTailChunk, ApiError> {
        self.server.shell_tail(id, host, shell, from).await
    }

    async fn shell_input(
        &self,
        id: &str,
        host: &str,
        shell: &str,
        data: String,
    ) -> Result<api::Shell, ApiError> {
        self.server
            .shell_input(&self.author, id, host, shell, data)
            .await
    }

    async fn shell_lock(
        &self,
        id: &str,
        host: &str,
        shell: &str,
        lock: api::ShellLockMode,
    ) -> Result<api::Shell, ApiError> {
        self.server
            .shell_lock(&self.author, id, host, shell, lock)
            .await
    }

    async fn shell_screen(
        &self,
        id: &str,
        host: &str,
        shell: &str,
    ) -> Result<api::ShellScreen, ApiError> {
        self.server.shell_screen(id, host, shell).await
    }

    async fn shell_resize(
        &self,
        id: &str,
        host: &str,
        shell: &str,
        cols: u16,
        rows: u16,
    ) -> Result<api::Shell, ApiError> {
        self.server.shell_resize(id, host, shell, cols, rows).await
    }

    async fn shell_start(
        &self,
        id: &str,
        host: &str,
        req: api::CreateShell,
    ) -> Result<api::Shell, ApiError> {
        self.server.shell_start(&self.author, id, host, req).await
    }

    async fn shell_rename(
        &self,
        id: &str,
        host: &str,
        shell: &str,
        title: Option<String>,
    ) -> Result<api::Shell, ApiError> {
        self.server
            .shell_rename(&self.author, id, host, shell, title)
            .await
    }

    async fn subagents(&self, id: &str) -> Result<api::Subagents, ApiError> {
        self.server.subagents(id).await
    }

    async fn subagent_lock(
        &self,
        id: &str,
        child: &str,
        lock: api::ShellLockMode,
    ) -> Result<api::Subagent, ApiError> {
        self.server
            .subagent_lock(&self.author, id, child, lock)
            .await
    }

    async fn subagent_input(
        &self,
        id: &str,
        child: &str,
        text: String,
    ) -> Result<api::Subagent, ApiError> {
        self.server
            .subagent_input(&self.author, id, child, text)
            .await
    }
}
