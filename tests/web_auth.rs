//! HTTP authentication, exercised against the real Rocket instance.
//!
//! The claim under test is structural: there is no anonymous path through
//! `/api`. Every route is mounted behind the `Caller` guard, so a request
//! without a valid access token cannot reach the runtime — and one that *is*
//! valid has its writes attributed to the token's owner, not to whoever
//! happens to be running the process.
//!
//! There are no passwords. Tokens are obtained the way a client obtains
//! them: redeeming an operator-minted one-time code at `POST /api/auth/token`
//! (`grant_type=code`), or a passkey ceremony. Administration is the
//! operator-only `/api/admin` surface — the server is the only writer of the
//! credential files, and this suite is where that surface is pinned.

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

/// The roster names who exists; credentials come from the store. `ada` is the
/// local user, which makes her the operator.
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
    let auth = Arc::new(AuthStore::in_memory());
    Server::with_model_factory_and_auth(
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

/// Sign `id` in the way a person does: a one-time code (minted here straight
/// into the store — the HTTP mint route has its own test) redeemed at the
/// token endpoint.
async fn signin(c: &Client, id: &str) -> String {
    let server = c.rocket().state::<Arc<Server>>().expect("server").clone();
    let minted = server.auth().mint_code(id).expect("mint");
    let resp = c
        .post("/api/auth/token")
        .header(ContentType::Form)
        .body(format!(
            "grant_type=code&username={id}&code={}",
            minted.code
        ))
        .dispatch()
        .await;
    assert_eq!(resp.status(), Status::Ok, "code sign-in for {id}");
    let token: myco_api::AccessToken = resp.into_json().await.expect("token response");
    assert_eq!(token.token_type, "bearer");
    assert!(token.expires_in > 0);
    token.access_token
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
        "/api/admin/users",
    ] {
        let resp = c.get(path).dispatch().await;
        assert_eq!(resp.status(), Status::Unauthorized, "GET {path}");
    }
    for path in [
        "/api/sessions",
        "/api/sessions/deadbeef/messages",
        "/api/sessions/deadbeef/cancel",
        "/api/sessions/deadbeef/compact",
        "/api/admin/users/ada/code",
        "/api/admin/users/grace/disable",
    ] {
        let resp = c.post(path).dispatch().await;
        assert_eq!(resp.status(), Status::Unauthorized, "POST {path}");
    }
    for path in ["/api/sessions/deadbeef/live", "/api/admin/users/grace"] {
        let resp = c.delete(path).dispatch().await;
        assert_eq!(resp.status(), Status::Unauthorized, "DELETE {path}");
    }
    let resp = c.patch("/api/sessions/deadbeef").dispatch().await;
    assert_eq!(resp.status(), Status::Unauthorized);
}

/// A wrong token is rejected, and so is a *prefix* of a real one — the
/// comparison must not accept a partial match.
#[tokio::test]
async fn a_bad_token_is_rejected() {
    let _home = myco::test_support::temp_home("web-auth-bad");
    let c = client().await;

    let live = signin(&c, "ada").await;
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

    for (id, name) in [("ada", "Ada Lovelace"), ("grace", "Grace Hopper")] {
        let token = signin(&c, id).await;
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
    let ada = signin(&c, "ada").await;
    let grace = signin(&c, "grace").await;

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
    let ada = signin(&c, "ada").await;

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

/// The retired grants stay retired: `password` (and anything else) is refused
/// by name, so a stale client learns the grant is gone rather than being told
/// its credentials are wrong.
#[tokio::test]
async fn only_the_code_grant_exists() {
    let _home = myco::test_support::temp_home("web-auth-grants");
    let c = client().await;

    for grant in ["password", "client_credentials", "authorization_code"] {
        let resp = c
            .post("/api/auth/token")
            .header(ContentType::Form)
            .body(format!(
                "grant_type={grant}&username=ada&password=whatever&code=whatever"
            ))
            .dispatch()
            .await;
        assert_eq!(resp.status(), Status::BadRequest, "grant {grant}");
        let body = resp.into_string().await.unwrap();
        assert!(body.contains(grant), "the refusal names the grant: {body}");
    }

    // The failure mode for a *bad code* is uniform: unknown user, known user
    // with no code outstanding, and a wrong guess all answer identically.
    let mut answers = Vec::new();
    for user in ["nobody", "grace", "ada"] {
        let resp = c
            .post("/api/auth/token")
            .header(ContentType::Form)
            .body(format!("grant_type=code&username={user}&code=AAAAA-AAAAA"))
            .dispatch()
            .await;
        answers.push((resp.status(), resp.into_string().await.unwrap()));
    }
    assert_eq!(answers[0], answers[1], "uniform answer, no enumeration");
    assert_eq!(answers[1], answers[2], "uniform answer, no enumeration");
}

/// Signing out has to end the session server-side. A client that merely
/// forgets its token leaves a live credential behind.
#[tokio::test]
async fn logging_out_invalidates_the_token() {
    let _home = myco::test_support::temp_home("web-auth-logout");
    let c = client().await;
    let ada = signin(&c, "ada").await;

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
    // And signing in again works.
    let _ = signin(&c, "ada").await;
}

// ---------------------------------------------------------------------------
// One-time codes
// ---------------------------------------------------------------------------

/// The code lifecycle over HTTP: the operator mints at the admin route, the
/// code signs its user in exactly once, and nobody but the operator can mint.
#[tokio::test]
async fn a_one_time_code_signs_in_once_and_only_the_operator_mints() {
    let _home = myco::test_support::temp_home("web-auth-codes");
    let c = client().await;

    // Grace (not the operator) may not mint, even authenticated.
    let grace_token = signin(&c, "grace").await;
    let refused = c
        .post("/api/admin/users/grace/code")
        .header(bearer(&grace_token))
        .dispatch()
        .await;
    assert_eq!(refused.status(), Status::Unauthorized);

    // The operator (the roster's local user, ada) mints for grace.
    let ada_token = signin(&c, "ada").await;
    let minted = c
        .post("/api/admin/users/grace/code")
        .header(bearer(&ada_token))
        .dispatch()
        .await;
    assert_eq!(minted.status(), Status::Ok);
    let body: myco_api::MintedLoginCode = minted.into_json().await.expect("minted");
    assert_eq!(body.username, "grace");

    // The code is bound to its user: ada cannot redeem grace's code.
    let wrong_user = c
        .post("/api/auth/token")
        .header(ContentType::Form)
        .body(format!("grant_type=code&username=ada&code={}", body.code))
        .dispatch()
        .await;
    assert_eq!(wrong_user.status(), Status::Unauthorized);

    // Grace redeems it and the token speaks as her.
    let redeemed = c
        .post("/api/auth/token")
        .header(ContentType::Form)
        .body(format!("grant_type=code&username=grace&code={}", body.code))
        .dispatch()
        .await;
    assert_eq!(redeemed.status(), Status::Ok);
    let issued: myco_api::AccessToken = redeemed.into_json().await.expect("token");
    let who = c
        .get("/api/whoami")
        .header(bearer(&issued.access_token))
        .dispatch()
        .await;
    assert_eq!(who.status(), Status::Ok);
    assert!(who.into_string().await.unwrap().contains("grace"));

    // Single use: the same code is dead now.
    let replay = c
        .post("/api/auth/token")
        .header(ContentType::Form)
        .body(format!("grant_type=code&username=grace&code={}", body.code))
        .dispatch()
        .await;
    assert_eq!(replay.status(), Status::Unauthorized);
}

// ---------------------------------------------------------------------------
// Administration
// ---------------------------------------------------------------------------

/// The `/api/admin` surface is operator-only: an authenticated non-operator
/// gets the same refusal on every route.
#[tokio::test]
async fn admin_routes_refuse_everyone_but_the_operator() {
    let _home = myco::test_support::temp_home("web-auth-admin-gate");
    let c = client().await;
    let grace = signin(&c, "grace").await;

    let get = c
        .get("/api/admin/users")
        .header(bearer(&grace))
        .dispatch()
        .await;
    assert_eq!(get.status(), Status::Unauthorized);
    for path in [
        "/api/admin/users/ada/code",
        "/api/admin/users/ada/disable",
        "/api/admin/users/ada/enable",
        "/api/admin/users/ada/revoke",
        "/api/admin/users/ada/passkeys/clear",
    ] {
        let resp = c.post(path).header(bearer(&grace)).dispatch().await;
        assert_eq!(resp.status(), Status::Unauthorized, "POST {path}");
    }
    let del = c
        .delete("/api/admin/users/ada")
        .header(bearer(&grace))
        .dispatch()
        .await;
    assert_eq!(del.status(), Status::Unauthorized);
}

/// The admin listing is the old `auth list` table, as data: who exists,
/// their state, and their live-session and passkey counts.
#[tokio::test]
async fn the_admin_listing_reports_the_roster_and_its_state() {
    let _home = myco::test_support::temp_home("web-auth-admin-list");
    let c = client().await;
    let ada = signin(&c, "ada").await;
    let _grace_one = signin(&c, "grace").await;
    let _grace_two = signin(&c, "grace").await;

    let resp = c
        .get("/api/admin/users")
        .header(bearer(&ada))
        .dispatch()
        .await;
    assert_eq!(resp.status(), Status::Ok);
    let list: myco_api::AdminUsers = resp.into_json().await.expect("admin users");
    assert_eq!(list.operator, "ada");
    let grace = list.users.iter().find(|u| u.id == "grace").expect("grace");
    assert_eq!(grace.sessions, 2);
    assert_eq!(grace.passkeys, 0);
    assert!(!grace.disabled);
    assert!(list.users.iter().any(|u| u.id == "ada"));
}

/// disable → refused sign-in and dead sessions; enable → fresh codes work
/// again; revoke → sessions die but the account stays; remove → forgotten.
#[tokio::test]
async fn the_admin_actions_manage_a_user_end_to_end() {
    let _home = myco::test_support::temp_home("web-auth-admin-act");
    let c = client().await;
    let server = c.rocket().state::<Arc<Server>>().expect("server").clone();
    let ada = signin(&c, "ada").await;

    // Disable grace mid-session: her token dies with the account.
    let grace = signin(&c, "grace").await;
    let resp = c
        .post("/api/admin/users/grace/disable")
        .header(bearer(&ada))
        .dispatch()
        .await;
    assert_eq!(resp.status(), Status::Ok);
    assert_eq!(
        c.get("/api/whoami")
            .header(bearer(&grace))
            .dispatch()
            .await
            .status(),
        Status::Unauthorized
    );
    // A disabled account cannot even be minted for.
    let resp = c
        .post("/api/admin/users/grace/code")
        .header(bearer(&ada))
        .dispatch()
        .await;
    assert_eq!(resp.status(), Status::BadRequest);

    // Enable and she signs in again.
    let resp = c
        .post("/api/admin/users/grace/enable")
        .header(bearer(&ada))
        .dispatch()
        .await;
    assert_eq!(resp.status(), Status::Ok);
    let grace = signin(&c, "grace").await;

    // Revoke ends the session but keeps the account.
    let resp = c
        .post("/api/admin/users/grace/revoke")
        .header(bearer(&ada))
        .dispatch()
        .await;
    assert_eq!(resp.status(), Status::Ok);
    let count: myco_api::AdminActionCount = resp.into_json().await.expect("count");
    assert_eq!(count.affected, 1);
    assert_eq!(
        c.get("/api/whoami")
            .header(bearer(&grace))
            .dispatch()
            .await
            .status(),
        Status::Unauthorized
    );

    // Remove forgets her entirely.
    let resp = c
        .delete("/api/admin/users/grace")
        .header(bearer(&ada))
        .dispatch()
        .await;
    assert_eq!(resp.status(), Status::Ok);
    assert!(server.auth().get("grace").is_none());
}

/// The lockout invariant: the account `--recover` signs in as can never be
/// disabled or removed, not even by itself.
#[tokio::test]
async fn the_operator_account_cannot_be_locked_out() {
    let _home = myco::test_support::temp_home("web-auth-admin-self");
    let c = client().await;
    let ada = signin(&c, "ada").await;

    let disable = c
        .post("/api/admin/users/ada/disable")
        .header(bearer(&ada))
        .dispatch()
        .await;
    assert_eq!(disable.status(), Status::BadRequest);
    let remove = c
        .delete("/api/admin/users/ada")
        .header(bearer(&ada))
        .dispatch()
        .await;
    assert_eq!(remove.status(), Status::BadRequest);
    // Still alive and still the operator.
    let resp = c
        .get("/api/admin/users")
        .header(bearer(&ada))
        .dispatch()
        .await;
    assert_eq!(resp.status(), Status::Ok);
}

// ---------------------------------------------------------------------------
// Passkeys
// ---------------------------------------------------------------------------

/// The passkey ceremonies end to end against a software authenticator: a
/// code-bootstrapped session enrolls, the passkey signs its user in, a
/// stranger's ticket cannot be replayed, and unknown users get the same
/// answer as users without passkeys.
#[tokio::test]
async fn a_passkey_enrolls_and_signs_in_via_the_ceremonies() {
    use webauthn_authenticator_rs::{WebauthnAuthenticator, softtoken::SoftToken};

    let c = client().await;
    let server = c.rocket().state::<Arc<Server>>().expect("server").clone();
    let origin = url::Url::parse("http://localhost:7773").expect("origin");
    let (token, _cert) = SoftToken::new(true).expect("soft token");
    let mut authenticator = WebauthnAuthenticator::new(token);

    // Enrollment requires a session; a one-time code bootstraps it.
    let ada_token = signin(&c, "ada").await;
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

    // With a passkey on file the startup banner has nothing to say — unless
    // `--recover` demands a code anyway.
    assert!(myco::web::startup_code(&server, false).is_none());
    let banner = myco::web::startup_code(&server, true).expect("--recover always mints");
    assert!(banner.contains("--recover"), "{banner}");

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

/// "Forget a user" forgets their passkeys too: after an admin remove, the
/// re-added roster name answers the login-start probe exactly like a user
/// who never enrolled.
#[tokio::test]
async fn removing_a_user_forgets_their_passkeys() {
    use webauthn_authenticator_rs::{WebauthnAuthenticator, softtoken::SoftToken};

    let c = client().await;
    let server = c.rocket().state::<Arc<Server>>().expect("server").clone();
    let origin = url::Url::parse("http://localhost:7773").expect("origin");
    let (token, _cert) = SoftToken::new(true).expect("soft token");
    let mut authenticator = WebauthnAuthenticator::new(token);

    let grace_token = signin(&c, "grace").await;
    let start = c
        .post("/api/auth/passkey/register/start")
        .header(bearer(&grace_token))
        .dispatch()
        .await;
    let challenge: webauthn_rs::prelude::CreationChallengeResponse =
        serde_json::from_str(&start.into_string().await.unwrap()).unwrap();
    let created = authenticator
        .do_registration(origin, challenge)
        .expect("registration");
    let finish = c
        .post("/api/auth/passkey/register/finish")
        .header(bearer(&grace_token))
        .header(ContentType::JSON)
        .body(serde_json::to_string(&created).unwrap())
        .dispatch()
        .await;
    assert_eq!(finish.status(), Status::Ok);
    assert_eq!(server.auth().passkey_counts().get("grace"), Some(&1));

    let ada = signin(&c, "ada").await;
    let removed = c
        .delete("/api/admin/users/grace")
        .header(bearer(&ada))
        .dispatch()
        .await;
    assert_eq!(removed.status(), Status::Ok);
    assert_eq!(server.auth().passkey_counts().get("grace"), None);

    // Re-added (as the roster reconciliation would on restart), she is
    // indistinguishable from someone who never enrolled.
    server.auth().add_user("grace", "Grace Hopper").unwrap();
    let probe = c
        .post("/api/auth/passkey/login/start")
        .header(ContentType::JSON)
        .body(r#"{"username": "grace"}"#)
        .dispatch()
        .await;
    assert_eq!(probe.status(), Status::Unauthorized);
}

/// The bootstrap path: a fresh server (no passkeys for the operator) prints
/// a code, and that code actually signs the operator in over HTTP.
#[tokio::test]
async fn the_startup_code_bootstraps_a_fresh_server() {
    let _home = myco::test_support::temp_home("web-auth-bootstrap");
    let c = client().await;
    let server = c.rocket().state::<Arc<Server>>().expect("server").clone();

    let banner = myco::web::startup_code(&server, false).expect("fresh server mints");
    assert!(banner.contains("ada"), "{banner}");
    assert!(banner.contains("no passkey enrolled yet"), "{banner}");
    let code = banner
        .split_whitespace()
        .find(|w| w.len() == 11 && w.as_bytes()[5] == b'-')
        .expect("banner carries the code");

    let resp = c
        .post("/api/auth/token")
        .header(ContentType::Form)
        .body(format!("grant_type=code&username=ada&code={code}"))
        .dispatch()
        .await;
    assert_eq!(resp.status(), Status::Ok, "{banner}");
    let issued: myco_api::AccessToken = resp.into_json().await.expect("token");
    assert_eq!(issued.user.id, "ada");
}

/// The point of passkeys over tokens: credentials persist in
/// `passkeys.json`, so a server restart — which voids every token and code
/// by design — does not cost anyone their sign-in method.
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
        let store = Arc::new(AuthStore::open(&auth_path).expect("open store"));
        let server = build(store);
        let c = Client::tracked(myco::web::rocket(
            server,
            rocket::Config::figment().merge(("log_level", "off")),
        ))
        .await
        .expect("rocket");
        let ada_token = signin(&c, "ada").await;
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
        let store = Arc::new(AuthStore::open(&auth_path).expect("reopen store"));
        assert_eq!(store.passkey_counts().get("ada"), Some(&1));
        let server = build(store);
        // With a passkey on file, a restart no longer volunteers a code.
        assert!(myco::web::startup_code(&server, false).is_none());
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
