//! HTTP + SSE routes under `/api`, plus static serving of the built GUI.
//!
//! REST is JSON in/out; the live event feed is Server-Sent Events. All routes
//! read/write the same session store and host pool as the CLI via
//! [`crate::repl::Repl`].

use std::path::PathBuf;
use std::sync::Arc;

use rocket::response::stream::{Event, EventStream};
use rocket::serde::json::Json;
use rocket::tokio::select;
use rocket::tokio::sync::broadcast::error::RecvError;
use rocket::{Shutdown, State, get, patch, post};
use serde::{Deserialize, Serialize};

use crate::CancelToken;
use crate::generative_model::{Content, Effort};
use crate::repl::LiveSession;
use crate::session::{Session, SessionLink};

use super::state::AppState;
use super::wire::{WireContent, WireEvent};

// ---------------------------------------------------------------------------
// Response DTOs
// ---------------------------------------------------------------------------

#[derive(Serialize)]
pub struct Health {
    pub name: &'static str,
    pub version: &'static str,
    pub hosts: usize,
    /// Configured default model key (config.toml `model` / sole catalog entry).
    pub model: String,
}

#[derive(Serialize)]
pub struct HostView {
    pub name: String,
    pub command: Vec<String>,
    pub connected: bool,
    pub in_process: bool,
    pub tools: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Serialize)]
pub struct SessionSummary {
    pub id: String,
    pub model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub message_count: usize,
    pub live: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snippet: Option<String>,
}

#[derive(Serialize)]
pub struct MessageView {
    pub role: &'static str,
    pub content: Vec<WireContent>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tool_uses: Vec<ToolUseView>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tool_results: Vec<ToolResultView>,
}

#[derive(Serialize)]
pub struct ToolUseView {
    pub id: String,
    pub name: String,
    pub input: serde_json::Value,
}

#[derive(Serialize)]
pub struct ToolResultView {
    pub id: String,
    pub is_error: bool,
    pub content: Vec<WireContent>,
}

#[derive(Serialize)]
pub struct SessionDetail {
    pub id: String,
    pub model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub scratchpad: String,
    pub links: Vec<SessionLink>,
    pub messages: Vec<MessageView>,
    pub live: bool,
    pub running: bool,
    /// Context window size for this session's model (drives `USER used/max`).
    pub context_window_tokens: u64,
}

// ---------------------------------------------------------------------------
// Request DTOs
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct CreateSessionReq {
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub effort: Option<String>,
}

#[derive(Deserialize)]
pub struct SendMessageReq {
    pub text: String,
}

#[derive(Deserialize)]
pub struct PatchSessionReq {
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub scratchpad: Option<String>,
}

// ---------------------------------------------------------------------------
// Error mapping
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub struct ApiError {
    status: rocket::http::Status,
    message: String,
}

impl ApiError {
    fn bad_request(message: impl Into<String>) -> Self {
        Self {
            status: rocket::http::Status::BadRequest,
            message: message.into(),
        }
    }
    fn not_found(message: impl Into<String>) -> Self {
        Self {
            status: rocket::http::Status::NotFound,
            message: message.into(),
        }
    }
    fn internal(message: impl Into<String>) -> Self {
        Self {
            status: rocket::http::Status::InternalServerError,
            message: message.into(),
        }
    }
}

impl<'r, 'o: 'r> rocket::response::Responder<'r, 'o> for ApiError {
    fn respond_to(self, _: &'r rocket::Request<'_>) -> rocket::response::Result<'o> {
        let body = serde_json::json!({ "error": self.message }).to_string();
        rocket::Response::build()
            .status(self.status)
            .header(rocket::http::ContentType::JSON)
            .sized_body(body.len(), std::io::Cursor::new(body))
            .ok()
    }
}

type ApiResult<T> = Result<Json<T>, ApiError>;

// ---------------------------------------------------------------------------
// Routes
// ---------------------------------------------------------------------------

#[get("/api/health")]
pub fn health(state: &State<AppState>) -> Json<Health> {
    Json(Health {
        name: "myco",
        version: env!("CARGO_PKG_VERSION"),
        hosts: state.repl().harness().host_status().len(),
        model: state.repl().config().model.clone(),
    })
}

#[get("/api/hosts")]
pub fn hosts(state: &State<AppState>) -> Json<Vec<HostView>> {
    let views = state
        .repl()
        .harness()
        .host_status()
        .into_iter()
        .map(|h| HostView {
            name: h.name,
            command: h.command,
            connected: h.connected,
            in_process: h.in_process,
            tools: h.tools,
            error: h.error,
        })
        .collect();
    Json(views)
}

#[get("/api/sessions?<limit>")]
pub fn list_sessions(
    state: &State<AppState>,
    limit: Option<usize>,
) -> ApiResult<Vec<SessionSummary>> {
    let limit = limit.unwrap_or(50);
    let live_ids: std::collections::HashSet<String> = state
        .live_summaries()
        .into_iter()
        .map(|(id, _, _)| id)
        .collect();

    let mut out: Vec<SessionSummary> = state
        .live_summaries()
        .into_iter()
        .map(|(id, model, title)| {
            let message_count = state
                .get_live(&id)
                .map(|s| s.active.snapshot().messages.len())
                .unwrap_or(0);
            SessionSummary {
                id,
                model,
                title,
                message_count,
                live: true,
                snippet: None,
            }
        })
        .collect();

    let saved = state.saved_sessions(limit).map_err(ApiError::internal)?;
    for entry in saved {
        if live_ids.contains(&entry.id) {
            continue;
        }
        out.push(SessionSummary {
            id: entry.id,
            model: entry.model,
            title: entry.title,
            message_count: entry.message_count,
            live: false,
            snippet: if entry.snippet.is_empty() {
                None
            } else {
                Some(entry.snippet)
            },
        });
    }

    Ok(Json(out))
}

#[post("/api/sessions", data = "<req>")]
pub fn create_session(
    state: &State<AppState>,
    req: Json<CreateSessionReq>,
) -> ApiResult<SessionDetail> {
    let effort = match &req.effort {
        Some(e) => Some(
            e.parse::<Effort>()
                .map_err(|e| ApiError::bad_request(format!("invalid effort: {e}")))?,
        ),
        None => None,
    };
    // Model is a catalog key (config.toml [models]); omitted → configured
    // default. Unknown keys come back as a user-actionable catalog error.
    let live = state
        .create_session(req.model.as_deref(), effort)
        .map_err(ApiError::bad_request)?;
    Ok(Json(detail_from_live(&live)))
}

#[get("/api/sessions/<id>")]
pub fn get_session(state: &State<AppState>, id: &str) -> ApiResult<SessionDetail> {
    let live = state
        .open_session(id)
        .map_err(|e| ApiError::not_found(format!("session {id}: {e}")))?;
    Ok(Json(detail_from_live(&live)))
}

#[patch("/api/sessions/<id>", data = "<req>")]
pub fn patch_session(
    state: &State<AppState>,
    id: &str,
    req: Json<PatchSessionReq>,
) -> ApiResult<SessionDetail> {
    let live = state
        .open_session(id)
        .map_err(|e| ApiError::not_found(format!("session {id}: {e}")))?;

    let PatchSessionReq { title, scratchpad } = req.into_inner();
    live.active
        .with_mut(|s| -> Result<(), String> {
            if let Some(title) = title {
                s.set_title(if title.is_empty() { None } else { Some(title) })?;
            }
            if let Some(scratchpad) = scratchpad {
                s.set_scratchpad(scratchpad)?;
            }
            s.touch();
            // Persist metadata immediately (only if the session already exists on
            // disk or has messages — mirrors CLI "no empty session files").
            if !s.messages.is_empty() || s.json_path().exists() {
                s.save()?;
            }
            Ok(())
        })
        .map_err(ApiError::bad_request)?;

    Ok(Json(detail_from_live(&live)))
}

#[post("/api/sessions/<id>/messages", data = "<req>")]
pub async fn send_message(
    state: &State<AppState>,
    id: &str,
    req: Json<SendMessageReq>,
) -> ApiResult<serde_json::Value> {
    let live = state
        .open_session(id)
        .map_err(|e| ApiError::not_found(format!("session {id}: {e}")))?;

    let text = req.into_inner().text;
    if text.trim().is_empty() {
        return Err(ApiError::bad_request("empty message"));
    }

    // Reject a second concurrent turn on the same session.
    if live.agent.try_lock().is_err() {
        return Err(ApiError::bad_request("a turn is already in flight"));
    }

    // Auto-title from first user line (best-effort; ignore errors).
    let _ = live.active.maybe_auto_title_from_user_text(&text);

    let cancel = CancelToken::new();
    {
        let mut slot = live.cancel.lock().unwrap_or_else(|e| e.into_inner());
        *slot = Some(cancel.clone());
    }

    // Run the turn on a background task so the POST returns immediately; the
    // client watches progress on the SSE stream.
    let live_bg = live.clone();
    let sid = live.active.id();
    let state_bg = (*state).clone();
    rocket::tokio::spawn(async move {
        let mut agent = live_bg.agent.lock().await;
        let result = agent.interact(vec![Content::Text { text }], cancel).await;
        // Persist regardless of outcome (history is well-formed on cancel/error).
        let _ = live_bg.active.persist_messages(agent.history(), true);
        if let Err(e) = result {
            state_bg.emit_wire(
                &sid,
                WireEvent::TurnError {
                    message: e.to_string(),
                    session_id: sid.clone(),
                },
            );
        }
        let mut slot = live_bg.cancel.lock().unwrap_or_else(|e| e.into_inner());
        *slot = None;
    });

    Ok(Json(serde_json::json!({ "accepted": true })))
}

#[post("/api/sessions/<id>/cancel")]
pub fn cancel_session(state: &State<AppState>, id: &str) -> ApiResult<serde_json::Value> {
    let live = state
        .get_live(id)
        .ok_or_else(|| ApiError::not_found(format!("session {id} is not live")))?;
    let cancelled = {
        let slot = live.cancel.lock().unwrap_or_else(|e| e.into_inner());
        match slot.as_ref() {
            Some(token) => {
                token.cancel();
                true
            }
            None => false,
        }
    };
    Ok(Json(serde_json::json!({ "cancelled": cancelled })))
}

/// Live event feed for a session (Server-Sent Events).
///
/// Emits one SSE `message` per [`WireEvent`]. On subscriber lag, sends a
/// `warning`-tagged event and resumes; the canonical transcript is always
/// refetchable via `GET /api/sessions/{id}`.
#[get("/api/sessions/<id>/events")]
pub fn session_events(
    state: &State<AppState>,
    id: &str,
    mut shutdown: Shutdown,
) -> Result<EventStream![], ApiError> {
    // Ensure the session is live (creates bus + agent if needed).
    let live = state
        .open_session(id)
        .map_err(|e| ApiError::not_found(format!("session {id}: {e}")))?;
    let sid = live.active.id();
    let mut rx = state
        .subscribe(&sid)
        .ok_or_else(|| ApiError::internal(format!("no event bus for session {sid}")))?;

    Ok(EventStream! {
        loop {
            select! {
                msg = rx.recv() => match msg {
                    Ok(ev) => yield Event::json(&ev),
                    Err(RecvError::Lagged(n)) => {
                        yield Event::json(&serde_json::json!({
                            "type": "lagged",
                            "skipped": n,
                        })).event("warning".to_string());
                    }
                    Err(RecvError::Closed) => break,
                },
                _ = &mut shutdown => break,
            }
        }
    })
}

// ---------------------------------------------------------------------------
// Static file serving (built Trunk GUI)
// ---------------------------------------------------------------------------

/// Serve the SPA shell for any non-`/api` path so client-side routing works.
///
/// When `dist_dir` is configured (production), files are served from there and
/// unknown routes fall back to `index.html`. In dev you run Trunk instead
/// (which reverse-proxies `/api` to this server), so this route is a no-op 404.
///
/// An empty path (request for `/`) yields `index.html` via the same fallback.
#[get("/<path..>", rank = 20)]
pub async fn spa(state: &State<AppState>, path: PathBuf) -> Option<rocket::fs::NamedFile> {
    let dist = state.dist_dir()?;
    // Try the exact asset first, then fall back to index.html (SPA routing).
    let candidate = dist.join(&path);
    if candidate.is_file() {
        return rocket::fs::NamedFile::open(candidate).await.ok();
    }
    rocket::fs::NamedFile::open(dist.join("index.html"))
        .await
        .ok()
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn detail_from_live(live: &Arc<LiveSession>) -> SessionDetail {
    let snap = live.active.snapshot();
    let running = {
        let slot = live.cancel.lock().unwrap_or_else(|e| e.into_inner());
        slot.is_some()
    };
    let messages = messages_view(&snap);
    SessionDetail {
        id: snap.id,
        model: snap.model,
        title: snap.title,
        scratchpad: snap.scratchpad,
        links: snap.links,
        messages,
        live: true,
        running,
        context_window_tokens: live.model.spec.context_window_tokens,
    }
}

fn messages_view(session: &Session) -> Vec<MessageView> {
    use crate::generative_model::Message;
    session
        .messages
        .iter()
        .map(|m| match m {
            Message::UserMessage { content } => MessageView {
                role: "user",
                content: content.iter().map(WireContent::from).collect(),
                tool_uses: Vec::new(),
                tool_results: Vec::new(),
            },
            Message::AssistantMessage {
                content, tool_uses, ..
            } => MessageView {
                role: "assistant",
                content: content.iter().map(WireContent::from).collect(),
                tool_uses: tool_uses
                    .iter()
                    .map(|t| ToolUseView {
                        id: t.id.clone(),
                        name: t.name.clone(),
                        input: t.input.clone(),
                    })
                    .collect(),
                tool_results: Vec::new(),
            },
            Message::ToolResults { tool_use_results } => MessageView {
                role: "tool",
                content: Vec::new(),
                tool_uses: Vec::new(),
                tool_results: tool_use_results
                    .iter()
                    .map(|r| ToolResultView {
                        id: r.id.clone(),
                        is_error: r.is_error,
                        content: r.content.iter().map(WireContent::from).collect(),
                    })
                    .collect(),
            },
        })
        .collect()
}
