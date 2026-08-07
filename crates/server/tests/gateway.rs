//! The generic verb gateway, end to end over HTTP: create, list, call —
//! including `sys.*` — with the serde error form as the wire body. One
//! dynamic route carries every kind; these tests use a local counter kind
//! so nothing here depends on a shell. (Auth contract details live in
//! auth_http.rs; here every request simply bears ada's token.)

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt as _;
use serde_json::{Value, json};
use tower::ServiceExt as _;

/// Router plus a live bearer token for ada.
fn app() -> (axum::Router, String) {
    let (router, _pool, token) = common::counter_app();
    (router, token)
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

fn post(token: &str, uri: &str, body: Value) -> Request<Body> {
    let builder = Request::builder()
        .method("POST")
        .uri(uri)
        .header("authorization", format!("Bearer {token}"));
    if body.is_null() {
        builder.body(Body::empty()).unwrap()
    } else {
        builder
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap()
    }
}

fn get(token: &str, uri: &str) -> Request<Body> {
    Request::builder()
        .uri(uri)
        .header("authorization", format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap()
}

#[tokio::test]
async fn create_call_and_read_through_one_route() {
    let (app, tok) = app();

    let (status, info) = send(
        &app,
        post(
            &tok,
            "/api/instances",
            json!({"kind": "counter", "project": "alpha", "args": {"start": 40}}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let id = info["id"].as_str().unwrap();
    assert_eq!(info["kind"], "counter");
    assert_eq!(info["driver"]["id"], "ada", "the creator drives");

    let (status, n) = send(
        &app,
        post(
            &tok,
            &format!("/api/instances/{id}/verbs/incr"),
            json!({"by": 2}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(n, json!(42));

    // A read with no body: absent args are null.
    let (status, n) = send(
        &app,
        post(&tok, &format!("/api/instances/{id}/verbs/get"), Value::Null),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(n, json!(42));

    // sys.* rides the same route — the gateway adds nothing per verb.
    let (status, meta) = send(
        &app,
        post(
            &tok,
            &format!("/api/instances/{id}/verbs/sys.meta"),
            Value::Null,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(meta["project"], "alpha");
    assert_eq!(meta["watermark"], 1);
}

#[tokio::test]
async fn listing_scopes_by_project() {
    let (app, tok) = app();
    for project in ["alpha", "alpha", "beta"] {
        let (status, _) = send(
            &app,
            post(
                &tok,
                "/api/instances",
                json!({"kind": "counter", "project": project}),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
    }

    let (_, all) = send(&app, get(&tok, "/api/instances")).await;
    assert_eq!(all.as_array().unwrap().len(), 3);
    let (_, alpha) = send(&app, get(&tok, "/api/instances?project=alpha")).await;
    assert_eq!(alpha.as_array().unwrap().len(), 2);
}

/// Refusals arrive as the error's serde form with a faithful status — the
/// wire body and the verb log can never disagree about what went wrong.
#[tokio::test]
async fn refusals_map_to_status_codes_with_the_serde_body() {
    let (app, tok) = app();

    let (status, err) = send(&app, post(&tok, "/api/instances", json!({"kind": "nope"}))).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(err["error"], "unknown_kind");

    let (status, err) = send(
        &app,
        post(&tok, "/api/instances/nope/verbs/get", Value::Null),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(err["error"], "unknown_instance");

    let (_, info) = send(
        &app,
        post(&tok, "/api/instances", json!({"kind": "counter"})),
    )
    .await;
    let id = info["id"].as_str().unwrap();
    let (status, err) = send(
        &app,
        post(
            &tok,
            &format!("/api/instances/{id}/verbs/explode"),
            Value::Null,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(err["error"], "unknown_verb");

    // Removal is a verb like everything else; afterwards the id is unknown.
    let (status, _) = send(
        &app,
        post(
            &tok,
            &format!("/api/instances/{id}/verbs/sys.remove"),
            Value::Null,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = send(
        &app,
        post(&tok, &format!("/api/instances/{id}/verbs/get"), Value::Null),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
