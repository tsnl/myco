//! `/api/admin` — everything v2's admin panel drove, operator-only. The
//! operator is the roster's local user (possession of the machine is what
//! makes someone the operator; the [`Operator`] extractor is that rule at
//! the HTTP layer), and the server process remains the only writer of the
//! credential files.
//!
//! The one refusal that protects everyone: the operator account cannot be
//! disabled or removed — the escape hatch must always have someone to sign
//! in as.

use axum::Json;
use axum::extract::{FromRequestParts, Path, State};
use axum::http::StatusCode;
use axum::http::request::Parts;
use serde_json::{Value, json};

use crate::{App, Caller};

/// An authenticated request from the operator. Everyone else gets one
/// uniform refusal.
pub(crate) struct Operator;

impl FromRequestParts<App> for Operator {
    type Rejection = (StatusCode, Json<Value>);

    async fn from_request_parts(parts: &mut Parts, app: &App) -> Result<Self, Self::Rejection> {
        let caller = Caller::from_request_parts(parts, app).await?;
        if caller.user_id() == app.operator {
            Ok(Operator)
        } else {
            Err((
                StatusCode::FORBIDDEN,
                Json(json!({"error": "forbidden", "why": "operator only"})),
            ))
        }
    }
}

/// The roster as the operator sees it: who exists, their state, their live
/// sessions. (Passkey counts join this row when the passkey PRs land.)
pub(crate) async fn users(State(app): State<App>, _op: Operator) -> Json<Value> {
    let sessions: std::collections::HashMap<String, usize> =
        app.auth.session_counts().into_iter().collect();
    let rows: Vec<Value> = app
        .auth
        .users()
        .into_iter()
        .map(|u| {
            json!({
                "id": u.id,
                "name": u.name,
                "disabled": u.disabled,
                "sessions": sessions.get(&u.id).copied().unwrap_or(0),
                "operator": u.id == app.operator,
            })
        })
        .collect();
    Json(json!({"operator": app.operator, "users": rows}))
}

fn auth_refusal(e: crate::auth::AuthError) -> (StatusCode, Json<Value>) {
    let status = match &e {
        crate::auth::AuthError::UnknownUser(_) => StatusCode::NOT_FOUND,
        _ => StatusCode::BAD_REQUEST,
    };
    (
        status,
        Json(json!({"error": "refused", "why": e.to_string()})),
    )
}

/// Refuse the one action that could orphan the server: locking out the
/// identity that mints codes.
fn not_the_operator(app: &App, id: &str) -> Result<(), (StatusCode, Json<Value>)> {
    if crate::auth::normalize_id(id) == app.operator {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": "refused",
                "why": "the operator account cannot be disabled or removed",
            })),
        ));
    }
    Ok(())
}

/// Mint a one-time sign-in code. Minting must happen against the live
/// server: the code is redeemed from this process's memory, so a file could
/// never carry it.
pub(crate) async fn mint_code(
    State(app): State<App>,
    _op: Operator,
    Path(id): Path<String>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let minted = app.auth.mint_code(&id).map_err(auth_refusal)?;
    Ok(Json(json!({
        "username": minted.user_id,
        "code": minted.code,
        "expires_at": minted.expires_at.to_rfc3339(),
    })))
}

pub(crate) async fn disable(
    State(app): State<App>,
    _op: Operator,
    Path(id): Path<String>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    not_the_operator(&app, &id)?;
    app.auth.set_disabled(&id, true).map_err(auth_refusal)?;
    Ok(Json(Value::Null))
}

pub(crate) async fn enable(
    State(app): State<App>,
    _op: Operator,
    Path(id): Path<String>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    app.auth.set_disabled(&id, false).map_err(auth_refusal)?;
    Ok(Json(Value::Null))
}

/// End every live session for a user without touching their credentials.
pub(crate) async fn revoke(
    State(app): State<App>,
    _op: Operator,
    Path(id): Path<String>,
) -> Json<Value> {
    Json(json!({"affected": app.auth.revoke_all_for(&id)}))
}

/// Forget a user's credentials entirely. If they are still in the roster,
/// the next restart re-adds the bare name — exactly "start them over with a
/// fresh code".
pub(crate) async fn remove(
    State(app): State<App>,
    _op: Operator,
    Path(id): Path<String>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    not_the_operator(&app, &id)?;
    app.auth.remove_user(&id).map_err(auth_refusal)?;
    Ok(Json(Value::Null))
}
