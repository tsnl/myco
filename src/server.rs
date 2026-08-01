//! The Rocket web adapter: the multiplayer experiment. Serves the session
//! runtime (`crate::supervisor`) over HTTP + SSE for `myco-gui` and scripts;
//! everything it does goes through the same [`Supervisor`] the CLI drives
//! in-process.

use std::sync::Arc;

use rocket::response::stream::{Event, EventStream};
use rocket::serde::json::Json;
use rocket::{State, delete, get, post, routes};
use tokio::sync::broadcast;

use myco_agent::AgentEvent;
use myco_config::Config;
use myco_machines::harness::StartupPreflight;
use myco_models::{Content, Message};
use myco_session::{Session, list_sessions};

use crate::supervisor::{Cmd, SessionEvent, Supervisor};
use myco_api as api;

/// Sessions shown in the browser list (most recent first).
const SESSION_LIST_LIMIT: usize = 200;

/// Project a core [`SessionEvent`] onto the wire.
fn wire_event(ev: SessionEvent) -> api::StreamEvent {
    match ev {
        SessionEvent::Agent(a) => match a {
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
        },
        SessionEvent::TurnStarted => api::StreamEvent::TurnStarted,
        SessionEvent::TurnFailed { message } => api::StreamEvent::TurnFailed { message },
        SessionEvent::TurnFinished => api::StreamEvent::TurnFinished,
        SessionEvent::Compacted {
            predecessor,
            successor,
        } => api::StreamEvent::Compacted {
            predecessor,
            successor,
        },
    }
}

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
                .unwrap_or_else(|| sup.config().model.clone());
            if let Err(e) = sup.config().models.get(&model_key) {
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
    let mut rx = live.subscribe();
    Ok(EventStream! {
        loop {
            match rx.recv().await {
                Ok(ev) => yield Event::json(&wire_event(ev)),
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
                busy: l.is_busy(),
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
    match sup.retire(id).await {
        Some(_) => Ok(Json(api::Poll {
            busy: false,
            total: 0,
            entries: Vec::new(),
            last_error: None,
        })),
        None => err(rocket::http::Status::NotFound, "session not live"),
    }
}

#[get("/models")]
async fn models(sup: &State<Arc<Supervisor>>) -> Json<api::Models> {
    Json(api::Models {
        models: sup
            .config()
            .models
            .keys()
            .into_iter()
            .map(String::from)
            .collect(),
        default_model: sup.config().model.clone(),
    })
}
