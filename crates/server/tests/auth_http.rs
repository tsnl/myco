//! Authentication over the wire: there is no anonymous path through `/api`.
//! Every route sits behind the `Caller` extractor, tokens are obtained the
//! way a person obtains them (a one-time code redeemed at the token
//! endpoint), and refusals are uniform — the response never says which half
//! was wrong.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt as _;
use myco_instance::Pool;
use myco_server::auth::AuthStore;
use serde_json::Value;
use tower::ServiceExt as _;

fn fixture() -> (axum::Router, Arc<AuthStore>) {
    let auth = Arc::new(AuthStore::in_memory());
    auth.add_user("ada", "Ada Lovelace").unwrap();
    auth.add_user("grace", "Grace Hopper").unwrap();
    let pool = Pool::new();
    pool.register(Arc::new(myco_kind_tty::TtyKind));
    (myco_server::router(pool, Arc::clone(&auth), "ada"), auth)
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

fn form(uri: &str, body: String) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/x-www-form-urlencoded")
        .body(Body::from(body))
        .unwrap()
}

/// Redeem a code over HTTP, the way a client does it.
async fn sign_in(router: &axum::Router, auth: &AuthStore, id: &str) -> String {
    let minted = auth.mint_code(id).expect("mint");
    let (status, token) = send(
        router,
        form(
            "/api/auth/token",
            format!("grant_type=code&username={id}&code={}", minted.code),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "sign-in for {id}");
    assert_eq!(token["token_type"], "bearer");
    token["access_token"].as_str().unwrap().to_string()
}

fn bearing(req: Request<Body>, token: &str) -> Request<Body> {
    let (mut parts, body) = req.into_parts();
    parts
        .headers
        .insert("authorization", format!("Bearer {token}").parse().unwrap());
    Request::from_parts(parts, body)
}

#[tokio::test]
async fn no_route_answers_without_a_token() {
    let (router, _) = fixture();

    for uri in ["/api/kinds", "/api/models", "/api/instances", "/api/whoami"] {
        let (status, body) = send(
            &router,
            Request::builder().uri(uri).body(Body::empty()).unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "GET {uri}");
        assert_eq!(body["error"], "unauthorized");
    }
    for uri in [
        "/api/instances",
        "/api/instances/deadbeef/verbs/get",
        "/api/auth/logout",
    ] {
        let (status, _) = send(
            &router,
            Request::builder()
                .method("POST")
                .uri(uri)
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "POST {uri}");
    }
}

#[tokio::test]
async fn a_code_signs_in_once_and_identifies_its_owner() {
    let (router, auth) = fixture();
    let minted = auth.mint_code("ada").unwrap();

    // The code is bound to its user…
    let (status, _) = send(
        &router,
        form(
            "/api/auth/token",
            format!("grant_type=code&username=grace&code={}", minted.code),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // …redeems for its owner…
    let (status, token) = send(
        &router,
        form(
            "/api/auth/token",
            format!("grant_type=code&username=ada&code={}", minted.code),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let bearer = token["access_token"].as_str().unwrap().to_string();

    // …exactly once.
    let (status, _) = send(
        &router,
        form(
            "/api/auth/token",
            format!("grant_type=code&username=ada&code={}", minted.code),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    let (status, who) = send(
        &router,
        bearing(
            Request::builder()
                .uri("/api/whoami")
                .body(Body::empty())
                .unwrap(),
            &bearer,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(who["id"], "ada");
    assert_eq!(who["name"], "Ada Lovelace");
}

/// Unknown user, known user with no code, and a wrong guess answer
/// identically — the token endpoint never enumerates the roster.
#[tokio::test]
async fn bad_codes_get_one_uniform_answer() {
    let (router, _) = fixture();
    let mut answers = Vec::new();
    for user in ["nobody", "grace", "ada"] {
        let (status, body) = send(
            &router,
            form(
                "/api/auth/token",
                format!("grant_type=code&username={user}&code=AAAAA-AAAAA"),
            ),
        )
        .await;
        answers.push((status, body));
    }
    assert_eq!(answers[0], answers[1]);
    assert_eq!(answers[1], answers[2]);
}

#[tokio::test]
async fn retired_grants_are_refused_by_name() {
    let (router, _) = fixture();
    let (status, body) = send(
        &router,
        form(
            "/api/auth/token",
            "grant_type=password&username=ada&code=x".into(),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        body["why"].as_str().unwrap().contains("password"),
        "the refusal names the grant: {body}"
    );
}

#[tokio::test]
async fn logout_ends_the_session_server_side() {
    let (router, auth) = fixture();
    let bearer = sign_in(&router, &auth, "ada").await;

    let (status, _) = send(
        &router,
        bearing(
            Request::builder()
                .method("POST")
                .uri("/api/auth/logout")
                .body(Body::empty())
                .unwrap(),
            &bearer,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, _) = send(
        &router,
        bearing(
            Request::builder()
                .uri("/api/whoami")
                .body(Body::empty())
                .unwrap(),
            &bearer,
        ),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "the token must be dead after logout"
    );
}

/// `?token=` is accepted alongside the header — the concession the browser
/// APIs that cannot set headers will need — and it is a real check, not a
/// way around one.
#[tokio::test]
async fn a_query_token_is_checked_like_a_header() {
    let (router, auth) = fixture();
    let bearer = sign_in(&router, &auth, "ada").await;

    let (status, _) = send(
        &router,
        Request::builder()
            .uri(format!("/api/whoami?token={bearer}"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, _) = send(
        &router,
        Request::builder()
            .uri("/api/whoami?token=nope")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

/// The gateway attributes verbs to the token's owner: grace's terminal is
/// grace's seat, and ada — operator or not — is refused at the keyboard.
#[tokio::test]
async fn verbs_are_attributed_to_the_token_holder() {
    let (router, auth) = fixture();
    let ada = sign_in(&router, &auth, "ada").await;
    let grace = sign_in(&router, &auth, "grace").await;

    let (status, info) = send(
        &router,
        bearing(
            Request::builder()
                .method("POST")
                .uri("/api/instances")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_vec(&serde_json::json!({
                        "kind": "tty", "args": {"command": "cat"}
                    }))
                    .unwrap(),
                ))
                .unwrap(),
            &grace,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(info["creator"]["id"], "grace");
    let id = info["id"].as_str().unwrap();

    let (status, err) = send(
        &router,
        bearing(
            Request::builder()
                .method("POST")
                .uri(format!("/api/instances/{id}/verbs/input"))
                .header("content-type", "application/json")
                .body(Body::from(r#"{"data": "x"}"#))
                .unwrap(),
            &ada,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(err["error"], "not_driver");
    assert_eq!(err["held_by"]["id"], "grace");
}
