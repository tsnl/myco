//! L2: the API server — the human's adapter to the bus, exactly as the agent
//! loop is the model's. Both translate an outside intelligence's native I/O
//! into `Pool` calls; the razor for everything in this crate is that it must
//! be either **authentication** or **translation**, or it belongs elsewhere.
//!
//! This is the skeleton: capability discovery (`GET /api/kinds`) over a
//! generic router, plus the two cross-origin-isolation headers every
//! response carries from day one (DESIGN.md DP-1: two headers now buy wasm
//! threads and `SharedArrayBuffer` whenever a renderer wants them — cheap
//! insurance, expensive retrofit).

use axum::http::HeaderValue;
use axum::routing::get;
use axum::{Json, Router, extract::State};
use myco_instance::Pool;
use tower_http::set_header::SetResponseHeaderLayer;

/// The `/api` router over a pool. The caller owns kind registration; the
/// server serves whatever the pool knows.
pub fn router(pool: Pool) -> Router {
    Router::new()
        .route("/api/kinds", get(kinds))
        .with_state(pool)
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
async fn kinds(State(pool): State<Pool>) -> Json<serde_json::Value> {
    Json(serde_json::to_value(pool.kinds()).expect("specs serialize"))
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

        let response = router(pool)
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
