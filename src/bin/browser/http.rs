//! Authenticated loopback routes and browser assets over the session runtime.

use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::Arc;

use axum::extract::{DefaultBodyLimit, Path, Query, Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Redirect, Response, Sse, sse};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::broadcast;
use uuid::Uuid;

use super::{
    markdown,
    runtime::{ActionRequest, Error, Sessions, Update},
};

type ApiResult<T> = Result<T, (StatusCode, String)>;

impl IntoResponse for Error {
    fn into_response(self) -> Response {
        let (status, message) = match self {
            Self::Invalid(e) => (StatusCode::BAD_REQUEST, e),
            Self::NotFound(e) => (StatusCode::NOT_FOUND, e),
            Self::Conflict(e) => (StatusCode::CONFLICT, e),
            Self::Unavailable(e) => (StatusCode::SERVICE_UNAVAILABLE, e),
            Self::Internal(e) => (StatusCode::INTERNAL_SERVER_ERROR, e),
        };
        (status, message).into_response()
    }
}

pub(super) struct Server {
    pub(super) token: String,
    pub(super) origin: String,
    port: u16,
    pub(super) cookie: String,
    launch_path: String,
    pub(super) sessions: Sessions,
}

impl Server {
    pub(super) fn new(sessions: Sessions, origin: String, port: u16, launch_path: String) -> Self {
        Self {
            token: Uuid::new_v4().as_simple().to_string(),
            cookie: format!("myco_{port}"),
            origin,
            port,
            launch_path,
            sessions,
        }
    }
}

/// The origin a request addressed, or `None` when its `Host` is not this server.
///
/// A non-loopback bind answers on every name that resolves here, so the served
/// origin cannot be one fixed string; the port is what identifies this server.
/// Authority rests on the launch cookie, which a browser only sends back to the
/// host that set it, so a rebound name reaching this port arrives without one.
fn addressed_origin(headers: &HeaderMap, port: u16) -> Option<String> {
    let host = headers.get(header::HOST)?.to_str().ok()?;
    (host.rsplit_once(':')?.1.parse::<u16>().ok()? == port).then(|| format!("http://{host}"))
}

pub(super) fn router(server: Arc<Server>) -> Router {
    let mut router = Router::new();
    for (path, content_type, body) in [
        (
            "/",
            "text/html; charset=utf-8",
            include_str!("assets/home.html"),
        ),
        (
            "/sessions/{id}",
            "text/html; charset=utf-8",
            include_str!("assets/index.html"),
        ),
        (
            "/app.js",
            "text/javascript; charset=utf-8",
            include_str!("assets/app.js"),
        ),
        (
            "/home.js",
            "text/javascript; charset=utf-8",
            include_str!("assets/home.js"),
        ),
        (
            "/common.js",
            "text/javascript; charset=utf-8",
            include_str!("assets/common.js"),
        ),
        (
            "/events.js",
            "text/javascript; charset=utf-8",
            include_str!("assets/events.js"),
        ),
        (
            "/style.css",
            "text/css; charset=utf-8",
            include_str!("assets/style.css"),
        ),
        (
            "/sky.js",
            "text/javascript; charset=utf-8",
            include_str!("assets/sky.js"),
        ),
        (
            "/horizon.js",
            "text/javascript; charset=utf-8",
            include_str!("assets/horizon.js"),
        ),
    ] {
        router = router.route(
            path,
            get(move || async move { ([(header::CONTENT_TYPE, content_type)], body) }),
        );
    }
    router
        .route("/api/events", get(events))
        .route("/api/sessions", get(sessions).post(create_session))
        .route("/api/sessions/{id}", get(session_snapshot))
        .route("/api/sessions/{id}/action", post(session_action))
        .route("/api/sessions/{id}/cancel", post(session_cancel))
        .route("/api/sessions/{id}/archive", post(session_archive))
        .route("/api/markdown", post(render_markdown))
        .route("/api/image", get(image))
        .route_layer(middleware::from_fn_with_state(server.clone(), authorize))
        .route("/auth", get(auth))
        .layer(DefaultBodyLimit::max(2 * 1024 * 1024))
        .layer(middleware::from_fn(headers))
        .with_state(server)
}

pub(super) fn allowed(headers: &HeaderMap, app: &Server, mutation: bool) -> bool {
    let Some(origin) = addressed_origin(headers, app.port) else {
        return false;
    };
    (!mutation
        || headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()) == Some(origin.as_str()))
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
    if addressed_origin(&headers, app.port).is_none() || query.get("token") != Some(&app.token) {
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

pub(super) async fn events(
    State(server): State<Arc<Server>>,
) -> Sse<impl futures::Stream<Item = Result<sse::Event, Infallible>>> {
    let receiver = server.sessions.events.subscribe();
    let initial = server.sessions.snapshots().await;
    let stream = futures::stream::unfold(
        (server, receiver, initial),
        |(server, mut receiver, mut initial)| async move {
            loop {
                let update = if let Some(initial) = initial.pop_front() {
                    initial
                } else {
                    tokio::select! {
                        _ = server.sessions.shutdown.cancelled() => return None,
                        next = receiver.recv() => match next {
                            Ok(update) => (*update).clone(),
                            Err(broadcast::error::RecvError::Lagged(_)) => {
                                initial = server.sessions.snapshots().await;
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
) -> Result<Json<Update>, Error> {
    Ok(Json(server.sessions.open(&id).await?.snapshot()))
}

pub(super) async fn session_action(
    State(server): State<Arc<Server>>,
    Path(id): Path<String>,
    Json(request): Json<ActionRequest>,
) -> Result<StatusCode, Error> {
    if request.session_id != id {
        return Err(Error::Conflict(
            "The request belongs to a different session.".into(),
        ));
    }
    server.sessions.open(&id).await?.accept(request)?;
    Ok(StatusCode::ACCEPTED)
}

pub(super) async fn session_cancel(
    State(server): State<Arc<Server>>,
    Path(id): Path<String>,
    Json(request): Json<SessionRequest>,
) -> Result<StatusCode, Error> {
    if request.session_id != id {
        return Err(Error::Conflict(
            "The request belongs to a different session.".into(),
        ));
    }
    server
        .sessions
        .open(&id)
        .await?
        .cancel(&request.session_id)?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateSession {
    request_id: Uuid,
}

async fn create_session(
    State(server): State<Arc<Server>>,
    Json(request): Json<CreateSession>,
) -> Result<Json<Value>, Error> {
    Ok(Json(
        json!({"id": server.sessions.create(request.request_id).await?}),
    ))
}

#[derive(Deserialize)]
pub(super) struct SessionRequest {
    pub(super) session_id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ArchiveRequest {
    session_id: String,
    archived: bool,
}

async fn session_archive(
    State(server): State<Arc<Server>>,
    Path(id): Path<String>,
    Json(request): Json<ArchiveRequest>,
) -> Result<StatusCode, Error> {
    if request.session_id != id {
        return Err(Error::Conflict(
            "The request belongs to a different session.".into(),
        ));
    }
    server.sessions.set_archived(id, request.archived).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct SessionFilter {
    #[serde(default)]
    archived: bool,
}

async fn sessions(
    State(server): State<Arc<Server>>,
    Query(filter): Query<SessionFilter>,
) -> Result<Json<Value>, Error> {
    Ok(Json(server.sessions.list(filter.archived).await?))
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
