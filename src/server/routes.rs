//! HTTP + SSE routes under `/api`, plus static serving of the built GUI.
//!
//! REST is JSON in/out; the live event feed is Server-Sent Events. All routes
//! read/write the same session store and host pool as the CLI.

use std::path::PathBuf;

use rocket::response::stream::{Event, EventStream};
use rocket::serde::json::Json;
use rocket::tokio::select;
use rocket::tokio::sync::broadcast::error::RecvError;
use rocket::{Shutdown, State, get, patch, post};
use serde::{Deserialize, Serialize};

use crate::CancelToken;
use crate::generative_model::{Content, Effort, Model};
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
// Error type
// ---------------------------------------------------------------------------

/// Thin error wrapper mapping domain strings to HTTP statuses + JSON body.
#[derive(Debug)]
pub struct ApiError {
    status: rocket::http::Status,
    message: String,
}

impl ApiError {
    fn bad_request(msg: impl Into<String>) -> Self {
        Self {
            status: rocket::http::Status::BadRequest,
            message: msg.into(),
        }
    }
    fn not_found(msg: impl Into<String>) -> Self {
        Self {
            status: rocket::http::Status::NotFound,
            message: msg.into(),
        }
    }
    fn internal(msg: impl Into<String>) -> Self {
        Self {
            status: rocket::http::Status::InternalServerError,
            message: msg.into(),
        }
    }
}

impl<'r> rocket::response::Responder<'r, 'static> for ApiError {
    fn respond_to(self, req: &'r rocket::Request<'_>) -> rocket::response::Result<'static> {
        let body = serde_json::json!({ "error": self.message });
        let mut resp = Json(body).respond_to(req)?;
        resp.set_status(self.status);
        Ok(resp)
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
        hosts: state.harness().host_names().len(),
    })
}

#[get("/api/hosts")]
pub fn hosts(state: &State<AppState>) -> Json<Vec<HostView>> {
    let views = state
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

#[get("/api/sessions")]
pub fn list_sessions(state: &State<AppState>) -> ApiResult<Vec<SessionSummary>> {
    // Live sessions first (deduped), then saved-on-disk ones not currently live.
    let mut out: Vec<SessionSummary> = Vec::new();
    let mut seen = std::collections::HashSet::new();

    for (id, model, title) in state.live_summaries() {
        seen.insert(id.clone());
        let message_count = state
            .get_live(&id)
            .map(|s| s.active.snapshot().messages.len())
            .unwrap_or(0);
        out.push(SessionSummary {
            id,
            model,
            title,
            message_count,
            live: true,
            snippet: None,
        });
    }

    let saved = state.saved_sessions(0).map_err(ApiError::internal)?;
    for entry in saved {
        if seen.contains(&entry.id) {
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
    let model = match &req.model {
        Some(m) => m
            .parse::<Model>()
            .map_err(|e| ApiError::bad_request(format!("invalid model: {e}")))?,
        None => Model::Grok45Build,
    };
    let effort = match &req.effort {
        Some(e) => Some(
            e.parse::<Effort>()
                .map_err(|e| ApiError::bad_request(format!("invalid effort: {e}")))?,
        ),
        None => None,
    };
    let live = state
        .create_session(model, effort)
        .map_err(ApiError::internal)?;
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
    // client watches progress on the SSE stream. State is `Arc`-cheap to clone.
    let live_bg = live.clone();
    let sid = live.active.id();
    rocket::tokio::spawn(async move {
        let mut agent = live_bg.agent.lock().await;
        let result = agent.interact(vec![Content::Text { text }], cancel).await;
        // Persist regardless of outcome (history is well-formed on cancel/error).
        let _ = live_bg.active.persist_messages(agent.history(), true);
        if let Err(e) = result {
            let _ = live_bg.events.send(WireEvent::TurnError {
                message: e.to_string(),
                session_id: sid,
            });
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
    let live = state
        .open_session(id)
        .map_err(|e| ApiError::not_found(format!("session {id}: {e}")))?;
    let mut rx = live.subscribe();

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

fn detail_from_live(live: &super::state::LiveSession) -> SessionDetail {
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
