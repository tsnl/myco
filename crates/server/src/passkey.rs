//! The WebAuthn ceremonies: standard challenge/response over the
//! webauthn-rs types, which serialize to the exact JSON
//! `PublicKeyCredential.parse*OptionsFromJSON` and `credential.toJSON()`
//! speak — a browser client passes them through untouched.
//!
//! Enrollment requires a signed-in session (a one-time code bootstraps it);
//! login requires only a username, and answers uniformly for "no such
//! user", "disabled", and "no passkeys enrolled" — the answer an attacker
//! gets is the same either way. Ceremony state lives in the store
//! ([`crate::auth::AuthStore`]), parked between the start and finish calls.

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use serde_json::{Value, json};
use webauthn_rs::prelude as webauthn;

use crate::config::PasskeySettings;
use crate::{App, Caller};

/// Build the relying party from `[passkeys]` in server.toml. With the
/// localhost defaults any port is allowed, so the direct server port and an
/// SSH tunnel's forwarded port both pass origin checks. A bad section fails
/// startup, not the first enrollment: the operator who mistyped it is at
/// the terminal *now*.
pub(crate) fn build_webauthn(settings: &PasskeySettings) -> Result<webauthn::Webauthn, String> {
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

fn refuse(status: StatusCode, why: impl Into<String>) -> (StatusCode, Json<Value>) {
    let error = match status {
        StatusCode::UNAUTHORIZED => "unauthorized",
        StatusCode::CONFLICT => "conflict",
        StatusCode::BAD_REQUEST => "bad_request",
        _ => "failed",
    };
    (status, Json(json!({"error": error, "why": why.into()})))
}

/// Start enrolling a passkey for the signed-in user.
pub(crate) async fn register_start(
    State(app): State<App>,
    caller: Caller,
) -> Result<Json<webauthn::CreationChallengeResponse>, (StatusCode, Json<Value>)> {
    let (id, name) = (caller.user.id.clone(), caller.user.name.clone());
    // Stable per-user handle, so authenticators overwrite their own old
    // credential for this account instead of piling up duplicates.
    let user_uuid = webauthn::Uuid::new_v5(&webauthn::Uuid::NAMESPACE_OID, id.as_bytes());
    let exclude: Vec<webauthn::CredentialID> = app
        .auth
        .passkeys_for(&id)
        .iter()
        .map(|p| p.cred_id().clone())
        .collect();
    match app
        .webauthn
        .start_passkey_registration(user_uuid, &id, &name, Some(exclude))
    {
        Ok((challenge, state)) => {
            app.auth.store_registration(&id, state);
            Ok(Json(challenge))
        }
        Err(e) => Err(refuse(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("passkey registration: {e:?}"),
        )),
    }
}

/// Finish enrollment with the browser's created credential.
pub(crate) async fn register_finish(
    State(app): State<App>,
    caller: Caller,
    Json(credential): Json<webauthn::RegisterPublicKeyCredential>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let id = caller.user.id.clone();
    let Some(state) = app.auth.take_registration(&id) else {
        return Err(refuse(
            StatusCode::CONFLICT,
            "no registration in progress (it may have expired — start again)",
        ));
    };
    match app
        .webauthn
        .finish_passkey_registration(&credential, &state)
    {
        Ok(passkey) => match app.auth.add_passkey(&id, passkey) {
            Ok(count) => Ok(Json(json!({"passkeys": count}))),
            Err(e) => Err(refuse(StatusCode::INTERNAL_SERVER_ERROR, e.to_string())),
        },
        Err(e) => Err(refuse(
            StatusCode::BAD_REQUEST,
            format!("passkey rejected: {e:?}"),
        )),
    }
}

#[derive(serde::Deserialize)]
pub(crate) struct LoginStart {
    username: String,
}

/// Start a passkey login. One uniform refusal covers "no such user",
/// "disabled", and "no passkeys enrolled".
pub(crate) async fn login_start(
    State(app): State<App>,
    Json(req): Json<LoginStart>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let no_passkey = || {
        refuse(
            StatusCode::UNAUTHORIZED,
            "no passkey available for this user",
        )
    };
    let user = app.auth.get(&req.username);
    let passkeys = match &user {
        Some(u) if !u.disabled => app.auth.passkeys_for(&u.id),
        _ => Vec::new(),
    };
    if passkeys.is_empty() {
        return Err(no_passkey());
    }
    match app.webauthn.start_passkey_authentication(&passkeys) {
        Ok((challenge, state)) => {
            let ticket = app
                .auth
                .store_authentication(&user.expect("checked above").id, state);
            Ok(Json(json!({"ticket": ticket, "options": challenge})))
        }
        Err(e) => Err(refuse(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("passkey login: {e:?}"),
        )),
    }
}

#[derive(serde::Deserialize)]
pub(crate) struct LoginFinish {
    ticket: String,
    credential: webauthn::PublicKeyCredential,
}

/// Finish a passkey login: verify the assertion, update the stored
/// credential's counters, and issue a token — the same response shape the
/// code grant returns.
pub(crate) async fn login_finish(
    State(app): State<App>,
    Json(req): Json<LoginFinish>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let Some((user_id, state)) = app.auth.take_authentication(&req.ticket) else {
        return Err(refuse(
            StatusCode::UNAUTHORIZED,
            "unknown or expired login challenge — start again",
        ));
    };
    match app
        .webauthn
        .finish_passkey_authentication(&req.credential, &state)
    {
        Ok(result) => {
            if let Err(e) = app.auth.update_passkey(&user_id, &result) {
                eprintln!("myco: passkey counter update failed: {e}");
            }
            match app.auth.issue_for(&user_id) {
                Some(issued) => {
                    crate::ensure_notifier(&app.pool, &issued.user.id);
                    Ok(Json(json!({
                    "access_token": issued.access_token,
                    "token_type": "bearer",
                        "expires_in": issued.expires_in_seconds,
                        "user": {"id": issued.user.id, "name": issued.user.name},
                    })))
                }
                None => Err(refuse(StatusCode::UNAUTHORIZED, "this account is disabled")),
            }
        }
        Err(e) => Err(refuse(
            StatusCode::UNAUTHORIZED,
            format!("passkey assertion rejected: {e:?}"),
        )),
    }
}
