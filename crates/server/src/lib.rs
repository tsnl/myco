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
//! Every response carries the two cross-origin-isolation headers (DESIGN.md
//! DP-1): two headers now buy wasm threads and `SharedArrayBuffer` whenever
//! a renderer wants them — cheap insurance, expensive retrofit.
//!
//! Authentication lands in a later PR of this stack; until then the router
//! acts as one configured local principal and binds loopback only.

pub mod auth;
mod util;

use axum::extract::{Path, Query, State};
use axum::http::{HeaderValue, StatusCode};
use axum::routing::{get, post};
use axum::{Json, Router};
use myco_instance::{Pool, Principal, VerbError};
use serde_json::Value;
use tower_http::set_header::SetResponseHeaderLayer;

#[derive(Clone)]
struct App {
    pool: Pool,
    /// The principal every request acts as, until bearer auth replaces it
    /// (stack PR 4). Loopback binding is what makes this interim state safe.
    local: Principal,
}

/// The `/api` router over a pool. The caller owns kind registration; the
/// server serves whatever the pool knows.
pub fn router(pool: Pool, local: Principal) -> Router {
    Router::new()
        .route("/api/kinds", get(kinds))
        .route("/api/instances", post(create).get(list))
        .route("/api/instances/{id}/verbs/{verb}", post(call))
        .with_state(App { pool, local })
        .layer(SetResponseHeaderLayer::overriding(
            axum::http::HeaderName::from_static("cross-origin-opener-policy"),
            HeaderValue::from_static("same-origin"),
        ))
        .layer(SetResponseHeaderLayer::overriding(
            axum::http::HeaderName::from_static("cross-origin-embedder-policy"),
            HeaderValue::from_static("require-corp"),
        ))
}

/// Capability discovery: every registered kind's spec — verbs with their
/// flags, version, and the two default-read hints. Clients build themselves
/// from this instead of hardcoding kind knowledge.
async fn kinds(State(app): State<App>) -> Json<Value> {
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
    #[serde(default)]
    args: Value,
}

async fn create(
    State(app): State<App>,
    Json(req): Json<CreateInstance>,
) -> Result<Json<Value>, (StatusCode, Json<VerbError>)> {
    let info = app
        .pool
        .create(&app.local, &req.kind, &req.project, &req.title, req.args)
        .map_err(refusal)?;
    Ok(Json(serde_json::to_value(info).expect("info serializes")))
}

#[derive(serde::Deserialize)]
struct ListQuery {
    project: Option<String>,
}

async fn list(State(app): State<App>, Query(q): Query<ListQuery>) -> Json<Value> {
    Json(serde_json::to_value(app.pool.list(q.project.as_deref())).expect("infos serialize"))
}

/// The generic gateway: one route carries every verb of every kind,
/// `sys.*` included. The body is the verb's args verbatim (absent body =
/// null), the reply is the verb's result verbatim — the wire adds nothing
/// but authentication (later) and status codes.
async fn call(
    State(app): State<App>,
    Path((id, verb)): Path<(String, String)>,
    body: Option<Json<Value>>,
) -> Result<Json<Value>, (StatusCode, Json<VerbError>)> {
    let args = body.map(|Json(v)| v).unwrap_or(Value::Null);
    let result = app
        .pool
        .call(&app.local, &id, &verb, args)
        .await
        .map_err(refusal)?;
    Ok(Json(result))
}

/// `VerbError` → HTTP, and the serde form (tagged `error`) *is* the wire
/// body, so the log's error names and the wire's never disagree.
fn refusal(e: VerbError) -> (StatusCode, Json<VerbError>) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt as _;
    use std::sync::Arc;
    use tower::ServiceExt as _;

    #[tokio::test]
    async fn kinds_lists_registered_specs_with_isolation_headers() {
        let pool = Pool::new();
        pool.register(Arc::new(myco_kind_tty::TtyKind));

        let response = router(pool, Principal::Human("ada".into()))
            .oneshot(
                Request::builder()
                    .uri("/api/kinds")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), 200);
        // The DP-1 insurance headers ride every response.
        assert_eq!(
            response.headers()["cross-origin-opener-policy"],
            "same-origin"
        );
        assert_eq!(
            response.headers()["cross-origin-embedder-policy"],
            "require-corp"
        );

        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let specs: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let tty = specs
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["kind"] == "tty")
            .expect("tty registered");
        assert_eq!(tty["version"], 1);
        assert!(
            tty["verbs"]
                .as_array()
                .unwrap()
                .iter()
                .any(|v| v["name"] == "input" && v["requires_driver"] == true)
        );
    }
}
