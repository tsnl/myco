//! L2: the API server — the human's adapter to the bus, exactly as the agent
//! loop is the model's. Both translate an outside intelligence's native I/O
//! into `Pool` calls; the razor for everything in this crate is that it must
//! be either **authentication** or **translation**, or it belongs elsewhere.
//!
//! The whole instance surface is three translation routes — create, list,
//! and the generic verb call — because the bus already is the API. There is
//! no route per operation and never will be: a new verb on a kind is a new
//! capability in `GET /api/kinds`, not a new route here (the four-places
//! problem v2's ledger executed).
//!
//! Authentication is the code grant (`POST /api/auth/token`,
//! `grant_type=code`) issuing bearer tokens; the [`Caller`] extractor turns
//! a token into a [`Principal`], so no route reaches the pool without an
//! identity to attribute its verbs to. `?token=` is accepted alongside the
//! header for the browser APIs that cannot set one (EventSource, WebSocket
//! — arriving with the watch PR).
//!
//! Every response carries the two cross-origin-isolation headers (DESIGN.md
//! DP-1): cheap insurance, expensive retrofit.

mod admin;
pub mod auth;
mod passkey;
pub mod roster;
mod util;
mod watch;

use std::sync::Arc;

use axum::extract::{FromRequestParts, Path, Query, State};
use axum::http::request::Parts;
use axum::http::{HeaderValue, StatusCode};
use axum::routing::{get, post};
use axum::{Form, Json, Router};
use myco_instance::{Pool, Principal, VerbError};
use serde_json::{Value, json};
use tower_http::set_header::SetResponseHeaderLayer;

use auth::AuthStore;

#[derive(Clone)]
pub(crate) struct App {
    pub(crate) pool: Pool,
    pub(crate) auth: Arc<AuthStore>,
    /// The roster's local user: possession of the process, spelled as an id.
    pub(crate) operator: String,
    /// The WebAuthn relying party, built once from `[passkeys]` at startup.
    pub(crate) webauthn: Arc<webauthn_rs::Webauthn>,
}

/// [`router_with`] under the default `[passkeys]` settings (localhost, any
/// port) — the always-valid configuration, so this cannot fail. Tests and
/// single-machine embedders use this; the binary goes through
/// [`router_with`] so a configured relying party is honored.
pub fn router(pool: Pool, auth: Arc<AuthStore>, operator: impl Into<String>) -> Router {
    router_with(pool, auth, operator, &roster::PasskeySettings::default())
        .expect("the default passkey settings always build")
}

/// The `/api` router over a pool and a credential store. The caller owns
/// kind registration and boot-time roster reconciliation; the server serves
/// whatever the pool knows, to whoever the store recognizes, with the
/// `/api/admin` surface reserved for `operator`. Fails only when the
/// `[passkeys]` section names an unusable relying party — at startup, where
/// the operator who mistyped it is still at the terminal.
pub fn router_with(
    pool: Pool,
    auth: Arc<AuthStore>,
    operator: impl Into<String>,
    passkeys: &roster::PasskeySettings,
) -> Result<Router, String> {
    let webauthn = Arc::new(passkey::build_webauthn(passkeys)?);
    Ok(Router::new()
        .route("/api/kinds", get(kinds))
        .route("/api/instances", post(create).get(list))
        .route("/api/instances/{id}/verbs/{verb}", post(call))
        .route("/api/instances/{id}/changed", get(watch::changed))
        .route("/api/ws", get(watch::ws))
        .route("/api/auth/token", post(token))
        .route("/api/auth/logout", post(logout))
        .route(
            "/api/auth/passkey/register/start",
            post(passkey::register_start),
        )
        .route(
            "/api/auth/passkey/register/finish",
            post(passkey::register_finish),
        )
        .route("/api/auth/passkey/login/start", post(passkey::login_start))
        .route(
            "/api/auth/passkey/login/finish",
            post(passkey::login_finish),
        )
        .route("/api/whoami", get(whoami))
        .route("/api/admin/users", get(admin::users))
        .route("/api/admin/users/{id}/code", post(admin::mint_code))
        .route("/api/admin/users/{id}/disable", post(admin::disable))
        .route("/api/admin/users/{id}/enable", post(admin::enable))
        .route("/api/admin/users/{id}/revoke", post(admin::revoke))
        .route(
            "/api/admin/users/{id}/passkeys/clear",
            post(admin::clear_passkeys),
        )
        .route(
            "/api/admin/users/{id}",
            axum::routing::delete(admin::remove),
        )
        .with_state(App {
            pool,
            auth,
            operator: operator.into(),
            webauthn,
        })
        .layer(SetResponseHeaderLayer::overriding(
            axum::http::HeaderName::from_static("cross-origin-opener-policy"),
            HeaderValue::from_static("same-origin"),
        ))
        .layer(SetResponseHeaderLayer::overriding(
            axum::http::HeaderName::from_static("cross-origin-embedder-policy"),
            HeaderValue::from_static("require-corp"),
        )))
}

// ---------------------------------------------------------------------------
// Authentication
// ---------------------------------------------------------------------------

/// An authenticated request, already resolved to a principal. Taking this as
/// a handler parameter *is* the auth check: a route that asks for it cannot
/// run without a live token.
pub(crate) struct Caller {
    pub(crate) user: auth::StoredUser,
}

impl Caller {
    fn principal(&self) -> Principal {
        Principal::Human(self.user.id.clone())
    }

    pub(crate) fn user_id(&self) -> &str {
        &self.user.id
    }
}

/// One message for missing, unknown, expired, and revoked alike: the
/// client's next step is the same in every case.
fn unauthorized() -> (StatusCode, Json<Value>) {
    (
        StatusCode::UNAUTHORIZED,
        Json(json!({"error": "unauthorized", "why": "missing or invalid bearer token"})),
    )
}

impl FromRequestParts<App> for Caller {
    type Rejection = (StatusCode, Json<Value>);

    async fn from_request_parts(parts: &mut Parts, app: &App) -> Result<Self, Self::Rejection> {
        // `Authorization: Bearer <token>`, or `?token=` for the browser
        // APIs that cannot set headers.
        let presented = parts
            .headers
            .get("authorization")
            .and_then(|h| h.to_str().ok())
            .and_then(|h| h.strip_prefix("Bearer "))
            .map(str::to_string)
            .or_else(|| {
                parts.uri.query().and_then(|q| {
                    q.split('&')
                        .find_map(|kv| kv.strip_prefix("token=").map(str::to_string))
                })
            });
        let Some(presented) = presented else {
            return Err(unauthorized());
        };
        match app.auth.authenticate_token(&presented) {
            Some(user) => Ok(Caller { user }),
            None => Err(unauthorized()),
        }
    }
}

/// The `POST /api/auth/token` form body: `grant_type=code` redeeming an
/// operator-minted one-time code. Tolerated when absent so
/// `curl -d username -d code` works; anything else is refused by name.
#[derive(serde::Deserialize)]
struct TokenRequest {
    username: String,
    #[serde(default)]
    code: String,
    grant_type: Option<String>,
}

/// RFC 6749's response shape, so a stock OAuth2 client can read it.
async fn token(
    State(app): State<App>,
    Form(form): Form<TokenRequest>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let grant = form.grant_type.as_deref().unwrap_or("code");
    if grant != "code" {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": "unsupported_grant_type",
                "why": format!("unsupported grant_type: {grant} (sign in with a one-time code)"),
            })),
        ));
    }
    match app.auth.redeem_code(&form.username, &form.code) {
        Ok(issued) => Ok(Json(json!({
            "access_token": issued.access_token,
            "token_type": "bearer",
            "expires_in": issued.expires_in_seconds,
            "user": {"id": issued.user.id, "name": issued.user.name},
        }))),
        Err(e) => Err((
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": "unauthorized", "why": e.to_string()})),
        )),
    }
}

/// Drop every token for the presented identity. Signing out must actually
/// end the session server-side, not just forget it client-side.
async fn logout(State(app): State<App>, caller: Caller) -> Json<Value> {
    app.auth.revoke_all_for(&caller.user.id);
    Json(json!({"id": caller.user.id, "name": caller.user.name}))
}

/// Who the presented token belongs to. Doubles as the client's login check.
async fn whoami(caller: Caller) -> Json<Value> {
    Json(json!({"id": caller.user.id, "name": caller.user.name}))
}

// ---------------------------------------------------------------------------
// Boot helpers (the binary's authentication plumbing)
// ---------------------------------------------------------------------------

/// Path of the local operator's bearer token: `$MYCO_HOME/operator.token`.
pub fn operator_token_path() -> Result<std::path::PathBuf, String> {
    Ok(util::data_root()?.join("operator.token"))
}

/// Write the operator token where local clients can read it, `0600`. It
/// dies with the server: tokens live only in memory, so the file goes stale
/// on restart and is rewritten.
pub fn write_operator_token(token: &str) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    let path = operator_token_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    std::fs::write(&path, token).map_err(|e| e.to_string())?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
        .map_err(|e| e.to_string())
}

/// The startup sign-in code: how a person first gets into a server that has
/// no signup and no passwords. Minted for the operator when they have no
/// passkey to sign in with — a fresh install — or on demand with
/// `--recover` (all passkeys lost). Returns the banner to print, or `None`
/// when nothing needs minting.
///
/// This is the whole trust rule in one function: possession of the process
/// is what makes someone the operator, so the code goes to the terminal the
/// server was started from and nowhere else.
pub fn startup_code(auth: &AuthStore, operator: &str, recover: bool) -> Option<String> {
    if !recover && !auth.passkeys_for(operator).is_empty() {
        return None;
    }
    let why = if recover {
        "--recover"
    } else {
        "no passkey enrolled yet"
    };
    Some(match auth.mint_code(operator) {
        Ok(minted) => format!(
            "myco: one-time sign-in code for {} ({why}): {}\n      \
             single use, {} minutes — POST /api/auth/token with grant_type=code, \
             then enroll a passkey\n      \
             (restart the server, or run with --recover, to mint another)",
            minted.user_id,
            minted.code,
            auth::CODE_TTL_MINUTES
        ),
        Err(e) => format!("myco: cannot mint a sign-in code: {e}"),
    })
}

// ---------------------------------------------------------------------------
// Translation: the verb gateway
// ---------------------------------------------------------------------------

/// Capability discovery: every registered kind's spec — verbs with their
/// flags, version, and the two default-read hints. Clients build themselves
/// from this instead of hardcoding kind knowledge.
async fn kinds(State(app): State<App>, _caller: Caller) -> Json<Value> {
    Json(serde_json::to_value(app.pool.kinds()).expect("specs serialize"))
}

#[derive(serde::Deserialize)]
struct CreateInstance {
    kind: String,
    /// Empty means unscoped; projects are a grouping key, not a namespace.
    #[serde(default)]
    project: String,
    #[serde(default)]
    title: String,
    /// The instance this one is created under, immutably. Absent means a
    /// root; an id nothing answers to is refused.
    #[serde(default)]
    parent: Option<String>,
    #[serde(default)]
    args: Value,
}

async fn create(
    State(app): State<App>,
    caller: Caller,
    Json(req): Json<CreateInstance>,
) -> Result<Json<Value>, (StatusCode, Json<VerbError>)> {
    let info = app
        .pool
        .create_under(
            &caller.principal(),
            &req.kind,
            &req.project,
            &req.title,
            req.args,
            req.parent.as_deref(),
        )
        .map_err(refusal)?;
    Ok(Json(serde_json::to_value(info).expect("info serializes")))
}

#[derive(serde::Deserialize)]
struct ListQuery {
    project: Option<String>,
}

async fn list(State(app): State<App>, _caller: Caller, Query(q): Query<ListQuery>) -> Json<Value> {
    Json(serde_json::to_value(app.pool.list(q.project.as_deref())).expect("infos serialize"))
}

/// The generic gateway: one route carries every verb of every kind,
/// `sys.*` included. The body is the verb's args verbatim (absent body =
/// null), the reply is the verb's result verbatim — the wire adds nothing
/// but authentication and status codes.
async fn call(
    State(app): State<App>,
    caller: Caller,
    Path((id, verb)): Path<(String, String)>,
    body: Option<Json<Value>>,
) -> Result<Json<Value>, (StatusCode, Json<VerbError>)> {
    let args = body.map(|Json(v)| v).unwrap_or(Value::Null);
    let result = app
        .pool
        .call(&caller.principal(), &id, &verb, args)
        .await
        .map_err(refusal)?;
    Ok(Json(result))
}

/// `VerbError` → HTTP, and the serde form (tagged `error`) *is* the wire
/// body, so the log's error names and the wire's never disagree.
pub(crate) fn refusal(e: VerbError) -> (StatusCode, Json<VerbError>) {
    let status = match &e {
        VerbError::UnknownKind { .. }
        | VerbError::UnknownInstance { .. }
        | VerbError::UnknownVerb { .. } => StatusCode::NOT_FOUND,
        VerbError::NotDriver { .. } | VerbError::Denied { .. } => StatusCode::FORBIDDEN,
        VerbError::BadArgs { .. } => StatusCode::BAD_REQUEST,
        VerbError::Gone => StatusCode::GONE,
        VerbError::Failed { .. } => StatusCode::INTERNAL_SERVER_ERROR,
    };
    (status, Json(e))
}
