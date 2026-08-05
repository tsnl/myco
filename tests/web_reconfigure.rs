//! Per-session model and effort switching over the API.
//!
//! The claims: a `PATCH` that names another catalog model (or an effort
//! level) is validated, persisted on the session document, and rebuilds the
//! live agent's model — so the next turn actually runs on what the client
//! picked. Invalid keys and levels are refused with the session untouched.

use std::sync::Arc;

use myco::auth::AuthStore;
use myco::models::{BackendConfig, Effort, GenerateOutput, GenerativeModel};
use myco::server::Server;
use myco::test_support::ScriptedModel;
use myco_api::{Author, Content, MycoApi, TurnEndReason};

const CONFIG_TOML: &str = r#"
model = "fake1"

[gateways.g]
protocol = "openai-completions"
base_url = "http://127.0.0.1:9/v1"
auth = "dummy"

[models.fake1]
gateway = "g"
context_window = 100000

[models.fake2]
gateway = "g"
context_window = 100000
effort = "medium"
"#;

const ROSTER_TOML: &str = "[[users]]\nid = \"ada\"\nname = \"Ada Lovelace\"\n";

/// A server whose factory answers every turn with the model and effort it was
/// actually built from — the observable that proves a reconfigure reached the
/// agent, not just the session document.
fn echo_server() -> Arc<Server> {
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
        Box::new(|_, _, catalog_model, _| {
            let effort = match &catalog_model.backend {
                BackendConfig::Anthropic(c) => c.effort,
                BackendConfig::OpenAIResponses(c) | BackendConfig::OpenAICompletions(c) => c.effort,
            };
            let line = format!(
                "running {} at {}",
                catalog_model.spec.key,
                effort.map(Effort::as_str).unwrap_or("unset")
            );
            let reply = GenerateOutput {
                content: vec![Content::Text { text: line }],
                tool_uses: vec![],
                turn_end_reason: TurnEndReason::EndTurn,
                usage: None,
            };
            Ok(ScriptedModel::new(vec![reply; 4]) as Arc<dyn GenerativeModel>)
        }),
        Arc::new(AuthStore::in_memory().with_work_factor(1)),
    )
}

async fn settle(api: &dyn MycoApi, id: &str, pred: impl Fn(&myco_api::Poll) -> bool) {
    for _ in 0..200 {
        let p = api.poll(id, 0).await.expect("poll");
        if !p.busy && pred(&p) {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    panic!("session never settled");
}

fn update(model: Option<&str>, effort: Option<&str>) -> myco_api::UpdateSession {
    myco_api::UpdateSession {
        model: model.map(str::to_string),
        effort: effort.map(str::to_string),
        ..Default::default()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_patched_model_and_effort_drive_the_next_turn() {
    let _home = myco::test_support::temp_home("web-reconfigure");
    let server = echo_server();
    let ada = server.as_user(Author::User {
        id: "ada".into(),
        name: "Ada Lovelace".into(),
    });

    let s = ada
        .create_session(myco_api::CreateSession {
            model: None,
            parent_session: None,
            fork: false,
        })
        .await
        .expect("create");

    // The configured default: fake1 at the resolved default effort.
    ada.post_message(
        &s.id,
        myco_api::PostMessage {
            text: "hello".into(),
        },
    )
    .await
    .expect("post");
    settle(&ada, &s.id, |p| {
        p.entries
            .iter()
            .any(|e| e.text() == "running fake1 at high")
    })
    .await;

    // Refusals leave the session untouched: an unknown key, a bad level.
    let bad_model = ada.update_session(&s.id, update(Some("nope"), None)).await;
    assert!(bad_model.is_err(), "unknown model key must be refused");
    let bad_effort = ada.update_session(&s.id, update(None, Some("plaid"))).await;
    assert!(bad_effort.is_err(), "unknown effort must be refused");
    let detail = ada.session_detail(&s.id).await.expect("detail");
    assert_eq!(detail.summary.model, "fake1");
    assert_eq!(detail.summary.effort, None);

    // Switch model and effort in one PATCH. The summary answers with the new
    // configuration; the session-level effort override beats fake2's own
    // configured `effort = "medium"`.
    let summary = ada
        .update_session(&s.id, update(Some("fake2"), Some("low")))
        .await
        .expect("patch");
    assert_eq!(summary.model, "fake2");
    assert_eq!(summary.effort.as_deref(), Some("low"));

    ada.post_message(
        &s.id,
        myco_api::PostMessage {
            text: "and now?".into(),
        },
    )
    .await
    .expect("post");
    settle(&ada, &s.id, |p| {
        p.entries.iter().any(|e| e.text() == "running fake2 at low")
    })
    .await;

    // Clearing the override (empty string on the wire) falls back to the
    // model's configured effort.
    let summary = ada
        .update_session(&s.id, update(None, Some("")))
        .await
        .expect("clear");
    assert_eq!(summary.effort, None);
    ada.post_message(
        &s.id,
        myco_api::PostMessage {
            text: "once more".into(),
        },
    )
    .await
    .expect("post");
    settle(&ada, &s.id, |p| {
        p.entries
            .iter()
            .any(|e| e.text() == "running fake2 at medium")
    })
    .await;
}
