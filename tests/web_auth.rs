//! HTTP authentication, exercised against the real Rocket instance.
//!
//! The claim under test is structural: there is no anonymous path through
//! `/api`. Every route is mounted behind the `Caller` guard, so a request
//! without a valid access token cannot reach the runtime — and one that *is*
//! valid has its writes attributed to the token's owner, not to whoever
//! happens to be running the process.
//!
//! Tokens are obtained the way a client obtains them: the OAuth 2.0 password
//! grant at `POST /api/auth/token`.

use std::sync::Arc;

use myco::auth::AuthStore;
use myco::models::{GenerateOutput, GenerativeModel};
use myco::server::Server;
use myco::test_support::ScriptedModel;
use myco_api::{Author, EntryBody};
use myco_api::{Content, TurnEndReason};
use rocket::http::{ContentType, Header, Status};
use rocket::local::asynchronous::Client;

const CONFIG_TOML: &str = r#"
model = "fake"

[gateways.g]
protocol = "openai-completions"
base_url = "http://127.0.0.1:9/v1"
auth = "dummy"

[models.fake]
gateway = "g"
context_window = 100000
"#;

const ADA_PASSWORD: &str = "ada's long enough password";
const GRACE_PASSWORD: &str = "grace's long enough password";

/// The roster names who exists; credentials come from the store.
const ROSTER_TOML: &str = r#"
[[users]]
id = "ada"
name = "Ada Lovelace"

[[users]]
id = "grace"
name = "Grace Hopper"
"#;

/// A server whose credential store is in memory only — the suite must never
/// touch a real `auth.json`.
fn test_server() -> Arc<Server> {
    let config = myco::config::Config::resolve_with(
        Default::default(),
        |k| (k == "USER").then(|| "ada".to_string()),
        |_, _| myco::config::parse_file_config_str(CONFIG_TOML),
        |_| myco::config::parse_file_roster_str(ROSTER_TOML),
        || Ok(Vec::new()),
        |_| Err("no auth files in tests".into()),
    )
    .expect("test config resolves");
    // Token work factor: these tests are about the HTTP contract, not the KDF
    // (which `myco-auth` covers on its own).
    let auth = Arc::new(AuthStore::in_memory().with_work_factor(1));
    let server = Server::with_model_factory_and_auth(
        config,
        Box::new(|_, _, _, _| {
            Ok(ScriptedModel::new(vec![GenerateOutput {
                content: vec![Content::Text { text: "ok".into() }],
                tool_uses: vec![],
                turn_end_reason: TurnEndReason::EndTurn,
                usage: None,
            }]) as Arc<dyn GenerativeModel>)
        }),
        auth,
    );
    // Roster users exist by construction; give two of them passwords.
    server
        .auth()
        .set_password("ada", ADA_PASSWORD)
        .expect("chpass");
    server
        .auth()
        .set_password("grace", GRACE_PASSWORD)
        .expect("chpass");
    server
}

async fn client() -> Client {
    let figment = rocket::Config::figment().merge(("log_level", "off"));
    Client::tracked(myco::web::rocket(test_server(), figment))
        .await
        .expect("rocket builds")
}

fn bearer(token: &str) -> Header<'static> {
    Header::new("Authorization", format!("Bearer {token}"))
}

/// The password grant, as a client performs it.
async fn login(c: &Client, id: &str, password: &str) -> Result<String, Status> {
    let resp = c
        .post("/api/auth/token")
        .header(ContentType::Form)
        .body(format!(
            "grant_type=password&username={id}&password={password}"
        ))
        .dispatch()
        .await;
    if resp.status() != Status::Ok {
        return Err(resp.status());
    }
    let token: myco_api::AccessToken = resp.into_json().await.expect("token response");
    assert_eq!(token.token_type, "bearer");
    assert!(token.expires_in > 0);
    Ok(token.access_token)
}

/// Every route, with no credential at all. None may answer.
#[tokio::test]
async fn no_route_answers_without_a_token() {
    let _home = myco::test_support::temp_home("web-auth-none");
    let c = client().await;

    for path in [
        "/api/sessions",
        "/api/sessions/deadbeef",
        "/api/sessions/deadbeef/poll",
        "/api/sessions/deadbeef/events",
        "/api/models",
        "/api/whoami",
    ] {
        let resp = c.get(path).dispatch().await;
        assert_eq!(resp.status(), Status::Unauthorized, "GET {path}");
    }
    for path in [
        "/api/sessions",
        "/api/sessions/deadbeef/messages",
        "/api/sessions/deadbeef/cancel",
        "/api/sessions/deadbeef/compact",
    ] {
        let resp = c.post(path).dispatch().await;
        assert_eq!(resp.status(), Status::Unauthorized, "POST {path}");
    }
    let resp = c.delete("/api/sessions/deadbeef/live").dispatch().await;
    assert_eq!(resp.status(), Status::Unauthorized);
    let resp = c.patch("/api/sessions/deadbeef").dispatch().await;
    assert_eq!(resp.status(), Status::Unauthorized);
}

/// A wrong token is rejected, and so is a *prefix* of a real one — the
/// comparison must not accept a partial match.
#[tokio::test]
async fn a_bad_token_is_rejected() {
    let _home = myco::test_support::temp_home("web-auth-bad");
    let c = client().await;

    let live = login(&c, "ada", ADA_PASSWORD).await.expect("login");
    for token in ["", "wrong", &live[..16], &format!("{live}x")] {
        let resp = c.get("/api/whoami").header(bearer(token)).dispatch().await;
        assert_eq!(resp.status(), Status::Unauthorized, "token {token:?}");
    }
    // The scheme matters too: a bare token without `Bearer ` is not a
    // credential this server accepts.
    let resp = c
        .get("/api/whoami")
        .header(Header::new("Authorization", live.clone()))
        .dispatch()
        .await;
    assert_eq!(resp.status(), Status::Unauthorized);
}

#[tokio::test]
async fn a_token_identifies_its_owner() {
    let _home = myco::test_support::temp_home("web-auth-whoami");
    let c = client().await;

    for (password, id, name) in [
        (ADA_PASSWORD, "ada", "Ada Lovelace"),
        (GRACE_PASSWORD, "grace", "Grace Hopper"),
    ] {
        let token = login(&c, id, password).await.expect("login");
        let resp = c.get("/api/whoami").header(bearer(&token)).dispatch().await;
        assert_eq!(resp.status(), Status::Ok);
        let who: myco_api::Identity = resp.into_json().await.expect("identity");
        assert_eq!(who.id, id);
        assert_eq!(who.name, name);
    }
}

/// The point of authenticating at all: the entry records the *caller*, not the
/// operator who started the process. `ada` is the local user here, so a
/// message posted with grace's token proves the attribution follows the token.
#[tokio::test]
async fn a_posted_message_is_attributed_to_the_token_holder() {
    let _home = myco::test_support::temp_home("web-auth-attrib");
    let c = client().await;
    let ada = login(&c, "ada", ADA_PASSWORD).await.expect("login");
    let grace = login(&c, "grace", GRACE_PASSWORD).await.expect("login");

    let created = c
        .post("/api/sessions")
        .header(bearer(&grace))
        .json(&myco_api::CreateSession {
            model: None,
            parent_session: None,
            fork: false,
        })
        .dispatch()
        .await;
    assert_eq!(created.status(), Status::Ok);
    let summary: myco_api::SessionSummary = created.into_json().await.expect("summary");

    let posted = c
        .post(format!("/api/sessions/{}/messages", summary.id))
        .header(bearer(&grace))
        .json(&myco_api::PostMessage {
            text: "hello".into(),
        })
        .dispatch()
        .await;
    assert_eq!(posted.status(), Status::Ok);

    // Poll until the turn lands, then read the author off the user entry.
    for _ in 0..200 {
        let resp = c
            .get(format!("/api/sessions/{}/poll?since=0", summary.id))
            .header(bearer(&ada))
            .dispatch()
            .await;
        let poll: myco_api::Poll = resp.into_json().await.expect("poll");
        if !poll.busy
            && let Some(entry) = poll
                .entries
                .iter()
                .find(|e| matches!(e.body, EntryBody::User { .. }))
        {
            match &entry.author {
                Author::User { id, name } => {
                    assert_eq!(id, "grace", "the token's owner authored the entry");
                    assert_eq!(name, "Grace Hopper");
                }
                other => panic!("expected a user author, got {other:?}"),
            }
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    panic!("turn never settled");
}

/// `EventSource` cannot send headers, so the SSE route accepts `?token=`.
/// That must be a real check, not a way around one.
#[tokio::test]
async fn the_event_stream_accepts_a_query_token_but_still_checks_it() {
    let _home = myco::test_support::temp_home("web-auth-sse");
    let c = client().await;
    let ada = login(&c, "ada", ADA_PASSWORD).await.expect("login");

    let created = c
        .post("/api/sessions")
        .header(bearer(&ada))
        .json(&myco_api::CreateSession {
            model: None,
            parent_session: None,
            fork: false,
        })
        .dispatch()
        .await;
    let summary: myco_api::SessionSummary = created.into_json().await.expect("summary");

    let bad = c
        .get(format!("/api/sessions/{}/events?token=nope", summary.id))
        .dispatch()
        .await;
    assert_eq!(bad.status(), Status::Unauthorized);

    let ok = c
        .get(format!("/api/sessions/{}/events?token={ada}", summary.id))
        .dispatch()
        .await;
    assert_eq!(ok.status(), Status::Ok);
}

/// The grant itself: right credentials succeed, wrong ones do not, and the
/// failure never says which half was wrong.
#[tokio::test]
async fn the_password_grant_accepts_only_correct_credentials() {
    let _home = myco::test_support::temp_home("web-auth-grant");
    let c = client().await;

    assert!(login(&c, "ada", ADA_PASSWORD).await.is_ok());
    assert_eq!(
        login(&c, "ada", "wrong password entirely")
            .await
            .unwrap_err(),
        Status::Unauthorized
    );
    assert_eq!(
        login(&c, "nobody", ADA_PASSWORD).await.unwrap_err(),
        Status::Unauthorized
    );
    // Ada's password must not work for Grace.
    assert_eq!(
        login(&c, "grace", ADA_PASSWORD).await.unwrap_err(),
        Status::Unauthorized
    );

    // A grant type we do not implement is rejected outright, so adding one
    // later cannot be mistaken for this one.
    let resp = c
        .post("/api/auth/token")
        .header(ContentType::Form)
        .body(format!(
            "grant_type=client_credentials&username=ada&password={ADA_PASSWORD}"
        ))
        .dispatch()
        .await;
    assert_eq!(resp.status(), Status::BadRequest);
}

/// Signing out has to end the session server-side. A client that merely
/// forgets its token leaves a live credential behind.
#[tokio::test]
async fn logging_out_invalidates_the_token() {
    let _home = myco::test_support::temp_home("web-auth-logout");
    let c = client().await;
    let ada = login(&c, "ada", ADA_PASSWORD).await.expect("login");

    assert_eq!(
        c.get("/api/whoami")
            .header(bearer(&ada))
            .dispatch()
            .await
            .status(),
        Status::Ok
    );
    assert_eq!(
        c.post("/api/auth/logout")
            .header(bearer(&ada))
            .dispatch()
            .await
            .status(),
        Status::Ok
    );
    assert_eq!(
        c.get("/api/whoami")
            .header(bearer(&ada))
            .dispatch()
            .await
            .status(),
        Status::Unauthorized,
        "the token must be dead after logout"
    );
    // And logging in again works.
    assert!(login(&c, "ada", ADA_PASSWORD).await.is_ok());
}

/// A user in the roster with no password set cannot log in. The roster says
/// who exists; it does not grant access.
#[tokio::test]
async fn a_roster_user_without_a_password_cannot_sign_in() {
    let _home = myco::test_support::temp_home("web-auth-nopass");
    let server = test_server();
    server.auth().add_user("mallory", "Mallory").ok();
    let figment = rocket::Config::figment().merge(("log_level", "off"));
    let c = Client::tracked(myco::web::rocket(server, figment))
        .await
        .expect("rocket builds");

    assert_eq!(
        login(&c, "mallory", "any password at all")
            .await
            .unwrap_err(),
        Status::Unauthorized
    );
}

// ---------------------------------------------------------------------------
// One-time codes and passkeys
// ---------------------------------------------------------------------------

/// The code grant: the operator mints against the live server, the code
/// signs its user in exactly once, and nobody but the operator can mint.
#[tokio::test]
async fn a_one_time_code_signs_in_once_and_only_the_operator_mints() {
    let c = client().await;
    let server = c.rocket().state::<Arc<Server>>().expect("server").clone();

    // Grace (not the operator) may not mint, even authenticated.
    let grace_token = login(&c, "grace", GRACE_PASSWORD).await.expect("login");
    let refused = c
        .post("/api/auth/codes")
        .header(bearer(&grace_token))
        .header(ContentType::JSON)
        .body(r#"{"username": "grace"}"#)
        .dispatch()
        .await;
    assert_eq!(refused.status(), Status::Unauthorized);

    // The operator (the roster's local user, ada) mints for grace.
    let operator = server.auth().issue_for("ada").expect("operator token");
    let minted = c
        .post("/api/auth/codes")
        .header(bearer(&operator.access_token))
        .header(ContentType::JSON)
        .body(r#"{"username": "grace"}"#)
        .dispatch()
        .await;
    assert_eq!(minted.status(), Status::Ok);
    let body: serde_json::Value =
        serde_json::from_str(&minted.into_string().await.unwrap()).unwrap();
    let code = body["code"].as_str().expect("code").to_string();
    assert_eq!(body["username"], "grace");

    // The code is bound to its user: ada cannot redeem grace's code.
    let wrong_user = c
        .post("/api/auth/token")
        .header(ContentType::Form)
        .body(format!("grant_type=code&username=ada&code={code}"))
        .dispatch()
        .await;
    assert_eq!(wrong_user.status(), Status::Unauthorized);

    // Grace redeems it and the token speaks as her.
    let redeemed = c
        .post("/api/auth/token")
        .header(ContentType::Form)
        .body(format!("grant_type=code&username=grace&code={code}"))
        .dispatch()
        .await;
    assert_eq!(redeemed.status(), Status::Ok);
    let issued: serde_json::Value =
        serde_json::from_str(&redeemed.into_string().await.unwrap()).unwrap();
    let token = issued["access_token"].as_str().expect("token");
    let who = c.get("/api/whoami").header(bearer(token)).dispatch().await;
    assert_eq!(who.status(), Status::Ok);
    assert!(who.into_string().await.unwrap().contains("grace"));

    // Single use: the same code is dead now.
    let replay = c
        .post("/api/auth/token")
        .header(ContentType::Form)
        .body(format!("grant_type=code&username=grace&code={code}"))
        .dispatch()
        .await;
    assert_eq!(replay.status(), Status::Unauthorized);
}

/// The passkey ceremonies end to end against a software authenticator: an
/// authenticated session enrolls, the passkey signs its user in, a stranger's
/// ticket cannot be replayed, and unknown users get the same answer as users
/// without passkeys.
#[tokio::test]
async fn a_passkey_enrolls_and_signs_in_via_the_ceremonies() {
    use webauthn_authenticator_rs::{WebauthnAuthenticator, softtoken::SoftToken};

    let c = client().await;
    let origin = url::Url::parse("http://localhost:7773").expect("origin");
    let (token, _cert) = SoftToken::new(true).expect("soft token");
    let mut authenticator = WebauthnAuthenticator::new(token);

    // Enrollment requires a session; bootstrap with the password.
    let ada_token = login(&c, "ada", ADA_PASSWORD).await.expect("login");
    let start = c
        .post("/api/auth/passkey/register/start")
        .header(bearer(&ada_token))
        .dispatch()
        .await;
    assert_eq!(start.status(), Status::Ok);
    let challenge: webauthn_rs::prelude::CreationChallengeResponse =
        serde_json::from_str(&start.into_string().await.unwrap()).unwrap();
    let created = authenticator
        .do_registration(origin.clone(), challenge)
        .expect("softtoken registration");
    let finish = c
        .post("/api/auth/passkey/register/finish")
        .header(bearer(&ada_token))
        .header(ContentType::JSON)
        .body(serde_json::to_string(&created).unwrap())
        .dispatch()
        .await;
    assert_eq!(finish.status(), Status::Ok);

    // Sign in with it: challenge → assertion → token that speaks as ada.
    let start = c
        .post("/api/auth/passkey/login/start")
        .header(ContentType::JSON)
        .body(r#"{"username": "ada"}"#)
        .dispatch()
        .await;
    assert_eq!(start.status(), Status::Ok);
    let body: serde_json::Value =
        serde_json::from_str(&start.into_string().await.unwrap()).unwrap();
    let ticket = body["ticket"].as_str().expect("ticket").to_string();
    let options: webauthn_rs::prelude::RequestChallengeResponse =
        serde_json::from_value(body["options"].clone()).unwrap();
    let asserted = authenticator
        .do_authentication(origin, options)
        .expect("softtoken assertion");
    let finish = c
        .post("/api/auth/passkey/login/finish")
        .header(ContentType::JSON)
        .body(serde_json::json!({ "ticket": ticket, "credential": asserted }).to_string())
        .dispatch()
        .await;
    assert_eq!(finish.status(), Status::Ok);
    let issued: serde_json::Value =
        serde_json::from_str(&finish.into_string().await.unwrap()).unwrap();
    let who = c
        .get("/api/whoami")
        .header(bearer(issued["access_token"].as_str().unwrap()))
        .dispatch()
        .await;
    assert!(who.into_string().await.unwrap().contains("ada"));

    // A ticket is single use — replaying the finished ceremony fails.
    let replay = c
        .post("/api/auth/passkey/login/finish")
        .header(ContentType::JSON)
        .body(serde_json::json!({ "ticket": ticket, "credential": asserted }).to_string())
        .dispatch()
        .await;
    assert_eq!(replay.status(), Status::Unauthorized);

    // No enumeration: a user without passkeys and a user who does not exist
    // answer identically.
    let mut answers = Vec::new();
    for name in ["grace", "nobody-here"] {
        let r = c
            .post("/api/auth/passkey/login/start")
            .header(ContentType::JSON)
            .body(format!(r#"{{"username": "{name}"}}"#))
            .dispatch()
            .await;
        answers.push((r.status(), r.into_string().await.unwrap()));
    }
    assert_eq!(answers[0], answers[1], "uniform answer, no enumeration");
}

/// The point of passkeys over tokens: credentials persist in
/// `passkeys.json`, so a server restart — which voids every token by design
/// — does not cost anyone their sign-in method.
#[tokio::test]
async fn passkeys_survive_a_server_restart() {
    use webauthn_authenticator_rs::{WebauthnAuthenticator, softtoken::SoftToken};

    let dir = std::env::temp_dir().join(format!("myco-passkey-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let auth_path = dir.join("auth.json");
    let origin = url::Url::parse("http://localhost:7773").expect("origin");
    let (token, _cert) = SoftToken::new(true).expect("soft token");
    let mut authenticator = WebauthnAuthenticator::new(token);

    let build = |auth: Arc<AuthStore>| {
        let config = myco::config::Config::resolve_with(
            Default::default(),
            |k| (k == "USER").then(|| "ada".to_string()),
            |_, _| myco::config::parse_file_config_str(CONFIG_TOML),
            |_| myco::config::parse_file_roster_str(ROSTER_TOML),
            || Ok(Vec::new()),
            |_| Err("no auth files in tests".into()),
        )
        .expect("test config resolves");
        Server::with_model_factory_and_auth(
            config,
            Box::new(|_, _, _, _| Ok(ScriptedModel::new(vec![]) as Arc<dyn GenerativeModel>)),
            auth,
        )
    };

    // First life: enroll.
    {
        let store = Arc::new(
            AuthStore::open(&auth_path)
                .expect("open store")
                .with_work_factor(1),
        );
        let server = build(store);
        server.auth().set_password("ada", ADA_PASSWORD).unwrap();
        let c = Client::tracked(myco::web::rocket(
            server,
            rocket::Config::figment().merge(("log_level", "off")),
        ))
        .await
        .expect("rocket");
        let ada_token = login(&c, "ada", ADA_PASSWORD).await.expect("login");
        let start = c
            .post("/api/auth/passkey/register/start")
            .header(bearer(&ada_token))
            .dispatch()
            .await;
        let challenge: webauthn_rs::prelude::CreationChallengeResponse =
            serde_json::from_str(&start.into_string().await.unwrap()).unwrap();
        let created = authenticator
            .do_registration(origin.clone(), challenge)
            .expect("registration");
        let finish = c
            .post("/api/auth/passkey/register/finish")
            .header(bearer(&ada_token))
            .header(ContentType::JSON)
            .body(serde_json::to_string(&created).unwrap())
            .dispatch()
            .await;
        assert_eq!(finish.status(), Status::Ok);
    }

    // Second life: a fresh store from the same files — every token is gone,
    // the passkey is not, and it signs ada in.
    {
        let store = Arc::new(
            AuthStore::open(&auth_path)
                .expect("reopen store")
                .with_work_factor(1),
        );
        assert_eq!(store.passkey_counts().get("ada"), Some(&1));
        let server = build(store);
        let c = Client::tracked(myco::web::rocket(
            server,
            rocket::Config::figment().merge(("log_level", "off")),
        ))
        .await
        .expect("rocket");
        let start = c
            .post("/api/auth/passkey/login/start")
            .header(ContentType::JSON)
            .body(r#"{"username": "ada"}"#)
            .dispatch()
            .await;
        assert_eq!(start.status(), Status::Ok);
        let body: serde_json::Value =
            serde_json::from_str(&start.into_string().await.unwrap()).unwrap();
        let options: webauthn_rs::prelude::RequestChallengeResponse =
            serde_json::from_value(body["options"].clone()).unwrap();
        let asserted = authenticator
            .do_authentication(origin, options)
            .expect("assertion");
        let finish = c
            .post("/api/auth/passkey/login/finish")
            .header(ContentType::JSON)
            .body(
                serde_json::json!({ "ticket": body["ticket"], "credential": asserted }).to_string(),
            )
            .dispatch()
            .await;
        assert_eq!(finish.status(), Status::Ok);
    }
    let _ = std::fs::remove_dir_all(&dir);
}
