//! Public loopback entrypoint. Profile workers own application state; this
//! supervisor owns discovery, URL routing, transport, and graceful shutdown.

use std::collections::{BTreeSet, HashMap};
use std::convert::Infallible;
use std::path::PathBuf;
use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::{Method, StatusCode, header};
use axum::middleware;
use axum::response::{Html, IntoResponse, Redirect, Response, Sse, sse};
use axum::routing::get;
use axum::{Json, Router};
use futures::StreamExt;
use myco::CancelToken;
use serde_json::{Value, json};
use tokio::sync::{Mutex, broadcast};

use super::super::Args;
use super::profile_worker::{ProfileEvent, Worker};

//
// Profile discovery and ownership
//

type WorkerSlot = Arc<Mutex<Option<Arc<Worker>>>>;

struct Profiles {
    args: Args,
    selected: String,
    directory: PathBuf,
    origin: String,
    workers: Mutex<HashMap<String, WorkerSlot>>,
    events: broadcast::Sender<Arc<ProfileEvent>>,
    shutdown: CancelToken,
}

impl Profiles {
    async fn names(&self) -> Result<BTreeSet<String>, String> {
        let mut names = BTreeSet::from([self.selected.clone()]);
        let mut entries = match tokio::fs::read_dir(&self.directory).await {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(names),
            Err(error) => return Err(format!("list profiles: {error}")),
        };
        while let Some(entry) = entries.next_entry().await.map_err(|e| e.to_string())? {
            if let Some(name) = entry.file_name().to_str()
                && myco::core::validate_profile(name).is_ok()
                && entry.path().is_dir()
            {
                names.insert(name.into());
            }
        }
        Ok(names)
    }

    async fn worker(&self, name: &str) -> Result<Arc<Worker>, (StatusCode, String)> {
        myco::core::validate_profile(name).map_err(|e| (StatusCode::NOT_FOUND, e))?;
        if name != self.selected
            && !tokio::fs::metadata(self.directory.join(name))
                .await
                .is_ok_and(|m| m.is_dir())
        {
            return Err((StatusCode::NOT_FOUND, format!("Unknown profile: {name}")));
        }
        let slot = self
            .workers
            .lock()
            .await
            .entry(name.into())
            .or_default()
            .clone();
        let mut cached = slot.lock().await;
        if self.shutdown.is_cancelled() {
            return Err(unavailable("The server is stopping."));
        }
        if let Some(worker) = &*cached
            && !worker.exited().await.map_err(unavailable)?
        {
            return Ok(worker.clone());
        }
        *cached = None;
        let start = Worker::start(
            &self.args,
            name,
            &self.selected,
            &self.origin,
            self.events.clone(),
        );
        let worker = tokio::select! {
            _ = self.shutdown.cancelled() => return Err(unavailable("The server is stopping.")),
            result = start => result.map_err(unavailable)?,
        };
        *cached = Some(worker.clone());
        Ok(worker)
    }

    async fn stop(&self) {
        self.shutdown.cancel();
        let workers = std::mem::take(&mut *self.workers.lock().await);
        futures::future::join_all(workers.into_values().map(|slot| async move {
            let worker = slot.lock().await.take();
            if let Some(worker) = worker {
                worker.stop().await;
            }
        }))
        .await;
    }
}

fn unavailable(error: impl ToString) -> (StatusCode, String) {
    (StatusCode::SERVICE_UNAVAILABLE, error.to_string())
}

//
// Public listener and routes
//

pub(super) async fn run(args: Args) -> Result<(), String> {
    let listener = tokio::net::TcpListener::bind((args.bind, args.port))
        .await
        .map_err(|e| format!("cannot listen for browser UI: {e}"))?;
    let address = listener.local_addr().map_err(|e| e.to_string())?;
    let selected = std::env::var("MYCO_PROFILE").map_err(|e| e.to_string())?;
    let directory = myco::core::myco_home()?
        .parent()
        .ok_or("missing profiles directory")?
        .to_owned();
    let profiles = Arc::new(Profiles {
        args,
        selected,
        directory,
        origin: format!("http://{address}"),
        workers: Mutex::new(HashMap::new()),
        events: broadcast::channel(128).0,
        shutdown: CancelToken::new(),
    });
    let launch = if profiles.args.resume.is_some() {
        profiles
            .worker(&profiles.selected)
            .await
            .map_err(|(_, e)| e)?
            .launch_path
            .clone()
    } else {
        format!("/profiles/{}/", profiles.selected)
    };
    println!("Browser UI: http://{address}{launch}");
    println!(
        "Listening on loopback only. Use an SSH tunnel for remote access (myco --help browser)."
    );
    println!(
        "Press Ctrl-C here to stop all profile instances. Profiles: http://{address}/profiles/"
    );
    let shutdown = profiles.clone();
    let result = axum::serve(listener, router(profiles.clone()))
        .with_graceful_shutdown(async move {
            let _ = tokio::signal::ctrl_c().await;
            shutdown.stop().await;
        })
        .await
        .map_err(|e| e.to_string());
    profiles.stop().await;
    result
}

fn router(profiles: Arc<Profiles>) -> Router {
    super::assets::icons()
        .route("/profiles/", get(chooser))
        .route("/api/profiles", get(list))
        .route("/api/profile-events", get(events))
        .route(
            "/profile-events.js",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
                    include_str!("assets/events.js"),
                )
            }),
        )
        .route(
            "/profile-style.css",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
                    include_str!("assets/style.css"),
                )
            }),
        )
        .fallback(forward)
        .layer(middleware::from_fn(super::origin::guard))
        .layer(middleware::from_fn(super::http::headers))
        .with_state(profiles)
}

async fn list(State(profiles): State<Arc<Profiles>>) -> Result<Json<Value>, (StatusCode, String)> {
    Ok(Json(Value::Array(
        profiles
            .names()
            .await
            .map_err(unavailable)?
            .into_iter()
            .map(|name| json!({"url":format!("/profiles/{name}/"), "name":name}))
            .collect(),
    )))
}

async fn chooser(
    State(profiles): State<Arc<Profiles>>,
) -> Result<Html<String>, (StatusCode, String)> {
    // Profile names are validated ASCII identifiers, so they need no HTML escapes.
    let links: String = profiles
        .names()
        .await
        .map_err(unavailable)?
        .into_iter()
        .map(|name| {
            format!(
                "<li><a href=\"/profiles/{name}/\"><span class=\"session-name\">{name}</span>\
             <span class=\"session-meta\">Open profile</span></a></li>"
            )
        })
        .collect();
    Ok(page(
        "Profiles",
        &format!(
            "<p>Choose a profile to open its sessions.</p><ul id=\"session-list\">{links}</ul>"
        ),
    ))
}

async fn forward(State(profiles): State<Arc<Profiles>>, request: Request) -> Response {
    let path = request.uri().path();
    if path == "/profiles" {
        return Redirect::permanent("/profiles/").into_response();
    }
    let (name, scoped) = match path.strip_prefix("/profiles/") {
        Some(path) => (path.split('/').next().unwrap_or(""), true),
        None => (profiles.selected.as_str(), false),
    };
    if myco::core::validate_profile(name).is_err() {
        return StatusCode::NOT_FOUND.into_response();
    }
    let original = request
        .uri()
        .path_and_query()
        .map_or("/", |value| value.as_str());
    let target = if scoped {
        original.to_owned()
    } else {
        format!("/profiles/{name}{original}")
    };
    if matches!(*request.method(), Method::GET | Method::HEAD)
        && ((!scoped && (path == "/" || path == "/new" || path.starts_with("/sessions/")))
            || path == format!("/profiles/{name}"))
    {
        let target = if path == format!("/profiles/{name}") {
            format!(
                "/profiles/{name}/{}",
                request
                    .uri()
                    .query()
                    .map_or(String::new(), |query| format!("?{query}"))
            )
        } else {
            target
        };
        return Redirect::temporary(&target).into_response();
    }
    let is_page = request.method() == Method::GET
        && (path == format!("/profiles/{name}/")
            || path.ends_with("/new")
            || path.contains("/sessions/") && !path.contains("/api/"));
    let result = match profiles.worker(name).await {
        Ok(worker) => worker.forward(&target, request).await.map_err(unavailable),
        Err(error) => Err(error),
    };
    match result {
        Err((status, message)) if is_page => (status, error_page(&message)).into_response(),
        result => result.into_response(),
    }
}

fn error_page(message: &str) -> Html<String> {
    let message = message
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;");
    page(
        "Profile unavailable",
        &format!(
            "<p>{message}</p><p><a class=\"button\" href=\"/profiles/\">Choose a profile</a></p>"
        ),
    )
}

fn page(title: &str, content: &str) -> Html<String> {
    Html(format!(
        r#"<!doctype html>
<html lang="en"><head>
  <meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
  <title>{title} · myco</title><link rel="stylesheet" href="/profile-style.css">
  <link rel="icon" href="/favicon.ico" sizes="16x16 32x32">
  <link rel="icon" href="/favicon.svg" type="image/svg+xml" sizes="any">
</head><body><main id="session-browser"><h1>{title}</h1>{content}</main></body></html>"#
    ))
}

//
// Multiplexed browser events
//

async fn events(
    State(profiles): State<Arc<Profiles>>,
) -> Sse<impl futures::Stream<Item = Result<sse::Event, Infallible>>> {
    let receiver = profiles.events.subscribe();
    let updates = futures::stream::unfold(
        (profiles, receiver),
        |(profiles, mut receiver)| async move {
            let event = tokio::select! {
                _ = profiles.shutdown.cancelled() => return None,
                next = receiver.recv() => match next {
                    Ok(event) => event,
                    Err(broadcast::error::RecvError::Lagged(_)) => Arc::new(ProfileEvent::Resync),
                    Err(broadcast::error::RecvError::Closed) => return None,
                },
            };
            Some((
                Ok(sse::Event::default().json_data(&*event).unwrap()),
                (profiles, receiver),
            ))
        },
    );
    let initial = futures::stream::once(async { Ok(sse::Event::default().comment("connected")) });
    Sse::new(initial.chain(updates)).keep_alive(sse::KeepAlive::default())
}
