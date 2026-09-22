//! Loopback browser frontend. The worker owns the runner and session lock;
//! browser connections only observe it and never own the lifetime of a turn.

use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::{Arc, Mutex};

use axum::extract::{DefaultBodyLimit, Query, Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Redirect, Response, Sse, sse};
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::Utc;
use myco::generative_model::ToolUse;
use myco::{AgentEvent, CancelToken, EventSink};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::{broadcast, mpsc};
use uuid::Uuid;

use super::{Args, Boot, Session, WorkflowEvent, boot, persist_session};

mod markdown;
#[cfg(test)]
mod tests;
mod view;
use view::Block;

type ApiResult<T> = Result<T, (StatusCode, String)>;

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
    blocks: Vec<Block>,
}

impl Snapshot {
    fn metadata(&self) -> Value {
        json!({"session_id":self.session_id, "thread_id":self.thread_id, "title":self.title, "model":self.model, "busy":self.busy, "status":self.status})
    }
}

struct Live {
    snapshot: Snapshot,
    cancel: Option<CancelToken>,
    accepted: HashMap<Uuid, ActionRequest>,
}

#[derive(Clone, Serialize)]
struct Update {
    revision: u64,
    change: Value,
}

struct App {
    token: String,
    origin: String,
    cookie: String,
    live: Mutex<Live>,
    events: broadcast::Sender<Arc<Update>>,
    work: mpsc::Sender<Work>,
    shutdown: CancelToken,
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
            revision: snapshot.revision,
            change,
        }));
    }

    fn snapshot(&self) -> Update {
        let live = self.live.lock().unwrap();
        Update {
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

    fn sync(&self, boot: &Boot, status: &str, idle: bool) {
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
        snapshot.busy = !idle;
        snapshot.tasks = tasks;
        let change = json!({"kind":"snapshot", "snapshot":snapshot});
        self.publish(snapshot, change);
        if idle {
            live.cancel = None;
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
    New,
    Open { id: String },
    Compact,
    SelectModel { key: String },
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
struct ActionRequest {
    request_id: Uuid,
    session_id: String,
    action: Action,
}

struct Work {
    request: ActionRequest,
    cancel: CancelToken,
}

pub(super) async fn run(args: Args) -> Result<(), String> {
    let listener =
        tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, args.web.unwrap()))
            .await
            .map_err(|e| format!("cannot listen for browser UI: {e}"))?;
    let origin = format!(
        "http://{}",
        listener.local_addr().map_err(|e| e.to_string())?
    );
    let (work, receiver) = mpsc::channel(1);
    let (mut boot, app) = boot(&args, |config, _, session| {
        let session = session.snapshot();
        Arc::new(App {
            token: Uuid::new_v4().as_simple().to_string(),
            cookie: format!("myco_{}", listener.local_addr().unwrap().port()),
            origin,
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
                    blocks: view::history(session.active_thread()),
                },
                cancel: None,
                accepted: HashMap::new(),
            }),
            events: broadcast::channel(256).0,
            work,
            shutdown: CancelToken::new(),
        })
    })
    .await;
    app.sync(&boot, "Ready", true);
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
    let router = Router::new()
        .route(
            "/",
            get(|| async { Html(include_str!("assets/index.html")) }),
        )
        .route(
            "/app.js",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
                    include_str!("assets/app.js"),
                )
            }),
        )
        .route(
            "/style.css",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
                    include_str!("assets/style.css"),
                )
            }),
        )
        .route("/api/events", get(events))
        .route("/api/action", post(action))
        .route("/api/cancel", post(cancel))
        .route("/api/sessions", get(sessions))
        .route("/api/markdown", post(render_markdown))
        .route("/api/image", get(image))
        .route_layer(middleware::from_fn_with_state(app.clone(), authorize))
        .route("/auth", get(auth))
        .layer(DefaultBodyLimit::max(2 * 1024 * 1024))
        .layer(middleware::from_fn(headers))
        .with_state(app.clone());
    println!("Browser UI: {}/auth?token={}", app.origin, app.token);
    println!("Press Ctrl-C here to stop the server. Browser refreshes keep the current run alive.");
    let worker = tokio::spawn(worker(boot, app.clone(), receiver, args));
    let shutdown = app.clone();
    let result = axum::serve(listener, router)
        .with_graceful_shutdown(async move {
            let _ = tokio::signal::ctrl_c().await;
            shutdown.stop();
        })
        .await
        .map_err(|e| e.to_string());
    app.stop();
    worker.await.map_err(|e| format!("browser worker: {e}"))?;
    result
}

async fn worker(mut boot: Boot, app: Arc<App>, mut receiver: mpsc::Receiver<Work>, args: Args) {
    let mut tick = tokio::time::interval(std::time::Duration::from_secs(1));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        let work = tokio::select! {
            _ = app.shutdown.cancelled() => break,
            _ = tick.tick() => {
                app.tasks(boot.runner.runtime().running_tool_summaries());
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
                    _ = tick.tick() => app.tasks(runtime.running_tool_summaries()),
                }
            }
        };
        app.sync(
            &boot,
            if result.is_ok() { "Ready" } else { "Stopped" },
            true,
        );
        if let Err(error) = result {
            app.notice(error);
        }
    }
    if let Err(error) = persist_session(boot.runner.agent(), &boot.session, false) {
        eprintln!("browser: {error}");
    }
}

async fn execute(boot: &mut Boot, app: &App, work: Work, args: &Args) -> Result<(), String> {
    match work.request.action {
        Action::Submit { text } => match super::expand_image_attachments(
            &text,
            boot.catalog_model.spec.max_image_base64_bytes,
        ) {
            Err(error) => Err(error),
            Ok(content) => {
                let time = Utc::now();
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
        Action::New => {
            let session = Session::new(boot.catalog_model.spec.key.clone());
            switch(boot, session).await
        }
        Action::Open { id } => match Session::load_by_id_or_prefix(&id) {
            Ok(session) => switch(boot, session).await,
            Err(error) => Err(error),
        },
        Action::SelectModel { key } => {
            let catalog = boot.app_config.models.get(&key)?.clone();
            super::select_runner_model(
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

async fn switch(boot: &mut Boot, loaded: Session) -> Result<(), String> {
    if loaded.id == boot.session.id() {
        return Ok(());
    }
    myco::agent::validate_checkpoint(
        &loaded.active_thread().messages,
        loaded.active_thread().pending_operation,
    )
    .map_err(|e| e.to_string())?;
    if !boot.runner.agent().state().is_idle() {
        return Err("The current session has an outstanding operation.".into());
    }
    persist_session(boot.runner.agent(), &boot.session, false)?;
    let lock = super::lock_session_or_report(&loaded.id)?;
    let previous = boot.session.snapshot();
    let previous_runtime = boot.runner.runtime().clone();
    boot.session.replace(loaded);
    let runtime = myco::SessionRuntime::new(boot.harness.clone(), boot.session.clone());
    runtime.set_max_image_base64_bytes(boot.catalog_model.spec.max_image_base64_bytes);
    if let Err(error) = boot.runner.bind_runtime(runtime).await {
        boot.session.replace(previous);
        boot.runner
            .bind_runtime(previous_runtime)
            .await
            .map_err(|e| format!("Session switch failed: {error}; restore failed: {e}"))?;
        return Err(error.to_string());
    }
    boot.session_lock = lock;
    Ok(())
}

fn allowed(headers: &HeaderMap, app: &App, mutation: bool) -> bool {
    headers.get(header::HOST).and_then(|v| v.to_str().ok()) == app.origin.strip_prefix("http://")
        && (!mutation
            || headers.get(header::ORIGIN).and_then(|v| v.to_str().ok())
                == Some(app.origin.as_str()))
        && headers
            .get(header::COOKIE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|cookies| {
                cookies
                    .split(';')
                    .any(|cookie| cookie.trim() == format!("{}={}", app.cookie, app.token))
            })
}

async fn authorize(State(app): State<Arc<App>>, request: Request, next: Next) -> Response {
    if !allowed(
        request.headers(),
        &app,
        request.method() != axum::http::Method::GET,
    ) {
        return (
            StatusCode::UNAUTHORIZED,
            "Open the browser URL printed by myco in your terminal.",
        )
            .into_response();
    }
    next.run(request).await
}

async fn headers(request: Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    for (name, value) in [
        ("cache-control", "no-store"),
        ("referrer-policy", "no-referrer"),
        ("x-content-type-options", "nosniff"),
        (
            "content-security-policy",
            "default-src 'self'; script-src 'self'; style-src 'self'; img-src 'self' data: https: http:; connect-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'self'",
        ),
    ] {
        response.headers_mut().insert(
            header::HeaderName::from_static(name),
            value.parse().unwrap(),
        );
    }
    response
}

async fn auth(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> ApiResult<Response> {
    if headers.get(header::HOST).and_then(|v| v.to_str().ok()) != app.origin.strip_prefix("http://")
        || query.get("token") != Some(&app.token)
    {
        return Err((StatusCode::UNAUTHORIZED, "Invalid launch URL.".into()));
    }
    let mut response = Redirect::to("/").into_response();
    response.headers_mut().insert(
        header::SET_COOKIE,
        format!(
            "{}={}; HttpOnly; SameSite=Strict; Path=/",
            app.cookie, app.token
        )
        .parse()
        .unwrap(),
    );
    Ok(response)
}

async fn events(
    State(app): State<Arc<App>>,
) -> Sse<impl futures::Stream<Item = Result<sse::Event, Infallible>>> {
    let receiver = app.events.subscribe();
    let initial = app.snapshot();
    let stream = futures::stream::unfold(
        (app, receiver, Some(initial), 0),
        |(app, mut receiver, mut initial, mut cursor)| async move {
            loop {
                let update = if let Some(initial) = initial.take() {
                    initial
                } else {
                    tokio::select! {
                        _ = app.shutdown.cancelled() => return None,
                        next = receiver.recv() => match next {
                            Ok(update) if update.revision > cursor => (*update).clone(),
                            Ok(_) => continue,
                            Err(broadcast::error::RecvError::Lagged(_)) => app.snapshot(),
                            Err(broadcast::error::RecvError::Closed) => return None,
                        },
                    }
                };
                cursor = update.revision;
                let event = sse::Event::default()
                    .id(cursor.to_string())
                    .json_data(update)
                    .unwrap();
                return Some((Ok(event), (app, receiver, initial, cursor)));
            }
        },
    );
    Sse::new(stream).keep_alive(sse::KeepAlive::default())
}

async fn action(
    State(app): State<Arc<App>>,
    Json(request): Json<ActionRequest>,
) -> ApiResult<StatusCode> {
    let mut live = app.live.lock().unwrap();
    if app.shutdown.is_cancelled() {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "The server is stopping.".into(),
        ));
    }
    if let Some(accepted) = live.accepted.get(&request.request_id) {
        return if accepted == &request {
            Ok(StatusCode::ACCEPTED)
        } else {
            Err((
                StatusCode::CONFLICT,
                "Request id already used for another action.".into(),
            ))
        };
    }
    if request.session_id != live.snapshot.session_id {
        return Err((
            StatusCode::CONFLICT,
            "The active session changed. Reconnect before sending.".into(),
        ));
    }
    if live.snapshot.busy {
        return Err((
            StatusCode::CONFLICT,
            "Wait for the current run or cancel it first.".into(),
        ));
    }
    if matches!(&request.action, Action::Submit { text } if text.trim().is_empty()) {
        return Err((StatusCode::BAD_REQUEST, "Enter a message.".into()));
    }
    let cancel = CancelToken::new();
    app.work
        .try_send(Work {
            request: request.clone(),
            cancel: cancel.clone(),
        })
        .map_err(|_| {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "The session worker is unavailable.".into(),
            )
        })?;
    live.accepted.insert(request.request_id, request);
    live.cancel = Some(cancel);
    live.snapshot.busy = true;
    live.snapshot.status = "Running".into();
    let change = json!({"kind":"meta", "meta":live.snapshot.metadata()});
    app.publish(&mut live.snapshot, change);
    Ok(StatusCode::ACCEPTED)
}

#[derive(Deserialize)]
struct SessionRequest {
    session_id: String,
}

async fn cancel(
    State(app): State<Arc<App>>,
    Json(request): Json<SessionRequest>,
) -> ApiResult<StatusCode> {
    let mut live = app.live.lock().unwrap();
    if request.session_id != live.snapshot.session_id {
        return Err((StatusCode::CONFLICT, "The active session changed.".into()));
    }
    if let Some(cancel) = &live.cancel {
        cancel.cancel();
        live.snapshot.status = "Cancelling".into();
        let change = json!({"kind":"meta", "meta":live.snapshot.metadata()});
        app.publish(&mut live.snapshot, change);
    }
    Ok(StatusCode::NO_CONTENT)
}

async fn sessions() -> ApiResult<Json<Value>> {
    let sessions = tokio::task::spawn_blocking(|| myco::session::list_sessions(50))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
    Ok(Json(json!(
        sessions
            .into_iter()
            .map(|s| json!({"id":s.id, "title":s.title.unwrap_or(s.snippet), "model":s.model}))
            .collect::<Vec<_>>()
    )))
}

#[derive(Deserialize)]
struct MarkdownRequest {
    text: String,
}

async fn render_markdown(Json(request): Json<MarkdownRequest>) -> Html<String> {
    Html(markdown::render(&request.text))
}

async fn image(Query(query): Query<HashMap<String, String>>) -> ApiResult<Response> {
    let source = query
        .get("source")
        .cloned()
        .ok_or((StatusCode::BAD_REQUEST, "Missing image source.".into()))?;
    let data = tokio::task::spawn_blocking(move || -> Result<String, String> {
        if myco::core::image_store::is_reference(&source) {
            return myco::core::image_store::ImageStore::for_profile()?.resolve(&source);
        }
        let path = if source.starts_with("file://") {
            url::Url::parse(&source)
                .map_err(|e| e.to_string())?
                .to_file_path()
                .map_err(|_| "Invalid file URL")?
        } else if let Some(path) = source.strip_prefix("~/") {
            dirs::home_dir()
                .ok_or("Cannot resolve home directory")?
                .join(path)
        } else {
            std::path::PathBuf::from(&source)
        };
        myco::core::image::read_image_data_url(&path, &source, 32 * 1024 * 1024)
    })
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
    .map_err(|e| (StatusCode::NOT_FOUND, e))?;
    let (mime, payload) = data
        .strip_prefix("data:")
        .and_then(|data| data.split_once(";base64,"))
        .ok_or((StatusCode::BAD_REQUEST, "Unsupported image.".into()))?;
    use base64::Engine as _;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(payload)
        .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
    Ok(([(header::CONTENT_TYPE, mime)], bytes).into_response())
}
