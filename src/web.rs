//! The Rocket web adapter: the multiplayer experiment. Every route is a
//! one-liner over [`MycoApi`] — the same trait the in-process [`Server`]
//! implements and `client::HttpClient` speaks from the other side.

use std::sync::Arc;

use rocket::http::Status;
use rocket::response::status::Custom;
use rocket::response::stream::{Event, EventStream};
use rocket::serde::json::Json;
use rocket::{State, delete, get, post, routes};

use futures::StreamExt;
use myco_api::{ApiError, ErrorKind, MycoApi};
use myco_config::Config;
use myco_machines::harness::StartupPreflight;

use crate::server::Server;
use myco_api as api;

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

    rocket(Server::new(config), figment)
        .launch()
        .await
        .map_err(|e| format!("rocket: {e}"))?;
    Ok(())
}

/// The Rocket instance serving `/api` for `server` — separated from [`serve`]
/// so tests drive it with `rocket::local` clients.
pub fn rocket(
    server: Arc<Server>,
    figment: rocket::figment::Figment,
) -> rocket::Rocket<rocket::Build> {
    rocket::custom(figment).manage(server).mount(
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

type ApiResult<T> = Result<Json<T>, Custom<Json<ApiError>>>;

fn output<T>(r: Result<T, ApiError>) -> ApiResult<T> {
    match r {
        Ok(v) => Ok(Json(v)),
        Err(e) => {
            let status = match e.kind {
                ErrorKind::NotFound => Status::NotFound,
                ErrorKind::Conflict => Status::Conflict,
                ErrorKind::BadRequest => Status::BadRequest,
                ErrorKind::Internal => Status::InternalServerError,
            };
            Err(Custom(status, Json(e)))
        }
    }
}

#[get("/sessions")]
async fn list(server: &State<Arc<Server>>) -> ApiResult<Vec<api::SessionSummary>> {
    output(server.list_sessions().await)
}

#[post("/sessions", data = "<req>")]
async fn create(
    server: &State<Arc<Server>>,
    req: Json<api::CreateSession>,
) -> ApiResult<api::SessionSummary> {
    output(server.create_session(req.into_inner()).await)
}

#[get("/sessions/<id>")]
async fn detail(server: &State<Arc<Server>>, id: &str) -> ApiResult<api::SessionDetail> {
    output(server.session_detail(id).await)
}

#[post("/sessions/<id>/messages", data = "<req>")]
async fn post_message(
    server: &State<Arc<Server>>,
    id: &str,
    req: Json<api::PostMessage>,
) -> ApiResult<api::Poll> {
    output(server.post_message(id, req.into_inner()).await)
}

#[get("/sessions/<id>/poll?<since>")]
async fn poll(server: &State<Arc<Server>>, id: &str, since: Option<usize>) -> ApiResult<api::Poll> {
    output(server.poll(id, since.unwrap_or(0)).await)
}

/// Live event stream (SSE). Subscribing makes the session resident: opening a
/// conversation is a resume.
#[get("/sessions/<id>/events")]
async fn events(
    server: &State<Arc<Server>>,
    id: &str,
) -> Result<EventStream![], Custom<Json<ApiError>>> {
    let mut stream = match server.events(id).await {
        Ok(s) => s,
        Err(e) => match output::<()>(Err(e)) {
            Err(custom) => return Err(custom),
            Ok(_) => unreachable!(),
        },
    };
    Ok(EventStream! {
        while let Some(ev) = stream.next().await {
            yield Event::json(&ev);
        }
    })
}

#[post("/sessions/<id>/cancel")]
async fn cancel(server: &State<Arc<Server>>, id: &str) -> ApiResult<api::Poll> {
    output(server.cancel(id).await)
}

/// Queue a compaction: the agent task summarizes the session into a
/// successor (new id — watch for `StreamEvent::Compacted`).
#[post("/sessions/<id>/compact")]
async fn compact(server: &State<Arc<Server>>, id: &str) -> ApiResult<api::Poll> {
    output(server.compact(id).await)
}

/// Retire the live agent task (the session stays on disk and resumable).
#[delete("/sessions/<id>/live")]
async fn archive(server: &State<Arc<Server>>, id: &str) -> ApiResult<api::Poll> {
    output(server.retire(id).await)
}

#[get("/models")]
async fn models(server: &State<Arc<Server>>) -> ApiResult<api::Models> {
    output(server.models().await)
}
