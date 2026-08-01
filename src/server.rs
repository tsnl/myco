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

use myco_agent::{Agent, AgentEvent, EventSink};
use myco_config::Config;
use myco_machines::harness::{Harness, StartupPreflight};
use myco_machines::tool_services::{
    ListRecentService, SessionHistoryTool, SessionMetaTool, ToolService,
};
use myco_models::{
    BackendConfig, CatalogModel, Content, Effort, GenerativeModel, GenerativeModelConfig, Message,
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

// ---------------------------------------------------------------------------
// Supervisor
// ---------------------------------------------------------------------------

/// Owns the live-session table. Shared by the HTTP routes and the `subagent`
/// tool (which spawns children through it).
pub struct Supervisor {
    config: Config,
    live: Mutex<HashMap<String, Arc<Live>>>,
}

/// One resident conversation: its agent task's input queue and shared handles.
pub(crate) struct Live {
    pub(crate) session: ActiveSession,
    pub(crate) tx: mpsc::UnboundedSender<String>,
    busy: Arc<AtomicBool>,
    cancel: Mutex<CancelToken>,
    /// Completed-turn counter; `subagent` waits on it.
    pub(crate) turns: watch::Receiver<u64>,
    /// SSE feed (see [`BroadcastSink`]).
    events: broadcast::Sender<api::StreamEvent>,
    /// Held for the lifetime of the live session; `None` when flock is
    /// unavailable on this filesystem.
    _lock: Option<SessionWriteLock>,
}

impl Live {
    pub(crate) async fn cancel_turn(&self) {
        self.cancel.lock().await.cancel();
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

        let model = build_model(catalog_model, &harness, self.config.max_soul_bytes)?;
        let mut agent = Agent::new(model, harness.clone(), sink);
        agent.set_context_window_tokens(catalog_model.spec.context_window_tokens);
        let restored = active.snapshot();
        agent.set_history(restored.messages.clone());
        agent.set_last_usage(restored.last_usage);
        wire_checkpoint(&mut agent, &active);

        let (tx, rx) = mpsc::unbounded_channel::<String>();
        let (turn_tx, turns) = watch::channel(0u64);
        let busy = Arc::new(AtomicBool::new(false));
        let handle = Arc::new(Live {
            session: active.clone(),
            tx,
            busy: busy.clone(),
            cancel: Mutex::new(CancelToken::new()),
            turns,
            events: events.clone(),
            _lock: lock,
        });
        live.insert(id.clone(), handle.clone());

        let max_image_bytes = catalog_model.spec.max_image_base64_bytes;
        tokio::spawn(run_agent_task(
            agent,
            active,
            rx,
            busy,
            handle.clone(),
            turn_tx,
            events,
            max_image_bytes,
        ));
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

    rocket::custom(figment)
        .manage(Arc::new(Supervisor {
            config,
            live: Mutex::new(HashMap::new()),
        }))
        .mount(
            "/api",
            routes![
                list,
                create,
                detail,
                post_message,
                poll,
                events,
                cancel,
                archive,
                models
            ],
        )
        .launch()
        .await
        .map_err(|e| format!("rocket: {e}"))?;
    Ok(())
}

/// The per-session agent task: pop queued user messages, run one turn each.
#[allow(clippy::too_many_arguments)]
async fn run_agent_task(
    mut agent: Agent,
    active: ActiveSession,
    mut rx: mpsc::UnboundedReceiver<String>,
    busy: Arc<AtomicBool>,
    handle: Arc<Live>,
    turn_tx: watch::Sender<u64>,
    events: broadcast::Sender<api::StreamEvent>,
    max_image_bytes: u64,
) {
    while let Some(text) = rx.recv().await {
        busy.store(true, Ordering::Relaxed);
        let _ = events.send(api::StreamEvent::TurnStarted);
        let _ = active.maybe_auto_title_from_user_text(&text);

        // Fresh token per turn so one cancel doesn't poison the next turn.
        let cancel = CancelToken::new();
        *handle.cancel.lock().await = cancel.clone();

        // `@path` image mentions expand exactly like the v1 CLI.
        let content = match expand_image_attachments(&text, max_image_bytes) {
            Ok(content) => content,
            Err(_) => vec![Content::Text { text: text.clone() }],
        };

        if let Err(e) = agent.interact(content, cancel).await {
            eprintln!("[{}] agent turn error: {e}", active.id());
        }
        if let Err(e) = active.persist_messages(agent.history(), agent.last_usage(), true) {
            eprintln!("[{}] session save failed: {e}", active.id());
        }
        busy.store(false, Ordering::Relaxed);
        turn_tx.send_modify(|n| *n += 1);
        // Belt and braces: the Agent emits TurnFinished on clean turns; this
        // covers errored ones so clients always get their refetch trigger.
        let _ = events.send(api::StreamEvent::TurnFinished);
    }
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
    if live.tx.send(req.text.clone()).is_err() {
        return err(rocket::http::Status::InternalServerError, "agent task gone");
    }
    let snapshot = live.session.snapshot();
    Ok(Json(api::Poll {
        busy: true,
        total: render_entries(&snapshot.messages).len(),
        entries: Vec::new(),
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
    let all = render_entries(&session.messages);
    let since = since.unwrap_or(0).min(all.len());
    Ok(Json(api::Poll {
        busy,
        total: all.len(),
        entries: all[since..].to_vec(),
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
            }))
        }
        None => err(rocket::http::Status::NotFound, "session not live"),
    }
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
