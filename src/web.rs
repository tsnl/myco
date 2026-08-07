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
use webauthn_rs::prelude as webauthn;

/// Resolve config, run preflight (warnings to stderr), and launch Rocket on
/// `127.0.0.1:<port>` serving `/api`.
///
/// `recover` forces a one-time sign-in code for the operator onto stderr even
/// when they have passkeys enrolled — the lockout escape hatch. Restarting
/// the process is what proves the right to it.
pub async fn serve(config: Config, port: u16, recover: bool) -> Result<(), String> {
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

    if let Some(banner) = startup_code(&server, recover) {
        eprintln!("{banner}");
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

/// The startup sign-in code: how a person first gets into a server that has
/// no signup and no passwords. Minted for the operator (the roster's local
/// user) when they have no passkey to sign in with — a fresh install — or on
/// demand with `--recover` (all passkeys lost). Returns the banner to print,
/// or `None` when nothing needs minting.
///
/// This is the whole trust rule in one function: possession of the process
/// is what makes someone the operator, so the code goes to the terminal the
/// server was started from and nowhere else.
pub fn startup_code(server: &Server, recover: bool) -> Option<String> {
    let local = &server.config().roster.local().id;
    if !recover && !server.auth().passkeys_for(local).is_empty() {
        return None;
    }
    let why = if recover {
        "--recover"
    } else {
        "no passkey enrolled yet"
    };
    match server.auth().mint_code(local) {
        Ok(minted) => Some(format!(
            "myco: one-time sign-in code for {} ({why}): {}\n      \
             single use, {} minutes — enter it on the sign-in page, then add a \
             passkey\n      (restart the server to mint another)",
            minted.user_id,
            minted.code,
            crate::auth::CODE_TTL_MINUTES
        )),
        Err(e) => Some(format!("myco: cannot mint a sign-in code: {e}")),
    }
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

/// Build the WebAuthn relying party from `[passkeys]` in server.toml. With
/// the localhost defaults any port is allowed, so the direct server port,
/// trunk's dev proxy, and an SSH tunnel all pass origin checks.
fn build_webauthn(config: &Config) -> Result<webauthn::Webauthn, String> {
    let settings = &config.roster.passkeys;
    let origin = webauthn::Url::parse(&settings.origin)
        .map_err(|e| format!("[passkeys] origin {:?}: {e}", settings.origin))?;
    let mut builder = webauthn::WebauthnBuilder::new(&settings.rp_id, &origin)
        .map_err(|e| format!("[passkeys] rp_id {:?}: {e:?}", settings.rp_id))?
        .rp_name("myco");
    if settings.rp_id == "localhost" {
        builder = builder.allow_any_port(true);
    }
    builder
        .build()
        .map_err(|e| format!("passkey relying party: {e:?}"))
}

/// The Rocket instance serving `/api` for `server` — separated from [`serve`]
/// so tests drive it with `rocket::local` clients.
pub fn rocket(
    server: Arc<Server>,
    figment: rocket::figment::Figment,
) -> rocket::Rocket<rocket::Build> {
    // A bad [passkeys] section fails startup, not the first enrollment: the
    // operator wrote it, so they are at the terminal to read the error.
    let webauthn = match build_webauthn(server.config()) {
        Ok(w) => w,
        Err(e) => {
            eprintln!("myco: {e}");
            std::process::exit(1);
        }
    };
    rocket::custom(figment)
        .manage(server)
        .manage(webauthn)
        .mount(
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
                auth_logout,
                admin_users,
                admin_disable,
                admin_enable,
                admin_revoke,
                admin_remove,
                admin_clear_passkeys,
                admin_mint_code,
                passkey_register_start,
                passkey_register_finish,
                passkey_login_start,
                passkey_login_finish
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

/// The token endpoint (RFC 6749's shape, one extension grant): a one-time
/// code in, a bearer token out. There is no password grant — codes and
/// passkeys are the only ways in.
///
/// The request is `application/x-www-form-urlencoded` and the response uses
/// the spec's field names, so a stock OAuth2 client works against it unchanged.
#[post("/auth/token", data = "<form>")]
async fn auth_token(
    server: &State<Arc<Server>>,
    form: Form<TokenRequest>,
) -> ApiResult<api::AccessToken> {
    // `grant_type` is required by the spec but tolerated when absent, so
    // `curl -d username -d code` works; anything else is refused by name —
    // notably `password`, so a stale client learns the grant is gone rather
    // than being told its credentials are wrong.
    let grant = form.grant_type.as_deref().unwrap_or("code");
    if grant != "code" {
        return output(Err(ApiError::new(
            ErrorKind::BadRequest,
            format!("unsupported grant_type: {grant} (sign in with a one-time code or a passkey)"),
        )));
    }
    match server.auth().redeem_code(&form.username, &form.code) {
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

/// The `POST /auth/token` form body: `grant_type=code` redeeming an
/// operator-minted one-time code.
#[derive(FromForm)]
struct TokenRequest {
    username: String,
    #[field(default = String::new())]
    code: String,
    /// Spec-required, but tolerated when absent so `curl -d user -d code` works.
    grant_type: Option<String>,
}

// ---------------------------------------------------------------------------
// Administration
// ---------------------------------------------------------------------------
//
// Everything the old `myco auth` CLI did, as `/api/admin` routes behind the
// [`Operator`] guard. The server is the only writer of the credential files;
// there is no other administrative surface.

/// An authenticated request from the *operator* — the roster's local user,
/// the identity whose token the server writes to `operator.token` at boot.
/// Possession of the machine is what makes someone the operator; this guard
/// is how that authority reaches HTTP.
struct Operator(#[allow(dead_code)] Caller);

#[rocket::async_trait]
impl<'r> FromRequest<'r> for Operator {
    type Error = ApiError;

    async fn from_request(req: &'r Request<'_>) -> Outcome<Self, ApiError> {
        let caller = match Caller::from_request(req).await {
            Outcome::Success(c) => c,
            Outcome::Error(e) => return Outcome::Error(e),
            Outcome::Forward(f) => return Outcome::Forward(f),
        };
        let Some(server) = req.rocket().state::<Arc<Server>>() else {
            return Outcome::Error((
                Status::InternalServerError,
                ApiError::new(ErrorKind::Internal, "server missing from state"),
            ));
        };
        let operator = &server.config().roster.local().id;
        let held = matches!(caller.author(), api::Author::User { id, .. } if id == operator);
        if held {
            Outcome::Success(Operator(caller))
        } else {
            Outcome::Error((
                Status::Unauthorized,
                ApiError::new(ErrorKind::Unauthorized, "operator only"),
            ))
        }
    }
}

/// The roster as the operator sees it: who exists, who can sign in with
/// what, who is signed in now.
#[get("/admin/users")]
async fn admin_users(server: &State<Arc<Server>>, _op: Operator) -> ApiResult<api::AdminUsers> {
    let sessions: std::collections::HashMap<String, usize> =
        server.auth().session_counts().into_iter().collect();
    let passkeys = server.auth().passkey_counts();
    output(Ok(api::AdminUsers {
        operator: server.config().roster.local().id.clone(),
        users: server
            .auth()
            .users()
            .into_iter()
            .map(|u| api::AdminUser {
                passkeys: passkeys.get(&u.id).copied().unwrap_or(0),
                sessions: sessions.get(&u.id).copied().unwrap_or(0),
                id: u.id,
                name: u.name,
                disabled: u.disabled,
            })
            .collect(),
    }))
}

/// Refuse the one action that could orphan the server: the operator locking
/// out the identity that mints codes. Every other account is fair game.
fn not_the_operator(server: &Server, id: &str) -> Result<(), ApiError> {
    if crate::auth::normalize_id(id) == server.config().roster.local().id {
        return Err(ApiError::new(
            ErrorKind::BadRequest,
            "the operator account cannot be disabled or removed — it is what `--recover` signs in as",
        ));
    }
    Ok(())
}

/// Refuse logins and end live sessions; the name stays, so history remains
/// attributable. `enable` undoes it.
#[post("/admin/users/<id>/disable")]
async fn admin_disable(server: &State<Arc<Server>>, _op: Operator, id: &str) -> ApiResult<()> {
    output(not_the_operator(server, id).and_then(|()| {
        server
            .auth()
            .set_disabled(id, true)
            .map_err(|e| ApiError::new(ErrorKind::BadRequest, e.to_string()))
    }))
}

#[post("/admin/users/<id>/enable")]
async fn admin_enable(server: &State<Arc<Server>>, _op: Operator, id: &str) -> ApiResult<()> {
    output(
        server
            .auth()
            .set_disabled(id, false)
            .map_err(|e| ApiError::new(ErrorKind::BadRequest, e.to_string())),
    )
}

/// End every live session for a user without touching their credentials.
#[post("/admin/users/<id>/revoke")]
async fn admin_revoke(
    server: &State<Arc<Server>>,
    _op: Operator,
    id: &str,
) -> ApiResult<api::AdminActionCount> {
    output(Ok(api::AdminActionCount {
        affected: server.auth().revoke_all_for(id),
    }))
}

/// Forget a user's credentials entirely — sessions, codes, and passkeys.
/// If they are still in the roster, the next restart re-adds the name with
/// nothing attached, which is exactly "start them over with a fresh code".
#[delete("/admin/users/<id>")]
async fn admin_remove(server: &State<Arc<Server>>, _op: Operator, id: &str) -> ApiResult<()> {
    output(not_the_operator(server, id).and_then(|()| {
        server
            .auth()
            .remove_user(id)
            .map_err(|e| ApiError::new(ErrorKind::BadRequest, e.to_string()))
    }))
}

/// Forget every passkey for a user (lost or compromised authenticator). Live
/// sessions survive — revoke separately if they must end now.
#[post("/admin/users/<id>/passkeys/clear")]
async fn admin_clear_passkeys(
    server: &State<Arc<Server>>,
    _op: Operator,
    id: &str,
) -> ApiResult<api::AdminActionCount> {
    output(
        server
            .auth()
            .clear_passkeys(id)
            .map(|affected| api::AdminActionCount { affected })
            .map_err(|e| ApiError::new(ErrorKind::BadRequest, e.to_string())),
    )
}

/// Mint a one-time sign-in code for `id`. Minting must happen against the
/// live server: the code is redeemed from this process's memory, so a file
/// could never carry it.
#[post("/admin/users/<id>/code")]
async fn admin_mint_code(
    server: &State<Arc<Server>>,
    _op: Operator,
    id: &str,
) -> ApiResult<api::MintedLoginCode> {
    match server.auth().mint_code(id) {
        Ok(minted) => output(Ok(api::MintedLoginCode {
            username: minted.user_id,
            code: minted.code,
            expires_at: minted.expires_at.to_rfc3339(),
        })),
        Err(e) => output(Err(ApiError::new(ErrorKind::BadRequest, e.to_string()))),
    }
}

// ---------------------------------------------------------------------------
// Passkeys
// ---------------------------------------------------------------------------
//
// Standard WebAuthn ceremonies over the webauthn-rs types, which serialize
// to the exact JSON `PublicKeyCredential.parse*OptionsFromJSON` and
// `credential.toJSON()` speak — the GUI passes them through untouched.
// Enrollment requires an authenticated session (a one-time code bootstraps
// it); login requires only a username, and answers uniformly for "no such
// user" and "no passkeys enrolled".

/// Start enrolling a passkey for the signed-in user.
#[post("/auth/passkey/register/start")]
async fn passkey_register_start(
    server: &State<Arc<Server>>,
    wan: &State<webauthn::Webauthn>,
    caller: Caller,
) -> ApiResult<webauthn::CreationChallengeResponse> {
    let api::Author::User { id, name } = caller.author().clone() else {
        return output(Err(ApiError::new(
            ErrorKind::BadRequest,
            "only a person can enroll a passkey",
        )));
    };
    // Stable per-user handle, so authenticators overwrite their own old
    // credential for this account instead of piling up duplicates.
    let user_uuid = webauthn::Uuid::new_v5(&webauthn::Uuid::NAMESPACE_OID, id.as_bytes());
    let exclude: Vec<webauthn::CredentialID> = server
        .auth()
        .passkeys_for(&id)
        .iter()
        .map(|p| p.cred_id().clone())
        .collect();
    match wan.start_passkey_registration(user_uuid, &id, &name, Some(exclude)) {
        Ok((challenge, state)) => {
            server.auth().store_registration(&id, state);
            output(Ok(challenge))
        }
        Err(e) => output(Err(ApiError::new(
            ErrorKind::Internal,
            format!("passkey registration: {e:?}"),
        ))),
    }
}

#[derive(serde::Serialize)]
struct PasskeyEnrolled {
    passkeys: usize,
}

/// Finish enrollment with the browser's created credential.
#[post("/auth/passkey/register/finish", data = "<req>")]
async fn passkey_register_finish(
    server: &State<Arc<Server>>,
    wan: &State<webauthn::Webauthn>,
    caller: Caller,
    req: Json<webauthn::RegisterPublicKeyCredential>,
) -> ApiResult<PasskeyEnrolled> {
    let api::Author::User { id, .. } = caller.author().clone() else {
        return output(Err(ApiError::new(
            ErrorKind::BadRequest,
            "only a person can enroll a passkey",
        )));
    };
    let Some(state) = server.auth().take_registration(&id) else {
        return output(Err(ApiError::new(
            ErrorKind::Conflict,
            "no registration in progress (it may have expired — start again)",
        )));
    };
    match wan.finish_passkey_registration(&req, &state) {
        Ok(passkey) => match server.auth().add_passkey(&id, passkey) {
            Ok(count) => output(Ok(PasskeyEnrolled { passkeys: count })),
            Err(e) => output(Err(ApiError::new(ErrorKind::Internal, e.to_string()))),
        },
        Err(e) => output(Err(ApiError::new(
            ErrorKind::BadRequest,
            format!("passkey rejected: {e:?}"),
        ))),
    }
}

#[derive(serde::Deserialize)]
struct PasskeyLoginStart {
    username: String,
}

#[derive(serde::Serialize)]
struct PasskeyChallenge {
    ticket: String,
    options: webauthn::RequestChallengeResponse,
}

/// Start a passkey login. One uniform error covers "no such user" and "no
/// passkeys enrolled" — the answer an attacker gets is the same either way.
#[post("/auth/passkey/login/start", data = "<req>")]
async fn passkey_login_start(
    server: &State<Arc<Server>>,
    wan: &State<webauthn::Webauthn>,
    req: Json<PasskeyLoginStart>,
) -> ApiResult<PasskeyChallenge> {
    let no_passkey = || {
        ApiError::new(
            ErrorKind::Unauthorized,
            "no passkey available for this user",
        )
    };
    let user = server.auth().get(&req.username);
    let passkeys = match &user {
        Some(u) if !u.disabled => server.auth().passkeys_for(&u.id),
        _ => Vec::new(),
    };
    if passkeys.is_empty() {
        return output(Err(no_passkey()));
    }
    match wan.start_passkey_authentication(&passkeys) {
        Ok((challenge, state)) => {
            let ticket = server
                .auth()
                .store_authentication(&user.expect("checked above").id, state);
            output(Ok(PasskeyChallenge {
                ticket,
                options: challenge,
            }))
        }
        Err(e) => output(Err(ApiError::new(
            ErrorKind::Internal,
            format!("passkey login: {e:?}"),
        ))),
    }
}

#[derive(serde::Deserialize)]
struct PasskeyLoginFinish {
    ticket: String,
    credential: webauthn::PublicKeyCredential,
}

/// Finish a passkey login: verify the assertion, update the stored
/// credential's counters, and issue a token — the same response shape the
/// code grant returns.
#[post("/auth/passkey/login/finish", data = "<req>")]
async fn passkey_login_finish(
    server: &State<Arc<Server>>,
    wan: &State<webauthn::Webauthn>,
    req: Json<PasskeyLoginFinish>,
) -> ApiResult<api::AccessToken> {
    let req = req.into_inner();
    let Some((user_id, state)) = server.auth().take_authentication(&req.ticket) else {
        return output(Err(ApiError::new(
            ErrorKind::Unauthorized,
            "unknown or expired login challenge — start again",
        )));
    };
    match wan.finish_passkey_authentication(&req.credential, &state) {
        Ok(result) => {
            if let Err(e) = server.auth().update_passkey(&user_id, &result) {
                eprintln!("myco: passkey counter update failed: {e}");
            }
            match server.auth().issue_for(&user_id) {
                Some(issued) => output(Ok(api::AccessToken {
                    access_token: issued.access_token,
                    token_type: "bearer".into(),
                    expires_in: issued.expires_in_seconds,
                    user: issued.user.identity(),
                })),
                None => output(Err(ApiError::new(
                    ErrorKind::Unauthorized,
                    "this account is disabled",
                ))),
            }
        }
        Err(e) => output(Err(ApiError::new(
            ErrorKind::Unauthorized,
            format!("passkey assertion rejected: {e:?}"),
        ))),
    }
}
