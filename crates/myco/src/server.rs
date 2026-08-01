//! The Rocket API server: v1 agent semantics behind `/api`, one live agent
//! task per session, concurrent across sessions.
//!
//! Each live session owns a per-session [`Harness`] and an [`Agent`] driven by
//! a dedicated tokio task; user messages queue on an mpsc channel and are
//! consumed one turn at a time (queued input — the client never blocks on a
//! running turn). Transcript reads go straight to the persisted session, so
//! clients see exactly what a resume would see; mid-turn checkpoints keep that
//! fresh at every completed tool round.
//!
//! Sharing one harness (one ssh connection per machine) across sessions is a
//! planned follow-up; today each live session attaches its own, exactly like
//! one v1 process per conversation.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use rocket::serde::json::Json;
use rocket::{State, delete, get, post, routes};
use tokio::sync::{Mutex, mpsc};

use myco_agent::{Agent, EventSink, NullEventSink};
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

use myco_api as api;
use myco_core::CancelToken;

const SYSTEM_PROMPT_PROLOGUE: &str = r#"
You are a helpful assistant running in an agentic harness with unfettered computer access.
"#;

/// Sessions shown in the browser list (most recent first).
const SESSION_LIST_LIMIT: usize = 200;

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

pub struct ServerState {
    config: Config,
    live: Mutex<HashMap<String, Arc<Live>>>,
}

/// One resident conversation: its agent task's input queue and shared handles.
struct Live {
    session: ActiveSession,
    tx: mpsc::UnboundedSender<String>,
    busy: Arc<AtomicBool>,
    cancel: Mutex<CancelToken>,
    /// Held for the lifetime of the live session; `None` when flock is
    /// unavailable on this filesystem.
    _lock: Option<SessionWriteLock>,
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

    // Local bash sessions (and their nested agents) discover the API here.
    // SAFETY: called once at boot before worker threads spawn shells.
    unsafe { std::env::set_var("MYCO_API", format!("http://127.0.0.1:{port}/api")) };

    let figment = rocket::Config::figment()
        .merge(("address", "127.0.0.1"))
        .merge(("port", port));

    rocket::custom(figment)
        .manage(ServerState {
            config,
            live: Mutex::new(HashMap::new()),
        })
        .mount(
            "/api",
            routes![
                list,
                create,
                detail,
                post_message,
                poll,
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

// ---------------------------------------------------------------------------
// Live session management
// ---------------------------------------------------------------------------

impl ServerState {
    /// The live handle for `id`, booting the agent task if needed.
    /// `fresh` supplies a brand-new session to boot instead of loading.
    async fn ensure_live(&self, id: &str, fresh: Option<Session>) -> Result<Arc<Live>, String> {
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
        let harness = Harness::attach_with_root_services(
            self.config.harness.clone(),
            vec![session_tool, history_tool, list_recent_tool],
        )
        .await?;

        let model = build_model(catalog_model, &harness, self.config.max_soul_bytes)?;
        let sink = Arc::new(NullEventSink) as Arc<dyn EventSink>;
        let mut agent = Agent::new(model, harness.clone(), sink);
        agent.set_context_window_tokens(catalog_model.spec.context_window_tokens);
        let restored = active.snapshot();
        agent.set_history(restored.messages.clone());
        agent.set_last_usage(restored.last_usage);
        wire_checkpoint(&mut agent, &active);

        let (tx, rx) = mpsc::unbounded_channel::<String>();
        let busy = Arc::new(AtomicBool::new(false));
        let cancel = CancelToken::new();
        let handle = Arc::new(Live {
            session: active.clone(),
            tx,
            busy: busy.clone(),
            cancel: Mutex::new(cancel.clone()),
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
            max_image_bytes,
        ));
        Ok(handle)
    }

    async fn live_flags(&self, id: &str) -> (bool, bool) {
        let live = self.live.lock().await;
        match live.get(id) {
            Some(l) => (true, l.busy.load(Ordering::Relaxed)),
            None => (false, false),
        }
    }
}

/// The per-session agent task: pop queued user messages, run one turn each.
async fn run_agent_task(
    mut agent: Agent,
    active: ActiveSession,
    mut rx: mpsc::UnboundedReceiver<String>,
    busy: Arc<AtomicBool>,
    handle: Arc<Live>,
    max_image_bytes: u64,
) {
    while let Some(text) = rx.recv().await {
        busy.store(true, Ordering::Relaxed);
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
fn render_entries(messages: &[Message]) -> Vec<api::Entry> {
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
async fn list(state: &State<ServerState>) -> ApiResult<Vec<api::SessionSummary>> {
    let entries = match list_sessions(SESSION_LIST_LIMIT) {
        Ok(e) => e,
        Err(e) => return err(rocket::http::Status::InternalServerError, e),
    };
    let mut out = Vec::with_capacity(entries.len());
    for e in entries {
        let (live, busy) = state.live_flags(&e.id).await;
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
    state: &State<ServerState>,
    req: Json<api::CreateSession>,
) -> ApiResult<api::SessionSummary> {
    let model_key = req
        .model
        .clone()
        .unwrap_or_else(|| state.config.model.clone());
    if let Err(e) = state.config.models.get(&model_key) {
        return err(
            rocket::http::Status::BadRequest,
            format!("model {model_key:?}: {e}"),
        );
    }
    let session = match req.parent_session.as_deref().map(str::trim) {
        None | Some("") => Session::new(model_key),
        Some(parent) if req.fork => match Session::load_by_id_or_prefix(parent) {
            Ok(parent_session) => parent_session.fork_child(model_key),
            Err(e) => {
                return err(
                    rocket::http::Status::BadRequest,
                    format!("fork: cannot load parent session {parent:?}: {e}"),
                );
            }
        },
        Some(parent) => {
            let mut fresh = Session::new(model_key);
            fresh.kind = myco_session::SessionKind::Subagent;
            fresh.parent_session_id = Some(parent.to_string());
            fresh
        }
    };
    let id = session.id.clone();
    match state.ensure_live(&id, Some(session)).await {
        Ok(l) => Ok(Json(summary_of(&l.session.snapshot(), true, false))),
        Err(e) => err(rocket::http::Status::InternalServerError, e),
    }
}

#[get("/sessions/<id>")]
async fn detail(state: &State<ServerState>, id: &str) -> ApiResult<api::SessionDetail> {
    let session = {
        let live = state.live.lock().await;
        match live.get(id) {
            Some(l) => l.session.snapshot(),
            None => match Session::load_by_id_or_prefix(id) {
                Ok(s) => s,
                Err(e) => return err(rocket::http::Status::NotFound, e),
            },
        }
    };
    let (live, busy) = state.live_flags(&session.id).await;
    Ok(Json(api::SessionDetail {
        entries: render_entries(&session.messages),
        summary: summary_of(&session, live, busy),
    }))
}

#[post("/sessions/<id>/messages", data = "<req>")]
async fn post_message(
    state: &State<ServerState>,
    id: &str,
    req: Json<api::PostMessage>,
) -> ApiResult<api::Poll> {
    if req.text.trim().is_empty() {
        return err(rocket::http::Status::BadRequest, "empty message");
    }
    let live = match state.ensure_live(id, None).await {
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
async fn poll(state: &State<ServerState>, id: &str, since: Option<usize>) -> ApiResult<api::Poll> {
    let session = {
        let live = state.live.lock().await;
        match live.get(id) {
            Some(l) => l.session.snapshot(),
            None => match Session::load_by_id_or_prefix(id) {
                Ok(s) => s,
                Err(e) => return err(rocket::http::Status::NotFound, e),
            },
        }
    };
    let (_, busy) = state.live_flags(&session.id).await;
    let all = render_entries(&session.messages);
    let since = since.unwrap_or(0).min(all.len());
    Ok(Json(api::Poll {
        busy,
        total: all.len(),
        entries: all[since..].to_vec(),
    }))
}

#[post("/sessions/<id>/cancel")]
async fn cancel(state: &State<ServerState>, id: &str) -> ApiResult<api::Poll> {
    let live = state.live.lock().await;
    match live.get(id) {
        Some(l) => {
            l.cancel.lock().await.cancel();
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
async fn archive(state: &State<ServerState>, id: &str) -> ApiResult<api::Poll> {
    let mut live = state.live.lock().await;
    match live.remove(id) {
        Some(l) => {
            l.cancel.lock().await.cancel();
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
async fn models(state: &State<ServerState>) -> Json<api::Models> {
    Json(api::Models {
        models: state
            .config
            .models
            .keys()
            .into_iter()
            .map(String::from)
            .collect(),
        default_model: state.config.model.clone(),
    })
}
