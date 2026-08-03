//! The Rocket web adapter: the multiplayer experiment. Every route is a
//! one-liner over [`MycoApi`] — the same trait the in-process [`Server`]
//! implements and `client::HttpClient` speaks from the other side.
//!
//! Requests authenticate with `Authorization: Bearer <token>` against the
//! roster in `server.toml`. The [`Caller`] guard turns that token into a
//! [`UserApi`], so a route cannot reach the runtime without an identity to
//! attribute its writes to — there is no anonymous path through this module.

use std::sync::Arc;

use rocket::http::Status;
use rocket::request::{FromRequest, Outcome};
use rocket::response::status::Custom;
use rocket::response::stream::{Event, EventStream};
use rocket::serde::json::Json;
use rocket::{Request, delete, get, patch, post, routes};

use futures::StreamExt;
use myco_api::{ApiError, ErrorKind, MycoApi};
use myco_config::Config;
use myco_machines::harness::StartupPreflight;

use crate::server::{Server, UserApi};
use myco_api as api;

/// Resolve config, run preflight (warnings to stderr), and launch Rocket on
/// `127.0.0.1:<port>` serving `/api`.
pub async fn serve(config: Config, port: u16) -> Result<(), String> {
    let preflight = StartupPreflight::run(&config.harness.remote_hosts, config.max_soul_bytes);
    if preflight.has_problems() {
        eprintln!("{}", preflight.warning_body());
    }

    // Local bash sessions (and scripts nesting agents by hand) discover the
    // API here. The token is the local user's, so a shell the agent spawns
    // acts as the operator who started the server — not as someone new.
    // SAFETY: called once at boot before shells are spawned.
    unsafe { std::env::set_var("MYCO_API", format!("http://127.0.0.1:{port}/api")) };
    if let Some(token) = config.roster.local().token.clone() {
        unsafe { std::env::set_var("MYCO_API_TOKEN", token) };
    } else {
        eprintln!(
            "myco: {} has no token for {:?}, so $MYCO_API is unusable from tools — \
             add one to reach the API from a shell",
            config.roster.path.display(),
            config.roster.local().id
        );
    }

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
            update,
            post_message,
            poll,
            events,
            cancel,
            compact,
            archive,
            models,
            whoami
        ],
    )
}

// ---------------------------------------------------------------------------
// Authentication
// ---------------------------------------------------------------------------

/// An authenticated request, already bound to the roster user its bearer
/// token belongs to. Taking this as a route parameter *is* the auth check:
/// a route that asks for it cannot run without one.
pub struct Caller(UserApi);

impl std::ops::Deref for Caller {
    type Target = UserApi;
    fn deref(&self) -> &UserApi {
        &self.0
    }
}

#[rocket::async_trait]
impl<'r> FromRequest<'r> for Caller {
    type Error = ApiError;

    async fn from_request(req: &'r Request<'_>) -> Outcome<Self, ApiError> {
        let Some(server) = req.rocket().state::<Arc<Server>>() else {
            return Outcome::Error((
                Status::InternalServerError,
                ApiError::new(ErrorKind::Internal, "server missing from state"),
            ));
        };
        // `Authorization: Bearer <token>`, or `?token=` for EventSource,
        // which cannot set headers.
        let presented = req
            .headers()
            .get_one("Authorization")
            .and_then(|h| h.strip_prefix("Bearer "))
            .map(str::to_string)
            .or_else(|| {
                req.query_value::<String>("token")
                    .and_then(|v: Result<String, _>| v.ok())
            });
        let Some(presented) = presented else {
            return Outcome::Error((
                Status::Unauthorized,
                ApiError::new(
                    ErrorKind::Unauthorized,
                    "missing bearer token (Authorization: Bearer <token>)",
                ),
            ));
        };
        match server.config().roster.by_token(&presented) {
            Some(user) => Outcome::Success(Caller(server.as_user(user.author()))),
            None => Outcome::Error((
                Status::Unauthorized,
                ApiError::new(ErrorKind::Unauthorized, "unknown token"),
            )),
        }
    }
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
                ErrorKind::Unauthorized => Status::Unauthorized,
                ErrorKind::Internal => Status::InternalServerError,
            };
            Err(Custom(status, Json(e)))
        }
    }
}

#[get("/sessions?<include_archived>")]
async fn list(
    caller: Caller,
    include_archived: Option<bool>,
) -> ApiResult<Vec<api::SessionSummary>> {
    output(
        caller
            .list_sessions(include_archived.unwrap_or(false))
            .await,
    )
}

#[post("/sessions", data = "<req>")]
async fn create(caller: Caller, req: Json<api::CreateSession>) -> ApiResult<api::SessionSummary> {
    output(caller.create_session(req.into_inner()).await)
}

#[get("/sessions/<id>")]
async fn detail(caller: Caller, id: &str) -> ApiResult<api::SessionDetail> {
    output(caller.session_detail(id).await)
}

/// Set session metadata: title, archived.
#[patch("/sessions/<id>", data = "<req>")]
async fn update(
    caller: Caller,
    id: &str,
    req: Json<api::UpdateSession>,
) -> ApiResult<api::SessionSummary> {
    output(caller.update_session(id, req.into_inner()).await)
}

#[post("/sessions/<id>/messages", data = "<req>")]
async fn post_message(
    caller: Caller,
    id: &str,
    req: Json<api::PostMessage>,
) -> ApiResult<api::Poll> {
    output(caller.post_message(id, req.into_inner()).await)
}

#[get("/sessions/<id>/poll?<since>")]
async fn poll(caller: Caller, id: &str, since: Option<usize>) -> ApiResult<api::Poll> {
    output(caller.poll(id, since.unwrap_or(0)).await)
}

/// Live event stream (SSE). Subscribing makes the session resident: opening a
/// conversation is a resume.
///
/// `EventSource` cannot set an `Authorization` header, so this route also
/// accepts `?token=` — see [`Caller::from_request`].
#[get("/sessions/<id>/events")]
async fn events(caller: Caller, id: &str) -> Result<EventStream![], Custom<Json<ApiError>>> {
    let mut stream = match caller.events(id).await {
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
async fn cancel(caller: Caller, id: &str) -> ApiResult<api::Poll> {
    output(caller.cancel(id).await)
}

/// Queue a compaction: the agent task summarizes the session into a
/// successor (new id — watch for `StreamEvent::Compacted`).
#[post("/sessions/<id>/compact")]
async fn compact(caller: Caller, id: &str) -> ApiResult<api::Poll> {
    output(caller.compact(id).await)
}

/// Retire the live agent task (the session stays on disk and resumable).
#[delete("/sessions/<id>/live")]
async fn archive(caller: Caller, id: &str) -> ApiResult<api::Poll> {
    output(caller.retire(id).await)
}

#[get("/models")]
async fn models(caller: Caller) -> ApiResult<api::Models> {
    output(caller.models().await)
}

/// Who the presented token belongs to. Doubles as the client's login check.
#[get("/whoami")]
async fn whoami(caller: Caller) -> ApiResult<api::Identity> {
    output(caller.whoami().await)
}
