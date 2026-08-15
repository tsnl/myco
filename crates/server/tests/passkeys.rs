//! The passkey ceremonies end to end against a software authenticator: a
//! code-bootstrapped session enrolls, the passkey signs its user in, a
//! ticket cannot be replayed, unknown users get the same answer as users
//! without passkeys, credentials survive the restart that voids every
//! token — and the startup banner goes quiet once a passkey exists.
//!
//! The requests use origin `http://localhost:7773` against the default
//! `[passkeys]` settings (origin `http://localhost`), pinning the
//! `allow_any_port` behavior the SSH-tunnel story depends on.

mod common;

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt as _;
use myco_instance::Pool;
use myco_server::auth::AuthStore;
use serde_json::{Value, json};
use tower::ServiceExt as _;
use webauthn_authenticator_rs::{WebauthnAuthenticator, softtoken::SoftToken};
use webauthn_rs::prelude::{CreationChallengeResponse, RequestChallengeResponse, Url};

fn router_over(auth: &Arc<AuthStore>) -> axum::Router {
    let pool = Pool::new();
    pool.register(Arc::new(common::CounterKind));
    myco_server::router(pool, Arc::clone(auth), "ada")
}

fn fixture() -> (axum::Router, Arc<AuthStore>) {
    let auth = Arc::new(AuthStore::in_memory());
    auth.add_user("ada", "Ada Lovelace").unwrap();
    auth.add_user("grace", "Grace Hopper").unwrap();
    (router_over(&auth), auth)
}

/// A browser-shaped authenticator: a soft token plus the origin it browses.
fn browser() -> (WebauthnAuthenticator<SoftToken>, Url) {
    let (token, _cert) = SoftToken::new(true).expect("soft token");
    (
        WebauthnAuthenticator::new(token),
        Url::parse("http://localhost:7773").expect("origin"),
    )
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

fn post_json(uri: &str, body: Value, token: Option<&str>) -> Request<Body> {
    let mut builder = Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json");
    if let Some(token) = token {
        builder = builder.header("authorization", format!("Bearer {token}"));
    }
    builder
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap()
}

async fn sign_in_with_code(router: &axum::Router, auth: &AuthStore, id: &str) -> String {
    let minted = auth.mint_code(id).expect("mint");
    let (status, token) = send(
        router,
        Request::builder()
            .method("POST")
            .uri("/api/auth/token")
            .header("content-type", "application/x-www-form-urlencoded")
            .body(Body::from(format!(
                "grant_type=code&username={id}&code={}",
                minted.code
            )))
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "code sign-in for {id}");
    token["access_token"].as_str().unwrap().to_string()
}

/// Run the enrollment ceremony over HTTP for the bearer of `token`.
async fn enroll(
    router: &axum::Router,
    authenticator: &mut WebauthnAuthenticator<SoftToken>,
    origin: &Url,
    token: &str,
) {
    let (status, challenge) = send(
        router,
        post_json("/api/auth/passkey/register/start", Value::Null, Some(token)),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let challenge: CreationChallengeResponse = serde_json::from_value(challenge).unwrap();
    let created = authenticator
        .do_registration(origin.clone(), challenge)
        .expect("softtoken registration");
    let (status, enrolled) = send(
        router,
        post_json(
            "/api/auth/passkey/register/finish",
            serde_json::to_value(&created).unwrap(),
            Some(token),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(enrolled["passkeys"], 1);
}

/// The full story on one server: bootstrap by code, enroll, sign in with
/// the passkey, and the answers that must stay uniform stay uniform.
#[tokio::test]
async fn a_passkey_enrolls_and_signs_in_via_the_ceremonies() {
    let (router, auth) = fixture();
    let (mut authenticator, origin) = browser();

    // Enrollment requires a session; a one-time code bootstraps it.
    let ada_token = sign_in_with_code(&router, &auth, "ada").await;
    enroll(&router, &mut authenticator, &origin, &ada_token).await;

    // With a passkey on file the startup banner has nothing to say —
    // unless --recover demands a code anyway.
    assert!(myco_server::startup_code(&auth, "ada", false).is_none());
    let banner = myco_server::startup_code(&auth, "ada", true).expect("--recover always mints");
    assert!(banner.contains("--recover"), "{banner}");

    // Sign in with it: challenge → assertion → token that speaks as ada.
    let (status, body) = send(
        &router,
        post_json(
            "/api/auth/passkey/login/start",
            json!({"username": "ada"}),
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let ticket = body["ticket"].as_str().expect("ticket").to_string();
    let options: RequestChallengeResponse =
        serde_json::from_value(body["options"].clone()).unwrap();
    let asserted = authenticator
        .do_authentication(origin, options)
        .expect("softtoken assertion");
    let (status, issued) = send(
        &router,
        post_json(
            "/api/auth/passkey/login/finish",
            json!({"ticket": ticket, "credential": asserted}),
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(issued["token_type"], "bearer");
    let (status, who) = send(
        &router,
        Request::builder()
            .uri("/api/whoami")
            .header(
                "authorization",
                format!("Bearer {}", issued["access_token"].as_str().unwrap()),
            )
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(who["id"], "ada");

    // A ticket is single use — replaying the finished ceremony fails.
    let (status, _) = send(
        &router,
        post_json(
            "/api/auth/passkey/login/finish",
            json!({"ticket": ticket, "credential": asserted}),
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // No enumeration: a user without passkeys and a user who does not
    // exist answer identically.
    let mut answers = Vec::new();
    for name in ["grace", "nobody-here"] {
        let (status, body) = send(
            &router,
            post_json(
                "/api/auth/passkey/login/start",
                json!({"username": name}),
                None,
            ),
        )
        .await;
        answers.push((status, body));
    }
    assert_eq!(answers[0], answers[1], "uniform answer, no enumeration");
}

/// The point of passkeys over tokens: credentials persist in
/// `passkeys.json`, so the restart that voids every token and code by
/// design does not cost anyone their sign-in method.
#[tokio::test]
async fn passkeys_survive_a_server_restart() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("auth.json");
    let (mut authenticator, origin) = browser();

    // First life: bootstrap by code, enroll.
    {
        let auth = Arc::new(AuthStore::open(&path).expect("open"));
        auth.add_user("ada", "Ada Lovelace").unwrap();
        let router = router_over(&auth);
        let token = sign_in_with_code(&router, &auth, "ada").await;
        enroll(&router, &mut authenticator, &origin, &token).await;
    }

    // Second life: a fresh store from the same files — every token is
    // gone, the passkey is not, and it signs ada in.
    let auth = Arc::new(AuthStore::open(&path).expect("reopen"));
    assert_eq!(auth.passkey_counts().get("ada"), Some(&1));
    assert!(
        myco_server::startup_code(&auth, "ada", false).is_none(),
        "a restart no longer volunteers a code"
    );
    let router = router_over(&auth);
    let (status, body) = send(
        &router,
        post_json(
            "/api/auth/passkey/login/start",
            json!({"username": "ada"}),
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let options: RequestChallengeResponse =
        serde_json::from_value(body["options"].clone()).unwrap();
    let asserted = authenticator
        .do_authentication(origin, options)
        .expect("assertion");
    let (status, issued) = send(
        &router,
        post_json(
            "/api/auth/passkey/login/finish",
            json!({"ticket": body["ticket"], "credential": asserted}),
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(issued["user"]["id"], "ada");
}

/// "Forget a user" forgets their passkeys too: after an admin remove, the
/// re-added roster name answers the login-start probe exactly like a user
/// who never enrolled — and the admin listing counts credentials while
/// they exist.
#[tokio::test]
async fn removing_a_user_forgets_their_passkeys() {
    let (router, auth) = fixture();
    let (mut authenticator, origin) = browser();

    let grace_token = sign_in_with_code(&router, &auth, "grace").await;
    enroll(&router, &mut authenticator, &origin, &grace_token).await;

    // The admin listing shows the credential while it exists.
    let ada_token = sign_in_with_code(&router, &auth, "ada").await;
    let (status, listing) = send(
        &router,
        Request::builder()
            .uri("/api/admin/users")
            .header("authorization", format!("Bearer {ada_token}"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let grace_row = listing["users"]
        .as_array()
        .unwrap()
        .iter()
        .find(|u| u["id"] == "grace")
        .expect("grace listed");
    assert_eq!(grace_row["passkeys"], 1);

    let (status, _) = send(
        &router,
        Request::builder()
            .method("DELETE")
            .uri("/api/admin/users/grace")
            .header("authorization", format!("Bearer {ada_token}"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(auth.passkey_counts().get("grace"), None);

    // Re-added (as roster reconciliation would on restart), she is
    // indistinguishable from someone who never enrolled.
    auth.add_user("grace", "Grace Hopper").unwrap();
    let (status, _) = send(
        &router,
        post_json(
            "/api/auth/passkey/login/start",
            json!({"username": "grace"}),
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

/// The operator's escape hatch for a lost authenticator, without removing
/// the account: clear the passkeys, and the login probe answers like a
/// fresh user while sessions live on.
#[tokio::test]
async fn the_admin_can_clear_a_users_passkeys() {
    let (router, auth) = fixture();
    let (mut authenticator, origin) = browser();

    let grace_token = sign_in_with_code(&router, &auth, "grace").await;
    enroll(&router, &mut authenticator, &origin, &grace_token).await;

    let ada_token = sign_in_with_code(&router, &auth, "ada").await;
    let (status, cleared) = send(
        &router,
        post_json(
            "/api/admin/users/grace/passkeys/clear",
            Value::Null,
            Some(&ada_token),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(cleared["affected"], 1);

    let (status, _) = send(
        &router,
        post_json(
            "/api/auth/passkey/login/start",
            json!({"username": "grace"}),
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // Clearing credentials is not revoking sessions: the token minted
    // before the clear still works.
    let (status, who) = send(
        &router,
        Request::builder()
            .uri("/api/whoami")
            .header("authorization", format!("Bearer {grace_token}"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(who["id"], "grace");
}

/// The bootstrap path: a fresh store (no passkeys for the operator) mints
/// a banner code, and that code actually signs the operator in over HTTP.
#[tokio::test]
async fn the_startup_code_bootstraps_a_fresh_server() {
    let (router, auth) = fixture();

    let banner = myco_server::startup_code(&auth, "ada", false).expect("fresh server mints");
    assert!(banner.contains("ada"), "{banner}");
    assert!(banner.contains("no passkey enrolled yet"), "{banner}");
    let code = banner
        .split_whitespace()
        .find(|w| w.len() == 11 && w.as_bytes()[5] == b'-')
        .expect("banner carries the code");

    let (status, issued) = send(
        &router,
        Request::builder()
            .method("POST")
            .uri("/api/auth/token")
            .header("content-type", "application/x-www-form-urlencoded")
            .body(Body::from(format!(
                "grant_type=code&username=ada&code={code}"
            )))
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{banner}");
    assert_eq!(issued["user"]["id"], "ada");
}
