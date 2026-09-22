//! Loopback browser frontend. Each session worker owns its runner and writer lock;
//! browser connections only observe it and never own the lifetime of a turn.

use std::collections::{HashMap, VecDeque};
use std::convert::Infallible;
use std::sync::{Arc, Mutex};

use axum::extract::{DefaultBodyLimit, Path, Query, Request, State};
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

use super::{
    Args, Boot, Config, Session, StartupPreflight, WorkflowEvent, boot_session, persist_session,
};

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
    session_id: String,
    revision: u64,
    change: Value,
}

struct App {
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
            session_id: snapshot.session_id.clone(),
            revision: snapshot.revision,
            change,
        }));
    }

    fn snapshot(&self) -> Update {
        let live = self.live.lock().unwrap();
        Update {
            session_id: live.snapshot.session_id.clone(),
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

struct RunningSession {
    app: Arc<App>,
    worker: tokio::task::JoinHandle<()>,
}

struct Server {
    token: String,
    origin: String,
    cookie: String,
    launch_path: String,
    args: Arc<Args>,
    config: Config,
    preflight: StartupPreflight,
    sessions: tokio::sync::Mutex<HashMap<String, RunningSession>>,
    events: broadcast::Sender<Arc<Update>>,
    shutdown: CancelToken,
}

impl Server {
    async fn open(&self, id: &str) -> ApiResult<Arc<App>> {
        let mut sessions = self.sessions.lock().await;
        if let Some(session) = sessions.get(id) {
            return Ok(session.app.clone());
        }
        let id = id.to_owned();
        let session = tokio::task::spawn_blocking(move || Session::load_by_id_or_prefix(&id))
            .await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
            .map_err(|e| (StatusCode::NOT_FOUND, e))?;
        self.start(&mut sessions, session).await
    }

    async fn start(
        &self,
        sessions: &mut HashMap<String, RunningSession>,
        session: Session,
    ) -> ApiResult<Arc<App>> {
        if self.shutdown.is_cancelled() {
            return Err((
                StatusCode::SERVICE_UNAVAILABLE,
                "The server is stopping.".into(),
            ));
        }
        if let Some(running) = sessions.get(&session.id) {
            return Ok(running.app.clone());
        }
        let catalog = self
            .config
            .models
            .get(&self.config.model)
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?
            .clone();
        let (work, receiver) = mpsc::channel(1);
        let (mut boot, app) = boot_session(
            &self.args,
            self.config.clone(),
            catalog,
            self.preflight.clone(),
            session,
            |config, _, session| {
                let session = session.snapshot();
                Arc::new(App {
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
                    events: self.events.clone(),
                    work,
                    shutdown: CancelToken::new(),
                })
            },
        )
        .await
        .map_err(|e| (StatusCode::CONFLICT, e))?;
        // New browser sessions need a durable URL before their first message.
        let session = boot.session.snapshot();
        boot.session
            .persist_agent_state(
                &session.active_thread().id,
                boot.runner.agent().state(),
                !session.json_path().exists(),
                None,
            )
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
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
        let id = boot.session.id();
        let worker = tokio::spawn(worker(boot, app.clone(), receiver, self.args.clone()));
        sessions.insert(
            id,
            RunningSession {
                app: app.clone(),
                worker,
            },
        );
        Ok(app)
    }

    async fn snapshots(&self) -> VecDeque<Update> {
        self.sessions
            .lock()
            .await
            .values()
            .map(|session| session.app.snapshot())
            .collect()
    }

    async fn stop(&self) {
        self.shutdown.cancel();
        for session in self.sessions.lock().await.values() {
            session.app.stop();
        }
    }
}

pub(super) async fn run(args: Args) -> Result<(), String> {
    let listener =
        tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, args.web.unwrap()))
            .await
            .map_err(|e| format!("cannot listen for browser UI: {e}"))?;
    let address = listener.local_addr().map_err(|e| e.to_string())?;
    let (config, catalog, preflight) = super::prepare_boot(&args);
    let initial = (args.resume.is_some() || args.parent_session.is_some())
        .then(|| super::initial_session_or_exit(&args, &catalog.spec.key));
    let launch_path = initial
        .as_ref()
        .map_or_else(|| "/".into(), |s| format!("/sessions/{}", s.id));
    let server = Arc::new(Server {
        token: Uuid::new_v4().as_simple().to_string(),
        cookie: format!("myco_{}", address.port()),
        origin: format!("http://{address}"),
        launch_path,
        args: Arc::new(args),
        config,
        preflight,
        sessions: tokio::sync::Mutex::new(HashMap::new()),
        events: broadcast::channel(256).0,
        shutdown: CancelToken::new(),
    });
    if let Some(session) = initial {
        server
            .start(&mut *server.sessions.lock().await, session)
            .await
            .map_err(|(_, e)| e)?;
    }
    let router = Router::new()
        .route(
            "/",
            get(|| async { Html(include_str!("assets/home.html")) }),
        )
        .route(
            "/sessions/{id}",
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
            "/home.js",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
                    include_str!("assets/home.js"),
                )
            }),
        )
        .route(
            "/events.js",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
                    include_str!("assets/events.js"),
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
        .route("/api/sessions", get(sessions).post(create_session))
        .route("/api/sessions/{id}", get(session_snapshot))
        .route("/api/sessions/{id}/action", post(session_action))
        .route("/api/sessions/{id}/cancel", post(session_cancel))
        .route("/api/markdown", post(render_markdown))
        .route("/api/image", get(image))
        .route_layer(middleware::from_fn_with_state(server.clone(), authorize))
        .route("/auth", get(auth))
        .layer(DefaultBodyLimit::max(2 * 1024 * 1024))
        .layer(middleware::from_fn(headers))
        .with_state(server.clone());
    println!("Browser UI: {}/auth?token={}", server.origin, server.token);
    println!("Press Ctrl-C here to stop the server. Browser tabs keep independent sessions alive.");
    let shutdown = server.clone();
    let result = axum::serve(listener, router)
        .with_graceful_shutdown(async move {
            let _ = tokio::signal::ctrl_c().await;
            shutdown.stop().await;
        })
        .await
        .map_err(|e| e.to_string());
    server.stop().await;
    let workers = std::mem::take(&mut *server.sessions.lock().await);
    for session in workers.into_values() {
        session
            .worker
            .await
            .map_err(|e| format!("browser worker: {e}"))?;
    }
    result
}

async fn worker(
    mut boot: Boot,
    app: Arc<App>,
    mut receiver: mpsc::Receiver<Work>,
    args: Arc<Args>,
) {
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

fn allowed(headers: &HeaderMap, app: &Server, mutation: bool) -> bool {
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

async fn authorize(State(app): State<Arc<Server>>, request: Request, next: Next) -> Response {
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
    State(app): State<Arc<Server>>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> ApiResult<Response> {
    if headers.get(header::HOST).and_then(|v| v.to_str().ok()) != app.origin.strip_prefix("http://")
        || query.get("token") != Some(&app.token)
    {
        return Err((StatusCode::UNAUTHORIZED, "Invalid launch URL.".into()));
    }
    let mut response = Redirect::to(&app.launch_path).into_response();
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
    State(server): State<Arc<Server>>,
) -> Sse<impl futures::Stream<Item = Result<sse::Event, Infallible>>> {
    let receiver = server.events.subscribe();
    let initial = server.snapshots().await;
    let stream = futures::stream::unfold(
        (server, receiver, initial),
        |(server, mut receiver, mut initial)| async move {
            loop {
                let update = if let Some(initial) = initial.pop_front() {
                    initial
                } else {
                    tokio::select! {
                        _ = server.shutdown.cancelled() => return None,
                        next = receiver.recv() => match next {
                            Ok(update) => (*update).clone(),
                            Err(broadcast::error::RecvError::Lagged(_)) => {
                                initial = server.snapshots().await;
                                continue;
                            }
                            Err(broadcast::error::RecvError::Closed) => return None,
                        },
                    }
                };
                let event = sse::Event::default().json_data(update).unwrap();
                return Some((Ok(event), (server, receiver, initial)));
            }
        },
    );
    Sse::new(stream).keep_alive(sse::KeepAlive::default())
}

async fn session_snapshot(
    State(server): State<Arc<Server>>,
    Path(id): Path<String>,
) -> ApiResult<Json<Update>> {
    Ok(Json(server.open(&id).await?.snapshot()))
}

async fn session_action(
    State(server): State<Arc<Server>>,
    Path(id): Path<String>,
    Json(request): Json<ActionRequest>,
) -> ApiResult<StatusCode> {
    if request.session_id != id {
        return Err((
            StatusCode::CONFLICT,
            "The request belongs to a different session.".into(),
        ));
    }
    action(State(server.open(&id).await?), Json(request)).await
}

async fn session_cancel(
    State(server): State<Arc<Server>>,
    Path(id): Path<String>,
    Json(request): Json<SessionRequest>,
) -> ApiResult<StatusCode> {
    if request.session_id != id {
        return Err((
            StatusCode::CONFLICT,
            "The request belongs to a different session.".into(),
        ));
    }
    cancel(State(server.open(&id).await?), Json(request)).await
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateSession {
    request_id: Uuid,
}

async fn create_session(
    State(server): State<Arc<Server>>,
    Json(request): Json<CreateSession>,
) -> ApiResult<Json<Value>> {
    // The request id is also the session id, so a retry cannot create a second session.
    let session = Session::new_with_id(
        &server.config.model,
        request.request_id.as_simple().to_string(),
    );
    let id = session.id.clone();
    server
        .start(&mut *server.sessions.lock().await, session)
        .await?;
    Ok(Json(json!({"id": id})))
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
            "The request belongs to a different session.".into(),
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
        return Err((
            StatusCode::CONFLICT,
            "The request belongs to a different session.".into(),
        ));
    }
    if let Some(cancel) = &live.cancel {
        cancel.cancel();
        live.snapshot.status = "Cancelling".into();
        let change = json!({"kind":"meta", "meta":live.snapshot.metadata()});
        app.publish(&mut live.snapshot, change);
    }
    Ok(StatusCode::NO_CONTENT)
}

async fn sessions(State(server): State<Arc<Server>>) -> ApiResult<Json<Value>> {
    let sessions = tokio::task::spawn_blocking(|| myco::session::list_sessions(0))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
    let running = server.sessions.lock().await;
    Ok(Json(json!(sessions.into_iter().map(|s| {
        let live = running.get(&s.id).map(|session| session.app.live.lock().unwrap());
        json!({
            "id": s.id, "title": s.title.unwrap_or_else(|| if s.snippet.is_empty() { "New session".into() } else { s.snippet }),
            "model": live.as_ref().map_or(s.model.as_str(), |live| &live.snapshot.model),
            "updated_at": s.updated_at, "message_count": s.message_count,
            "status": live.as_ref().map_or("Saved", |live| if live.snapshot.busy { "Running" } else if !live.snapshot.tasks.is_empty() { "Background tasks" } else { "Ready" }),
        })
    }).collect::<Vec<_>>())))
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
