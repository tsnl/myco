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

use myco::server::Server;
use myco_api::{Author, EntryBody};
use myco_auth::AuthStore;
use myco_models::{Content, GenerateOutput, GenerativeModel, TurnEndReason};
use myco_test_support::ScriptedModel;
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
        |_| myco::config::parse_file_config_str(CONFIG_TOML),
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
        Box::new(|_, _, _, _, _| {
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
    let _home = myco_test_support::temp_home("web-auth-none");
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
    let _home = myco_test_support::temp_home("web-auth-bad");
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
    let _home = myco_test_support::temp_home("web-auth-whoami");
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
    let _home = myco_test_support::temp_home("web-auth-attrib");
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
    let _home = myco_test_support::temp_home("web-auth-sse");
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
    let _home = myco_test_support::temp_home("web-auth-grant");
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
    let _home = myco_test_support::temp_home("web-auth-logout");
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
    let _home = myco_test_support::temp_home("web-auth-nopass");
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
