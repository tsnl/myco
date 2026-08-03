//! HTTP authentication, exercised against the real Rocket instance.
//!
//! The claim under test is structural: there is no anonymous path through
//! `/api`. Every route is mounted behind the `Caller` guard, so a request
//! without a recognized bearer token cannot reach the runtime — and one that
//! *is* recognized has its writes attributed to the token's owner, not to
//! whoever happens to be running the process.

use std::sync::Arc;

use myco::server::Server;
use myco_api::{Author, EntryBody};
use myco_models::{Content, GenerateOutput, GenerativeModel, TurnEndReason};
use myco_test_support::ScriptedModel;
use rocket::http::{Header, Status};
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

const ADA_TOKEN: &str = "0123456789abcdef0123456789abcdef";
const GRACE_TOKEN: &str = "fedcba9876543210fedcba9876543210";

/// Two users with tokens, plus the local operator with none: `ada` drives the
/// CLI in-process, and both `ada` and `grace` can reach the HTTP API.
const ROSTER_TOML: &str = r#"
[[users]]
id = "ada"
name = "Ada Lovelace"
token = "0123456789abcdef0123456789abcdef"

[[users]]
id = "grace"
name = "Grace Hopper"
token = "fedcba9876543210fedcba9876543210"
"#;

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
    Server::with_model_factory(
        config,
        Box::new(|_, _, _, _, _| {
            Ok(ScriptedModel::new(vec![GenerateOutput {
                content: vec![Content::Text { text: "ok".into() }],
                tool_uses: vec![],
                turn_end_reason: TurnEndReason::EndTurn,
                usage: None,
            }]) as Arc<dyn GenerativeModel>)
        }),
    )
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

    for token in ["", "wrong", &ADA_TOKEN[..16], &format!("{ADA_TOKEN}x")] {
        let resp = c.get("/api/whoami").header(bearer(token)).dispatch().await;
        assert_eq!(resp.status(), Status::Unauthorized, "token {token:?}");
    }
    // The scheme matters too: a bare token without `Bearer ` is not a
    // credential this server accepts.
    let resp = c
        .get("/api/whoami")
        .header(Header::new("Authorization", ADA_TOKEN))
        .dispatch()
        .await;
    assert_eq!(resp.status(), Status::Unauthorized);
}

#[tokio::test]
async fn a_token_identifies_its_owner() {
    let _home = myco_test_support::temp_home("web-auth-whoami");
    let c = client().await;

    for (token, id, name) in [
        (ADA_TOKEN, "ada", "Ada Lovelace"),
        (GRACE_TOKEN, "grace", "Grace Hopper"),
    ] {
        let resp = c.get("/api/whoami").header(bearer(token)).dispatch().await;
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

    let created = c
        .post("/api/sessions")
        .header(bearer(GRACE_TOKEN))
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
        .header(bearer(GRACE_TOKEN))
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
            .header(bearer(ADA_TOKEN))
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

    let created = c
        .post("/api/sessions")
        .header(bearer(ADA_TOKEN))
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
        .get(format!(
            "/api/sessions/{}/events?token={ADA_TOKEN}",
            summary.id
        ))
        .dispatch()
        .await;
    assert_eq!(ok.status(), Status::Ok);
}
