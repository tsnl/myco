//! The Rocket API server: v1 agent semantics behind `/api`, one live agent
//! task per session, concurrent across sessions.
//!
//! Each live session owns a per-session [`Harness`] and an [`Agent`] driven by
//! a dedicated tokio task; user messages queue on an mpsc channel and are
//! consumed one turn at a time (queued input — the client never blocks on a
//! running turn). Transcript reads go straight to the persisted session, so
//! clients see exactly what a resume would see; mid-turn checkpoints keep that
//! fresh at every completed tool round. Live output streams over SSE
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

use rocket::response::stream::{Event, EventStream};
use rocket::serde::json::Json;
use rocket::{State, delete, get, post, routes};
use tokio::sync::{Mutex, broadcast, mpsc, watch};

use myco_agent::{Agent, AgentEvent, CompactWorkerError, EventSink, run_compact_worker};
use myco_config::Config;
use myco_machines::harness::{Harness, StartupPreflight};
use myco_machines::tool_services::{
    ListRecentService, SessionHistoryTool, SessionMetaTool, ToolService,
};
use myco_models::{
    BackendConfig, CatalogModel, Content, Effort, GenerativeModel, GenerativeModelConfig, Message,
    Recovery,
};
use myco_session::{
    ActiveSession, Session, SessionWriteLock, expand_image_attachments, list_sessions,
};

use crate::subagent::SubagentTool;
use myco_api as api;
use myco_core::CancelToken;

const SYSTEM_PROMPT_PROLOGUE: &str = r#"
You are a helpful assistant running in an agentic harness with unfettered computer access.
"#;

/// Sessions shown in the browser list (most recent first).
const SESSION_LIST_LIMIT: usize = 200;

/// Per-session SSE fan-out capacity; slow receivers lag and skip, they never
/// block the agent.
const EVENT_BUFFER: usize = 1024;

/// Auto-compact when a turn ends with the context this full. A failed compact
/// re-triggers after the next turn — visibly, via `TurnFailed`.
const AUTO_COMPACT_FRACTION: f64 = 0.85;

// ---------------------------------------------------------------------------
// Supervisor
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
/// models here; production uses [`Supervisor::new`]'s real factory.
pub type ModelFactory = Box<
    dyn Fn(
            ModelPurpose,
            &str,
            &CatalogModel,
            &Harness,
            usize,
        ) -> Result<Arc<dyn GenerativeModel>, String>
        + Send
        + Sync,
>;

/// Owns the live-session table. Shared by the HTTP routes and the `subagent`
/// tool (which spawns children through it).
pub struct Supervisor {
    config: Config,
    live: Mutex<HashMap<String, Arc<Live>>>,
    model_factory: ModelFactory,
}

impl Supervisor {
    pub fn new(config: Config) -> Arc<Self> {
        Self::with_model_factory(config, Box::new(default_model_factory))
    }

    /// Test/embedder constructor: models come from `factory` instead of the
    /// provider backends.
    pub fn with_model_factory(config: Config, factory: ModelFactory) -> Arc<Self> {
        Arc::new(Self {
            config,
            live: Mutex::new(HashMap::new()),
            model_factory: factory,
        })
    }
}

fn default_model_factory(
    purpose: ModelPurpose,
    _session_id: &str,
    catalog_model: &CatalogModel,
    harness: &Harness,
    max_soul_bytes: usize,
) -> Result<Arc<dyn GenerativeModel>, String> {
    match purpose {
        ModelPurpose::Agent => build_model(catalog_model, harness, max_soul_bytes),
        ModelPurpose::Compactor => myco_models::new(GenerativeModelConfig {
            model: catalog_model.spec.clone(),
            tools: harness.tool_specs(),
            system_prompt: myco_agent::compactor_system_prompt(catalog_model, max_soul_bytes),
            backend_config: catalog_model.backend.clone(),
        })
        .map_err(|e| format!("failed to create compactor model: {e}")),
    }
}

/// One queued unit of work for a session's agent task.
pub(crate) enum Cmd {
    /// One user turn.
    User(String),
    /// Compact into a successor session (also queued automatically when a
    /// turn ends with the context nearly full).
    Compact,
}

/// One resident conversation: its agent task's input queue and shared handles.
pub(crate) struct Live {
    pub(crate) session: ActiveSession,
    pub(crate) tx: mpsc::UnboundedSender<Cmd>,
    busy: Arc<AtomicBool>,
    cancel: Mutex<CancelToken>,
    /// Completed-turn counter; `subagent` waits on it.
    pub(crate) turns: watch::Receiver<u64>,
    /// SSE feed (see [`BroadcastSink`]).
    events: broadcast::Sender<api::StreamEvent>,
    /// Why the most recent turn produced nothing; cleared at turn start.
    last_error: std::sync::Mutex<Option<String>>,
    /// Held for the lifetime of the live session; swapped on compaction;
    /// `None` when flock is unavailable on this filesystem.
    lock: std::sync::Mutex<Option<SessionWriteLock>>,
}

impl Live {
    pub(crate) async fn cancel_turn(&self) {
        self.cancel.lock().await.cancel();
    }

    fn set_error(&self, msg: Option<String>) {
        *self.last_error.lock().unwrap_or_else(|e| e.into_inner()) = msg;
    }

    fn error(&self) -> Option<String> {
        self.last_error
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

impl Supervisor {
    /// The live handle for `id`, booting the agent task if needed.
    /// `fresh` supplies a brand-new session to boot instead of loading.
    pub(crate) async fn ensure_live(
        self: &Arc<Self>,
        id: &str,
        fresh: Option<Session>,
    ) -> Result<Arc<Live>, String> {
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
            Err(myco_session::SessionLockError::Busy { path }) => {
                return Err(format!(
                    "session {id} is open in another myco process (lock: {})",
                    path.display()
                ));
            }
            Err(e @ myco_session::SessionLockError::Unavailable(_)) => {
                eprintln!("warning: {e}; continuing without a single-writer guard");
                None
            }
        };

        let model_key = session.model.clone();
        let catalog_model = self
            .config
            .models
            .get(&model_key)
            .map_err(|e| format!("model {model_key:?}: {e}"))?;

        let active = ActiveSession::new(session);
        active
            .with(|s| s.save())
            .map_err(|e| format!("save: {e}"))?;

        let session_tool = Arc::new(SessionMetaTool::new(active.clone())) as Arc<dyn ToolService>;
        let history_tool = Arc::new(SessionHistoryTool::new()) as Arc<dyn ToolService>;
        let list_recent_tool = Arc::new(ListRecentService::new()) as Arc<dyn ToolService>;
        let subagent_tool =
            Arc::new(SubagentTool::new(Arc::downgrade(self), id.clone())) as Arc<dyn ToolService>;
        let harness = Harness::attach_with_root_services(
            self.config.harness.clone(),
            vec![session_tool, history_tool, list_recent_tool, subagent_tool],
        )
        .await?;

        let (events, _) = broadcast::channel::<api::StreamEvent>(EVENT_BUFFER);
        let sink = Arc::new(BroadcastSink { tx: events.clone() }) as Arc<dyn EventSink>;

        let model = (self.model_factory)(
            ModelPurpose::Agent,
            &id,
            catalog_model,
            &harness,
            self.config.max_soul_bytes,
        )?;
        let mut agent = Agent::new(model, harness.clone(), sink);
        agent.set_context_window_tokens(catalog_model.spec.context_window_tokens);
        let restored = active.snapshot();
        agent.set_history(restored.messages.clone());
        agent.set_last_usage(restored.last_usage);
        wire_checkpoint(&mut agent, &active);

        let (tx, rx) = mpsc::unbounded_channel::<Cmd>();
        let (turn_tx, turns) = watch::channel(0u64);
        let busy = Arc::new(AtomicBool::new(false));
        let handle = Arc::new(Live {
            session: active.clone(),
            tx,
            busy: busy.clone(),
            cancel: Mutex::new(CancelToken::new()),
            turns,
            events: events.clone(),
            last_error: std::sync::Mutex::new(None),
            lock: std::sync::Mutex::new(lock),
        });
        live.insert(id.clone(), handle.clone());

        tokio::spawn(run_agent_task(AgentTask {
            supervisor: Arc::downgrade(self),
            agent,
            active,
            rx,
            busy,
            handle: handle.clone(),
            turn_tx,
            events,
            catalog_model: catalog_model.clone(),
            harness,
        }));
        Ok(handle)
    }

    /// Build (but do not boot) a hidden child session of `parent`.
    pub(crate) fn new_child(
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
            fresh.kind = myco_session::SessionKind::Subagent;
            fresh.parent_session_id = Some(parent.to_string());
            Ok(fresh)
        }
    }

    pub(crate) async fn get_live(&self, id: &str) -> Option<Arc<Live>> {
        self.live.lock().await.get(id).cloned()
    }

    async fn live_flags(&self, id: &str) -> (bool, bool) {
        match self.live.lock().await.get(id) {
            Some(l) => (true, l.busy.load(Ordering::Relaxed)),
            None => (false, false),
        }
    }
}

/// [`EventSink`] → per-session SSE broadcast. Sending to zero receivers is
/// fine (nobody watching); slow receivers lag and skip.
struct BroadcastSink {
    tx: broadcast::Sender<api::StreamEvent>,
}

impl EventSink for BroadcastSink {
    fn emit(&self, event: AgentEvent) {
        let ev = match event {
            AgentEvent::TextDelta { text, .. } => api::StreamEvent::TextDelta { text },
            AgentEvent::ThinkingDelta { text, .. } => api::StreamEvent::ThinkingDelta { text },
            AgentEvent::ToolStarted { tool_use, .. } => {
                let mut input = tool_use.input.to_string();
                input.truncate(200);
                api::StreamEvent::ToolStarted {
                    name: tool_use.name,
                    input,
                }
            }
            AgentEvent::TurnFinished { .. } => api::StreamEvent::TurnFinished,
        };
        let _ = self.tx.send(ev);
    }
}

// ---------------------------------------------------------------------------
// Boot
// ---------------------------------------------------------------------------

/// Resolve config, run preflight (warnings to stderr), and launch Rocket on
/// `127.0.0.1:<port>` serving `/api`.
pub async fn serve(config: Config, port: u16) -> Result<(), String> {
    let preflight = StartupPreflight::run(&config.harness.remote_hosts, config.max_soul_bytes);
    if preflight.has_problems() {
        eprintln!("{}", preflight.warning_body());
    }

    // Local bash sessions (and scripts nesting agents by hand) discover the
    // API here. SAFETY: called once at boot before shells are spawned.
    unsafe { std::env::set_var("MYCO_API", format!("http://127.0.0.1:{port}/api")) };

    let figment = rocket::Config::figment()
        .merge(("address", "127.0.0.1"))
        .merge(("port", port));

    rocket(Supervisor::new(config), figment)
        .launch()
        .await
        .map_err(|e| format!("rocket: {e}"))?;
    Ok(())
}

/// The Rocket instance serving `/api` for `sup` — separated from [`serve`] so
/// tests drive it with `rocket::local` clients.
pub fn rocket(
    sup: Arc<Supervisor>,
    figment: rocket::figment::Figment,
) -> rocket::Rocket<rocket::Build> {
    rocket::custom(figment).manage(sup).mount(
        "/api",
        routes![
            list,
            create,
            detail,
            post_message,
            poll,
            events,
            cancel,
            compact,
            archive,
            models
        ],
    )
}

/// Everything a session's agent task owns.
struct AgentTask {
    supervisor: std::sync::Weak<Supervisor>,
    agent: Agent,
    active: ActiveSession,
    rx: mpsc::UnboundedReceiver<Cmd>,
    busy: Arc<AtomicBool>,
    handle: Arc<Live>,
    turn_tx: watch::Sender<u64>,
    events: broadcast::Sender<api::StreamEvent>,
    catalog_model: CatalogModel,
    harness: Arc<Harness>,
}

/// The per-session agent task: pop queued commands, run one turn each.
async fn run_agent_task(mut t: AgentTask) {
    while let Some(cmd) = t.rx.recv().await {
        t.busy.store(true, Ordering::Relaxed);
        t.handle.set_error(None);
        let _ = t.events.send(api::StreamEvent::TurnStarted);

        // Fresh token per turn so one cancel doesn't poison the next turn.
        let cancel = CancelToken::new();
        *t.handle.cancel.lock().await = cancel.clone();

        match cmd {
            Cmd::User(text) => run_user_turn(&mut t, text, cancel).await,
            Cmd::Compact => run_compact(&mut t, cancel).await,
        }

        t.busy.store(false, Ordering::Relaxed);
        t.turn_tx.send_modify(|n| *n += 1);
        // Belt and braces: the Agent emits TurnFinished on clean turns; this
        // covers errored ones so clients always get their refetch trigger.
        let _ = t.events.send(api::StreamEvent::TurnFinished);
    }
}

async fn run_user_turn(t: &mut AgentTask, text: String, cancel: CancelToken) {
    let _ = t.active.maybe_auto_title_from_user_text(&text);

    // `@path` image mentions expand exactly like the v1 CLI.
    let max_image_bytes = t.catalog_model.spec.max_image_base64_bytes;
    let mut content = match expand_image_attachments(&text, max_image_bytes) {
        Ok(content) => content,
        Err(_) => vec![Content::Text { text: text.clone() }],
    };

    // One retry for provider blips; other failures surface immediately.
    let mut retried = false;
    let error = loop {
        match t.agent.interact(content.clone(), cancel.clone()).await {
            Ok(_) => break None,
            Err(myco_agent::AgentInteractionError::Cancelled) => {
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
        let _ = t.events.send(api::StreamEvent::TurnFailed { message: msg });
    }
    if let Err(e) = t
        .active
        .persist_messages(t.agent.history(), t.agent.last_usage(), true)
    {
        eprintln!("[{}] session save failed: {e}", t.active.id());
    }

    // Auto-compact: queue it when the context is nearly full, so the next
    // thing this task does is shrink the session.
    if let Some(usage) = t.agent.last_usage() {
        let window = t.agent.context_window_tokens();
        if window > 0 && usage.context_tokens() as f64 >= window as f64 * AUTO_COMPACT_FRACTION {
            let _ = t.handle.tx.send(Cmd::Compact);
        }
    }
}

/// `Cmd::Compact`: run the worker, then swap this task's session, history,
/// lock, and live-table entry over to the successor (the v1 `/compact`
/// lifecycle, server-side).
async fn run_compact(t: &mut AgentTask, cancel: CancelToken) {
    let fail = |t: &AgentTask, msg: String| {
        eprintln!("[{}] compact: {msg}", t.active.id());
        t.handle.set_error(Some(format!("compact: {msg}")));
        let _ = t.events.send(api::StreamEvent::TurnFailed {
            message: format!("compact: {msg}"),
        });
    };

    let Some(sup) = t.supervisor.upgrade() else {
        return;
    };
    if let Err(e) = t
        .active
        .persist_messages(t.agent.history(), t.agent.last_usage(), true)
    {
        fail(t, format!("failed to persist current session: {e}"));
        return;
    }
    let predecessor = t.active.snapshot();
    if predecessor.messages.is_empty() {
        fail(t, "session is empty".into());
        return;
    }

    let model = match (sup.model_factory)(
        ModelPurpose::Compactor,
        &predecessor.id,
        &t.catalog_model,
        &t.harness,
        sup.config.max_soul_bytes,
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
        Err(myco_session::SessionLockError::Busy { path }) => {
            fail(
                t,
                format!("successor is locked elsewhere ({})", path.display()),
            );
            return;
        }
        Err(myco_session::SessionLockError::Unavailable(_)) => None,
    };
    t.active.replace(successor.clone());
    t.agent.set_history(successor.messages.clone());
    t.agent.set_last_usage(successor.last_usage);
    *t.handle.lock.lock().unwrap_or_else(|e| e.into_inner()) = new_lock;

    // Re-key the live table: the conversation now answers to the new id.
    {
        let mut live = sup.live.lock().await;
        if let Some(entry) = live.remove(&outcome.predecessor_id) {
            live.insert(outcome.successor_id.clone(), entry);
        }
    }
    let _ = t.events.send(api::StreamEvent::Compacted {
        predecessor: outcome.predecessor_id,
        successor: outcome.successor_id,
    });
}

/// Persist agent history at replayable mid-turn boundaries (after the user
/// message, after each completed tool round).
fn wire_checkpoint(agent: &mut Agent, active_session: &ActiveSession) {
    let checkpoint_session = active_session.clone();
    agent.set_checkpoint(Box::new(move |messages, last_usage| {
        if let Err(e) = checkpoint_session.persist_messages(messages, last_usage, false) {
            eprintln!("warning: mid-turn session save failed: {e}");
        }
    }));
}

fn build_model(
    catalog_model: &CatalogModel,
    harness: &Harness,
    max_soul_bytes: usize,
) -> Result<Arc<dyn GenerativeModel>, String> {
    let mut backend_config = catalog_model.backend.clone();
    match &mut backend_config {
        BackendConfig::Anthropic(c) => c.effort = Some(Effort::High),
        BackendConfig::OpenAIResponses(c) | BackendConfig::OpenAICompletions(c) => {
            c.effort = Some(Effort::High)
        }
    }
    myco_models::new(GenerativeModelConfig {
        model: catalog_model.spec.clone(),
        tools: harness.tool_specs(),
        system_prompt: [
            SYSTEM_PROMPT_PROLOGUE.to_string(),
            myco_prompts::agent_prompt_epilogue(max_soul_bytes),
            myco_prompts::model_stamp(&catalog_model.spec.key),
        ]
        .join("\n"),
        backend_config,
    })
    .map_err(|e| format!("failed to create model: {e}"))
}

// ---------------------------------------------------------------------------
// Transcript projection
// ---------------------------------------------------------------------------

/// Lossy plaintext projection of the message history (see `myco_api::Entry`).
pub(crate) fn render_entries(messages: &[Message]) -> Vec<api::Entry> {
    let mut out = Vec::new();
    let mut push = |role: &str, text: String| {
        if !text.trim().is_empty() {
            let index = out.len();
            out.push(api::Entry {
                index,
                role: role.to_string(),
                text,
            });
        }
    };
    for m in messages {
        match m {
            Message::UserMessage { content } => push("user", content_text(content)),
            Message::AssistantMessage {
                content, tool_uses, ..
            } => {
                for c in content {
                    match c {
                        Content::Thinking { text, .. } => push("thinking", text.clone()),
                        Content::Text { text } => push("assistant", text.clone()),
                        Content::Image { .. } => push("assistant", "[image]".into()),
                    }
                }
                for t in tool_uses {
                    push(
                        "tool_use",
                        format!(
                            "{}({})",
                            t.name,
                            serde_json::to_string(&t.input).unwrap_or_default()
                        ),
                    );
                }
            }
            Message::ToolResults { tool_use_results } => {
                for r in tool_use_results {
                    let prefix = if r.is_error { "error: " } else { "" };
                    push(
                        "tool_result",
                        format!("{prefix}{}", content_text(&r.content)),
                    );
                }
            }
        }
    }
    out
}

fn content_text(content: &[Content]) -> String {
    content
        .iter()
        .map(|c| match c {
            Content::Text { text } => text.clone(),
            Content::Image { .. } => "[image]".to_string(),
            Content::Thinking { text, .. } => text.clone(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The final assistant prose of the last turn, for `subagent` results.
pub(crate) fn last_answer(messages: &[Message]) -> Option<String> {
    match messages.last()? {
        Message::AssistantMessage { content, .. } => {
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

fn summary_of(s: &Session, live: bool, busy: bool) -> api::SessionSummary {
    api::SessionSummary {
        id: s.id.clone(),
        title: s.title.clone(),
        model: s.model.clone(),
        created_at: s.created_at.to_rfc3339(),
        updated_at: s.updated_at.to_rfc3339(),
        message_count: s.messages.len(),
        snippet: String::new(),
        live,
        busy,
    }
}

// ---------------------------------------------------------------------------
// Routes
// ---------------------------------------------------------------------------

type ApiResult<T> = Result<Json<T>, rocket::response::status::Custom<Json<api::ApiError>>>;

fn err<T>(status: rocket::http::Status, msg: impl Into<String>) -> ApiResult<T> {
    Err(rocket::response::status::Custom(
        status,
        Json(api::ApiError { error: msg.into() }),
    ))
}

#[get("/sessions")]
async fn list(sup: &State<Arc<Supervisor>>) -> ApiResult<Vec<api::SessionSummary>> {
    let entries = match list_sessions(SESSION_LIST_LIMIT) {
        Ok(e) => e,
        Err(e) => return err(rocket::http::Status::InternalServerError, e),
    };
    let mut out = Vec::with_capacity(entries.len());
    for e in entries {
        let (live, busy) = sup.live_flags(&e.id).await;
        out.push(api::SessionSummary {
            id: e.id,
            title: e.title,
            model: e.model,
            created_at: e.created_at.to_rfc3339(),
            updated_at: e.updated_at.to_rfc3339(),
            message_count: e.message_count,
            snippet: e.snippet,
            live,
            busy,
        });
    }
    Ok(Json(out))
}

#[post("/sessions", data = "<req>")]
async fn create(
    sup: &State<Arc<Supervisor>>,
    req: Json<api::CreateSession>,
) -> ApiResult<api::SessionSummary> {
    let session = match req.parent_session.as_deref().map(str::trim) {
        None | Some("") => {
            let model_key = req
                .model
                .clone()
                .unwrap_or_else(|| sup.config.model.clone());
            if let Err(e) = sup.config.models.get(&model_key) {
                return err(
                    rocket::http::Status::BadRequest,
                    format!("model {model_key:?}: {e}"),
                );
            }
            Session::new(model_key)
        }
        Some(parent) => match sup.new_child(parent, req.model.clone(), req.fork) {
            Ok(s) => s,
            Err(e) => return err(rocket::http::Status::BadRequest, e),
        },
    };
    let id = session.id.clone();
    match sup.ensure_live(&id, Some(session)).await {
        Ok(l) => Ok(Json(summary_of(&l.session.snapshot(), true, false))),
        Err(e) => err(rocket::http::Status::InternalServerError, e),
    }
}

#[get("/sessions/<id>")]
async fn detail(sup: &State<Arc<Supervisor>>, id: &str) -> ApiResult<api::SessionDetail> {
    let session = match sup.get_live(id).await {
        Some(l) => l.session.snapshot(),
        None => match Session::load_by_id_or_prefix(id) {
            Ok(s) => s,
            Err(e) => return err(rocket::http::Status::NotFound, e),
        },
    };
    let (live, busy) = sup.live_flags(&session.id).await;
    Ok(Json(api::SessionDetail {
        entries: render_entries(&session.messages),
        summary: summary_of(&session, live, busy),
    }))
}

#[post("/sessions/<id>/messages", data = "<req>")]
async fn post_message(
    sup: &State<Arc<Supervisor>>,
    id: &str,
    req: Json<api::PostMessage>,
) -> ApiResult<api::Poll> {
    if req.text.trim().is_empty() {
        return err(rocket::http::Status::BadRequest, "empty message");
    }
    let live = match sup.ensure_live(id, None).await {
        Ok(l) => l,
        Err(e) => return err(rocket::http::Status::Conflict, e),
    };
    if live.tx.send(Cmd::User(req.text.clone())).is_err() {
        return err(rocket::http::Status::InternalServerError, "agent task gone");
    }
    let snapshot = live.session.snapshot();
    Ok(Json(api::Poll {
        busy: true,
        total: render_entries(&snapshot.messages).len(),
        entries: Vec::new(),
        last_error: None,
    }))
}

#[get("/sessions/<id>/poll?<since>")]
async fn poll(
    sup: &State<Arc<Supervisor>>,
    id: &str,
    since: Option<usize>,
) -> ApiResult<api::Poll> {
    let session = match sup.get_live(id).await {
        Some(l) => l.session.snapshot(),
        None => match Session::load_by_id_or_prefix(id) {
            Ok(s) => s,
            Err(e) => return err(rocket::http::Status::NotFound, e),
        },
    };
    let (_, busy) = sup.live_flags(&session.id).await;
    let last_error = match sup.get_live(&session.id).await {
        Some(l) => l.error(),
        None => None,
    };
    let all = render_entries(&session.messages);
    let since = since.unwrap_or(0).min(all.len());
    Ok(Json(api::Poll {
        busy,
        total: all.len(),
        entries: all[since..].to_vec(),
        last_error,
    }))
}

/// Live event stream (SSE). Subscribing makes the session resident: opening a
/// conversation is a resume.
#[get("/sessions/<id>/events")]
async fn events(
    sup: &State<Arc<Supervisor>>,
    id: &str,
) -> Result<EventStream![], rocket::response::status::Custom<Json<api::ApiError>>> {
    let live = sup.ensure_live(id, None).await.map_err(|e| {
        rocket::response::status::Custom(
            rocket::http::Status::Conflict,
            Json(api::ApiError { error: e }),
        )
    })?;
    let mut rx = live.events.subscribe();
    Ok(EventStream! {
        loop {
            match rx.recv().await {
                Ok(ev) => yield Event::json(&ev),
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    })
}

#[post("/sessions/<id>/cancel")]
async fn cancel(sup: &State<Arc<Supervisor>>, id: &str) -> ApiResult<api::Poll> {
    match sup.get_live(id).await {
        Some(l) => {
            l.cancel_turn().await;
            let snapshot = l.session.snapshot();
            Ok(Json(api::Poll {
                busy: l.busy.load(Ordering::Relaxed),
                total: render_entries(&snapshot.messages).len(),
                entries: Vec::new(),
                last_error: l.error(),
            }))
        }
        None => err(rocket::http::Status::NotFound, "session not live"),
    }
}

/// Queue a compaction: the agent task summarizes the session into a
/// successor (new id — watch for `StreamEvent::Compacted`).
#[post("/sessions/<id>/compact")]
async fn compact(sup: &State<Arc<Supervisor>>, id: &str) -> ApiResult<api::Poll> {
    let live = match sup.ensure_live(id, None).await {
        Ok(l) => l,
        Err(e) => return err(rocket::http::Status::Conflict, e),
    };
    if live.tx.send(Cmd::Compact).is_err() {
        return err(rocket::http::Status::InternalServerError, "agent task gone");
    }
    Ok(Json(api::Poll {
        busy: true,
        total: 0,
        entries: Vec::new(),
        last_error: None,
    }))
}

/// Retire the live agent task (the session stays on disk and resumable).
#[delete("/sessions/<id>/live")]
async fn archive(sup: &State<Arc<Supervisor>>, id: &str) -> ApiResult<api::Poll> {
    let removed = sup.live.lock().await.remove(id);
    match removed {
        Some(l) => {
            l.cancel_turn().await;
            Ok(Json(api::Poll {
                busy: false,
                total: 0,
                entries: Vec::new(),
                last_error: None,
            }))
        }
        None => err(rocket::http::Status::NotFound, "session not live"),
    }
}

#[get("/models")]
async fn models(sup: &State<Arc<Supervisor>>) -> Json<api::Models> {
    Json(api::Models {
        models: sup
            .config
            .models
            .keys()
            .into_iter()
            .map(String::from)
            .collect(),
        default_model: sup.config.model.clone(),
    })
}
