//! The Rocket web adapter: the multiplayer experiment. Every route is a
//! one-liner over [`MycoApi`] — the same trait the in-process [`Server`]
//! implements and `client::HttpClient` speaks from the other side.
//!
//! Requests authenticate with `Authorization: Bearer <token>` against the
//! roster in `server.toml`. The [`Caller`] guard turns that token into a
//! [`UserApi`], so a route cannot reach the runtime without an identity to
//! attribute its writes to — there is no anonymous path through this module.

use std::sync::Arc;

use rocket::form::{Form, FromForm};
use rocket::http::Status;
use rocket::request::{FromRequest, Outcome};
use rocket::response::status::Custom;
use rocket::response::stream::{Event, EventStream};
use rocket::serde::json::Json;
use rocket::{Request, State, delete, get, patch, post, routes};

use crate::config::Config;
use crate::machines::harness::{fatal_startup_check, prelude_warning};
use futures::StreamExt;
use myco_api::{ApiError, ErrorKind, MycoApi};

use crate::server::{Server, UserApi};
use myco_api as api;

/// Resolve config, run preflight (warnings to stderr), and launch Rocket on
/// `127.0.0.1:<port>` serving `/api`.
pub async fn serve(config: Config, port: u16) -> Result<(), String> {
    // Fatal checks first: they end the process, so run them before anything
    // that would leave a half-built session behind.
    if let Some(fatal) = fatal_startup_check(config.max_prelude_bytes) {
        eprintln!("myco: {fatal}");
        std::process::exit(1);
    }
    if let Some(warning) = prelude_warning() {
        eprintln!("{warning}");
    }

    // Local bash sessions (and scripts nesting agents by hand) discover the
    // API here.
    // SAFETY: called once at boot before shells are spawned.
    unsafe { std::env::set_var("MYCO_API", format!("http://127.0.0.1:{port}/api")) };

    let figment = rocket::Config::figment()
        .merge(("address", "127.0.0.1"))
        .merge(("port", port));

    let server = Server::new(config);
    // Tools the agent spawns — and local CLIs / scripts (`myco -p`,
    // clients/myco.py) — act as the operator who started the server. The
    // token is minted at boot, exported into the tool environment, and
    // mirrored to `$MYCO_HOME/v2/operator.token` (0600) so sibling processes
    // on this machine can authenticate. It dies with the server: tokens live
    // only in memory, so the file goes stale on restart and is rewritten.
    match server
        .auth()
        .issue_for(server.config().roster.local().id.as_str())
    {
        Some(issued) => {
            unsafe {
                std::env::set_var("MYCO_API_TOKEN", &issued.access_token);
            }
            if let Err(e) = write_operator_token(&issued.access_token) {
                eprintln!("myco: could not write operator.token: {e}");
            }
        }
        None => eprintln!(
            "myco: no credential store entry for {:?}, so $MYCO_API is unusable from tools",
            server.config().roster.local().id
        ),
    }

    rocket(server, figment)
        .launch()
        .await
        .map_err(|e| format!("rocket: {e}"))?;
    Ok(())
}

/// Path of the local operator's bearer token: `$MYCO_HOME/v2/operator.token`.
pub fn operator_token_path() -> Result<std::path::PathBuf, String> {
    Ok(crate::core::data_root()?.join("operator.token"))
}

/// Write the operator token where local clients can read it, `0600`.
fn write_operator_token(token: &str) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    let path = operator_token_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    std::fs::write(&path, token).map_err(|e| e.to_string())?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
        .map_err(|e| e.to_string())
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
            shells,
            shell_tail,
            shell_screen,
            shell_input,
            shell_lock,
            shell_resize,
            shell_start,
            shell_rename,
            shell_ws,
            subagents,
            subagent_lock,
            subagent_input,
            cancel,
            compact,
            archive,
            models,
            whoami,
            auth_token,
            auth_logout
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
        match server.auth().authenticate_token(&presented) {
            Some(user) => Outcome::Success(Caller(server.as_user(user.author()))),
            None => Outcome::Error((
                Status::Unauthorized,
                // One message for unknown, expired, and revoked alike: the
                // client's next step is the same in every case.
                ApiError::new(ErrorKind::Unauthorized, "invalid or expired token"),
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

/// Live bash sessions across the session's hosts.
#[get("/sessions/<id>/shells")]
async fn shells(caller: Caller, id: &str) -> ApiResult<api::Shells> {
    output(caller.shells(id).await)
}

/// Non-consuming scrollback tail from absolute offset `from`.
#[get("/sessions/<id>/shells/<host>/<shell>?<from>")]
async fn shell_tail(
    caller: Caller,
    id: &str,
    host: &str,
    shell: &str,
    from: Option<u64>,
) -> ApiResult<api::ShellTailChunk> {
    output(caller.shell_tail(id, host, shell, from.unwrap_or(0)).await)
}

/// The shell's rendered terminal screen (what a pty session looks like now).
#[get("/sessions/<id>/shells/<host>/<shell>/screen")]
async fn shell_screen(
    caller: Caller,
    id: &str,
    host: &str,
    shell: &str,
) -> ApiResult<api::ShellScreen> {
    output(caller.shell_screen(id, host, shell).await)
}

/// Type into a user-locked shell.
#[post("/sessions/<id>/shells/<host>/<shell>/input", data = "<req>")]
async fn shell_input(
    caller: Caller,
    id: &str,
    host: &str,
    shell: &str,
    req: Json<api::ShellInput>,
) -> ApiResult<api::Shell> {
    output(
        caller
            .shell_input(id, host, shell, req.into_inner().data)
            .await,
    )
}

/// Open a terminal on `host` for the user (agent-owned, user-held).
#[post("/sessions/<id>/shells/<host>", data = "<req>")]
async fn shell_start(
    caller: Caller,
    id: &str,
    host: &str,
    req: Json<api::CreateShell>,
) -> ApiResult<api::Shell> {
    output(caller.shell_start(id, host, req.into_inner()).await)
}

/// Set or clear a shell's display name (`""` clears).
#[post("/sessions/<id>/shells/<host>/<shell>/rename", data = "<req>")]
async fn shell_rename(
    caller: Caller,
    id: &str,
    host: &str,
    shell: &str,
    req: Json<api::ShellRename>,
) -> ApiResult<api::Shell> {
    let title = Some(req.into_inner().title).filter(|t| !t.trim().is_empty());
    output(caller.shell_rename(id, host, shell, title).await)
}

/// Fit the shell's terminal to the viewer's window (requires the user
/// keyboard lock).
#[post("/sessions/<id>/shells/<host>/<shell>/resize", data = "<req>")]
async fn shell_resize(
    caller: Caller,
    id: &str,
    host: &str,
    shell: &str,
    req: Json<api::ShellResize>,
) -> ApiResult<api::Shell> {
    let r = req.into_inner();
    output(caller.shell_resize(id, host, shell, r.cols, r.rows).await)
}

/// The shell's live socket: ordered input/resize frames in, change-driven
/// screen pushes out. The interactive accelerator over the same guts as the
/// REST routes — same lock checks, same typed-note batching — so a client
/// without it loses latency, not capability. Auth rides `?token=` like the
/// SSE route: the browser's WebSocket cannot set headers either.
#[get("/sessions/<id>/shells/<host>/<shell>/ws")]
fn shell_ws(
    caller: Caller,
    id: &str,
    host: &str,
    shell: &str,
    ws: rocket_ws::WebSocket,
) -> rocket_ws::Channel<'static> {
    let (id, host, shell) = (id.to_string(), host.to_string(), shell.to_string());
    ws.channel(move |stream| {
        Box::pin(async move {
            shell_ws_run(caller, &id, &host, &shell, stream).await;
            Ok(())
        })
    })
}

/// The socket's event loop: one task, two sources — client frames and the
/// shell's change signal. Screens are pushed only when they differ from the
/// last one sent, with a short coalescing pause so a burst of output is one
/// redraw, not fifty.
async fn shell_ws_run(
    caller: Caller,
    id: &str,
    host: &str,
    shell: &str,
    mut stream: rocket_ws::stream::DuplexStream,
) {
    use futures::SinkExt;
    use rocket_ws::Message;

    const WAIT_SLICE: std::time::Duration = std::time::Duration::from_secs(30);
    const COALESCE: std::time::Duration = std::time::Duration::from_millis(25);

    let send = |frame: api::ShellWsOutput| {
        Message::Text(serde_json::to_string(&frame).unwrap_or_default())
    };

    // The opening screen, so the terminal paints before anything happens.
    let mut last_screen = match caller.shell_screen(id, host, shell).await {
        Ok(s) => {
            if stream
                .send(send(api::ShellWsOutput::Screen { screen: s.clone() }))
                .await
                .is_err()
            {
                return;
            }
            Some(s)
        }
        Err(_) => None,
    };
    let mut seen = 0u64;

    loop {
        tokio::select! {
            msg = stream.next() => {
                let frame = match msg {
                    Some(Ok(Message::Text(text))) => text,
                    Some(Ok(Message::Close(_))) | None => return,
                    Some(Ok(_)) => continue,
                    Some(Err(_)) => return,
                };
                let Ok(input) = serde_json::from_str::<api::ShellWsInput>(&frame) else {
                    continue;
                };
                let refused = match input {
                    api::ShellWsInput::Input { data } => {
                        caller.shell_input(id, host, shell, data).await.err()
                    }
                    api::ShellWsInput::Resize { cols, rows } => {
                        caller.shell_resize(id, host, shell, cols, rows).await.err()
                    }
                };
                if let Some(e) = refused
                    && stream
                        .send(send(api::ShellWsOutput::Error { message: e.error }))
                        .await
                        .is_err()
                {
                    return;
                }
            }
            changed = caller.server().shell_wait_change(id, host, shell, seen, WAIT_SLICE) => {
                let Ok(watermark) = changed else { return };
                if watermark != seen {
                    // Let the burst land before rendering it.
                    tokio::time::sleep(COALESCE).await;
                }
                seen = watermark;
                let Ok(screen) = caller.shell_screen(id, host, shell).await else {
                    return;
                };
                if last_screen.as_ref() != Some(&screen) {
                    if stream
                        .send(send(api::ShellWsOutput::Screen { screen: screen.clone() }))
                        .await
                        .is_err()
                    {
                        return;
                    }
                    last_screen = Some(screen);
                }
            }
        }
    }
}

/// Take or return the shell's keyboard.
#[post("/sessions/<id>/shells/<host>/<shell>/lock", data = "<req>")]
async fn shell_lock(
    caller: Caller,
    id: &str,
    host: &str,
    shell: &str,
    req: Json<api::ShellLockRequest>,
) -> ApiResult<api::Shell> {
    output(
        caller
            .shell_lock(id, host, shell, req.into_inner().lock)
            .await,
    )
}

/// Live subagent children (the `subagent` tool's hidden sessions).
#[get("/sessions/<id>/subagents")]
async fn subagents(caller: Caller, id: &str) -> ApiResult<api::Subagents> {
    output(caller.subagents(id).await)
}

/// Take or return a subagent child.
#[post("/sessions/<id>/subagents/<child>/lock", data = "<req>")]
async fn subagent_lock(
    caller: Caller,
    id: &str,
    child: &str,
    req: Json<api::ShellLockRequest>,
) -> ApiResult<api::Subagent> {
    output(caller.subagent_lock(id, child, req.into_inner().lock).await)
}

/// Post into a user-held subagent child.
#[post("/sessions/<id>/subagents/<child>/input", data = "<req>")]
async fn subagent_input(
    caller: Caller,
    id: &str,
    child: &str,
    req: Json<api::PostMessage>,
) -> ApiResult<api::Subagent> {
    output(
        caller
            .subagent_input(id, child, req.into_inner().text)
            .await,
    )
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

/// OAuth 2.0 password grant (RFC 6749 §4.3): credentials in, bearer token out.
///
/// The request is `application/x-www-form-urlencoded` and the response uses
/// the spec's field names, so a stock OAuth2 client works against it unchanged.
#[post("/auth/token", data = "<form>")]
async fn auth_token(
    server: &State<Arc<Server>>,
    form: Form<TokenRequest>,
) -> ApiResult<api::AccessToken> {
    // `grant_type` is required by the spec; refusing an unknown one keeps a
    // future grant from silently being treated as this one.
    if let Some(grant) = &form.grant_type
        && grant != "password"
    {
        return output(Err(ApiError::new(
            ErrorKind::BadRequest,
            format!("unsupported grant_type: {grant}"),
        )));
    }
    match server.auth().login(&form.username, &form.password) {
        Ok(issued) => output(Ok(api::AccessToken {
            access_token: issued.access_token,
            token_type: "bearer".into(),
            expires_in: issued.expires_in_seconds,
            user: issued.user.identity(),
        })),
        Err(e) => output(Err(ApiError::new(ErrorKind::Unauthorized, e.to_string()))),
    }
}

/// Drop the presented token. Signing out must actually end the session, not
/// just forget it client-side.
#[post("/auth/logout")]
async fn auth_logout(server: &State<Arc<Server>>, caller: Caller) -> ApiResult<api::Identity> {
    let who = caller.whoami().await;
    if let Ok(id) = &who {
        server.auth().revoke_all_for(&id.id);
    }
    output(who)
}

/// The `POST /auth/token` form body.
#[derive(FromForm)]
struct TokenRequest {
    username: String,
    password: String,
    /// Spec-required, but tolerated when absent so `curl -d user -d pass` works.
    grant_type: Option<String>,
}
