//! Loopback routes and browser assets over the session runtime.

use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::Arc;

use axum::extract::{DefaultBodyLimit, Path, Query, Request, State};
use axum::http::{HeaderMap, Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Response, Sse, sse};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::broadcast;

use super::{
    files::Files,
    markdown, origin,
    runtime::{ActionRequest, CreateSession, Error, Sessions, Update},
    weather::{Coordinates, Weather},
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
    pub(super) sessions: Sessions,
    weather: Weather,
    files: Files,
}

impl Server {
    pub(super) fn new(sessions: Sessions, files: Files) -> Self {
        Self {
            sessions,
            weather: Weather::new(),
            files,
        }
    }
}

pub(super) fn router(server: Arc<Server>) -> Router {
    super::assets::routes()
        .route("/api/sky/weather", get(sky_weather))
        .route("/api/sky/locations", get(sky_locations))
        .route("/api/events", get(events))
        .route("/api/sessions", get(sessions).post(create_session))
        .route("/api/sessions/{id}", get(session_snapshot))
        .route("/api/sessions/{id}/action", post(session_action))
        .route("/api/sessions/{id}/cancel", post(session_cancel))
        .route("/api/sessions/{id}/archive", post(session_archive))
        .route("/api/markdown", post(render_markdown))
        .route("/api/image", get(image))
        .route("/files/{*path}", get(workspace_file))
        .route("/files/", get(workspace_index))
        .route_layer(middleware::from_fn(origin::guard))
        .layer(DefaultBodyLimit::max(2 * 1024 * 1024))
        .layer(middleware::from_fn(headers))
        .with_state(server)
}

async fn sky_weather(
    State(server): State<Arc<Server>>,
    Query(coordinates): Query<Coordinates>,
) -> Result<Json<super::weather::Forecast>, Error> {
    server.weather.forecast(coordinates).await.map(Json)
}

async fn sky_locations(
    State(server): State<Arc<Server>>,
    Query(query): Query<HashMap<String, String>>,
) -> Result<Json<super::weather::Locations>, Error> {
    server
        .weather
        .locations(query.get("query").map_or("", String::as_str))
        .await
        .map(Json)
}

async fn headers(request: Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    for (name, value) in [
        ("cache-control", "no-store"),
        ("referrer-policy", "no-referrer"),
        ("x-content-type-options", "nosniff"),
        ("cross-origin-resource-policy", "same-origin"),
        ("cross-origin-opener-policy", "same-origin"),
        (
            "content-security-policy",
            "default-src 'self'; script-src 'self'; style-src 'self'; img-src 'self' data: https: http:; connect-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'self'",
        ),
    ] {
        response
            .headers_mut()
            .entry(header::HeaderName::from_static(name))
            .or_insert_with(|| value.parse().unwrap());
    }
    response
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

async fn create_session(
    State(server): State<Arc<Server>>,
    Json(request): Json<CreateSession>,
) -> Result<Json<Value>, Error> {
    Ok(Json(json!({"id": server.sessions.create(request).await?})))
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

async fn render_markdown(
    State(server): State<Arc<Server>>,
    Json(request): Json<MarkdownRequest>,
) -> Html<String> {
    Html(markdown::render(&request.text, &server.files))
}

async fn workspace_file(
    State(server): State<Arc<Server>>,
    Path(path): Path<String>,
    method: Method,
    headers: HeaderMap,
) -> Response {
    server.files.serve(path, method, headers).await
}

async fn workspace_index(
    State(server): State<Arc<Server>>,
    method: Method,
    headers: HeaderMap,
) -> Response {
    server.files.serve(String::new(), method, headers).await
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
