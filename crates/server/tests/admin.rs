//! The admin surface: operator-only, complete lifecycle, and the one
//! refusal that protects everyone — the operator account cannot be locked
//! out, because the escape hatch must always have someone to sign in as.

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt as _;
use serde_json::Value;
use tower::ServiceExt as _;

/// Router + operator token (ada) + a second signed-in user (grace).
async fn fixture() -> (axum::Router, String, String) {
    use myco_server::auth::AuthStore;
    use std::sync::Arc;

    let auth = Arc::new(AuthStore::in_memory());
    auth.add_user("ada", "Ada Lovelace").unwrap();
    auth.add_user("grace", "Grace Hopper").unwrap();
    let ada = auth.issue_for("ada").unwrap().access_token;
    let grace = auth.issue_for("grace").unwrap().access_token;
    let pool = myco_instance::Pool::new();
    pool.register(Arc::new(common::CounterKind));
    (myco_server::router(pool, auth, "Ada"), ada, grace)
}

async fn send(router: &axum::Router, req: Request<Body>) -> (StatusCode, Value) {
    let response = router.clone().oneshot(req).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let body = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap()
    };
    (status, body)
}

fn req(method: &str, uri: &str, token: &str) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header("authorization", format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap()
}

#[tokio::test]
async fn the_admin_surface_is_operator_only() {
    let (router, _ada, grace) = fixture().await;

    let (status, _) = send(&router, req("GET", "/api/admin/users", &grace)).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    for uri in [
        "/api/admin/users/ada/code",
        "/api/admin/users/ada/disable",
        "/api/admin/users/ada/enable",
        "/api/admin/users/ada/revoke",
    ] {
        let (status, _) = send(&router, req("POST", uri, &grace)).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "POST {uri}");
    }
    let (status, _) = send(&router, req("DELETE", "/api/admin/users/ada", &grace)).await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // And nothing answers anonymously.
    let (status, _) = send(
        &router,
        Request::builder()
            .uri("/api/admin/users")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn the_listing_reports_users_sessions_and_the_operator_bit() {
    let (router, ada, _grace) = fixture().await;

    let (status, list) = send(&router, req("GET", "/api/admin/users", &ada)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(list["operator"], "ada");
    let users = list["users"].as_array().unwrap();
    let grace_row = users.iter().find(|u| u["id"] == "grace").unwrap();
    assert_eq!(grace_row["sessions"], 1);
    assert_eq!(grace_row["operator"], false);
    assert_eq!(grace_row["disabled"], false);
}

/// The full lifecycle over HTTP: mint → redeem; disable kills the session
/// and refuses minting; enable restores; revoke counts; remove forgets.
#[tokio::test]
async fn the_admin_actions_manage_a_user_end_to_end() {
    let (router, ada, grace) = fixture().await;

    // Mint for grace and redeem it — the minted code is real.
    let (status, minted) = send(&router, req("POST", "/api/admin/users/grace/code", &ada)).await;
    assert_eq!(status, StatusCode::OK);
    let code = minted["code"].as_str().unwrap();
    let (status, _) = send(
        &router,
        Request::builder()
            .method("POST")
            .uri("/api/auth/token")
            .header("content-type", "application/x-www-form-urlencoded")
            .body(Body::from(format!(
                "grant_type=code&username=grace&code={code}"
            )))
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Disable: her tokens die, and she cannot even be minted for.
    let (status, _) = send(&router, req("POST", "/api/admin/users/grace/disable", &ada)).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = send(&router, req("GET", "/api/whoami", &grace)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, _) = send(&router, req("POST", "/api/admin/users/grace/code", &ada)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // Enable, sign her in again, revoke: the session count is exact.
    let (status, _) = send(&router, req("POST", "/api/admin/users/grace/enable", &ada)).await;
    assert_eq!(status, StatusCode::OK);
    let (_, minted) = send(&router, req("POST", "/api/admin/users/grace/code", &ada)).await;
    let code = minted["code"].as_str().unwrap();
    let (_, _token) = send(
        &router,
        Request::builder()
            .method("POST")
            .uri("/api/auth/token")
            .header("content-type", "application/x-www-form-urlencoded")
            .body(Body::from(format!(
                "grant_type=code&username=grace&code={code}"
            )))
            .unwrap(),
    )
    .await;
    let (status, count) = send(&router, req("POST", "/api/admin/users/grace/revoke", &ada)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(count["affected"], 1);

    // Remove: forgotten entirely.
    let (status, _) = send(&router, req("DELETE", "/api/admin/users/grace", &ada)).await;
    assert_eq!(status, StatusCode::OK);
    let (_, list) = send(&router, req("GET", "/api/admin/users", &ada)).await;
    assert!(
        !list["users"]
            .as_array()
            .unwrap()
            .iter()
            .any(|u| u["id"] == "grace")
    );
}

/// The lockout invariant: the account the startup code signs in as can
/// never be disabled or removed, not even by itself.
#[tokio::test]
async fn the_operator_account_cannot_be_locked_out() {
    let (router, ada, _grace) = fixture().await;

    let (status, why) = send(&router, req("POST", "/api/admin/users/ada/disable", &ada)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(why["why"].as_str().unwrap().contains("operator"));
    let (status, _) = send(&router, req("DELETE", "/api/admin/users/ada", &ada)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // Still alive, still the operator.
    let (status, _) = send(&router, req("GET", "/api/admin/users", &ada)).await;
    assert_eq!(status, StatusCode::OK);
}
