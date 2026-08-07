//! The subagent surface: the `subagent` tool's hidden children observable and
//! drivable by a person, with the same keyboard-lock contract as shells.
//!
//! The claims: a child the agent delegates to shows up on the work panel's
//! list; the user takes it over, talks to it directly, and both the takeover
//! and what was said land in the *parent* transcript as attributed,
//! non-waking messages; while the user holds the child the parent agent's
//! `subagent` calls to it are refused politely; and handing it back restores
//! them.

use std::sync::Arc;

use myco::auth::AuthStore;
use myco::models::{GenerateOutput, GenerativeModel};
use myco::server::Server;
use myco::test_support::ScriptedModel;
use myco_api::{Author, Content, EntryBody, MycoApi, ShellLockMode, TurnEndReason};

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

const ROSTER_TOML: &str = "[[users]]\nid = \"ada\"\nname = \"Ada Lovelace\"\n";

fn answer(text: &str) -> GenerateOutput {
    GenerateOutput {
        content: vec![Content::Text { text: text.into() }],
        tool_uses: vec![],
        turn_end_reason: TurnEndReason::EndTurn,
        usage: None,
    }
}

fn delegate(call_id: &str, input: serde_json::Value) -> GenerateOutput {
    GenerateOutput {
        content: vec![],
        tool_uses: vec![myco_api::ToolUse {
            id: call_id.into(),
            name: "subagent".into(),
            input,
        }],
        turn_end_reason: TurnEndReason::ToolUse,
        usage: None,
    }
}

/// A server whose first model (the parent's) is scripted to delegate once and
/// is returned to the test for later, id-dependent turns; every later model
/// (each child's) answers its prompts in order.
fn subagent_server() -> (Arc<Server>, Arc<ScriptedModel>) {
    let config = myco::config::Config::resolve_with(
        Default::default(),
        |k| (k == "USER").then(|| "ada".to_string()),
        |_, _| myco::config::parse_file_config_str(CONFIG_TOML),
        |_| myco::config::parse_file_roster_str(ROSTER_TOML),
        || Ok(Vec::new()),
        |_| Err("no auth files in tests".into()),
    )
    .expect("test config resolves");

    let parent_model = ScriptedModel::new(vec![
        delegate(
            "call_sub",
            serde_json::json!({ "prompt": "compute the thing" }),
        ),
        answer("delegated: it said forty-two"),
    ]);
    let handle = parent_model.clone();
    let calls = std::sync::atomic::AtomicUsize::new(0);
    let server = Server::with_model_factory_and_auth(
        config,
        Box::new(move |_, _, _, _| {
            let n = calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if n == 0 {
                Ok(parent_model.clone() as Arc<dyn GenerativeModel>)
            } else {
                Ok(ScriptedModel::new(vec![
                    answer("forty-two"),
                    answer("hello person"),
                    answer("forty-three"),
                ]) as Arc<dyn GenerativeModel>)
            }
        }),
        Arc::new(AuthStore::in_memory()),
    );
    (server, handle)
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

fn has_text(p: &myco_api::Poll, needle: &str) -> bool {
    p.entries.iter().any(|e| e.text().contains(needle))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_user_can_watch_take_and_drive_the_agents_subagent() {
    let _home = myco::test_support::temp_home("web-subagents");
    let (server, parent_model) = subagent_server();
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
    ada.post_message(
        &s.id,
        myco_api::PostMessage {
            text: "delegate something".into(),
        },
    )
    .await
    .expect("post");
    settle(&ada, &s.id, |p| has_text(p, "delegated: it said forty-two")).await;

    // The child shows on the work panel's list: live, idle, agent-held.
    let subs = ada.subagents(&s.id).await.expect("subagents");
    assert_eq!(subs.subagents.len(), 1, "{:?}", subs.subagents);
    let child = subs.subagents[0].clone();
    assert_eq!(child.model, "fake");
    assert!(!child.busy);
    assert_eq!(child.lock, ShellLockMode::Assistant);
    // And the tool's own result named it, so parent history and panel agree.
    let p = ada.poll(&s.id, 0).await.expect("poll");
    assert!(
        has_text(&p, &format!("session: {}", child.id)),
        "tool result names the child"
    );

    // Posting into an agent-held child is refused — same contract as typing
    // into an agent-held shell.
    let refused = ada.subagent_input(&s.id, &child.id, "sneaky".into()).await;
    assert!(refused.is_err(), "agent-held subagent must refuse input");

    // Take the child over. The takeover is recorded in the parent transcript
    // (attributed, non-waking), exactly like a shell keyboard grab.
    let taken = ada
        .subagent_lock(&s.id, &child.id, ShellLockMode::User)
        .await
        .expect("lock");
    assert_eq!(taken.lock, ShellLockMode::User);
    settle(&ada, &s.id, |p| has_text(p, "[took subagent")).await;

    // Talk to the child directly: the child's own agent answers the person,
    // and what was said is echoed into the parent transcript.
    ada.subagent_input(&s.id, &child.id, "hello child".into())
        .await
        .expect("input");
    settle(&ada, &child.id, |p| has_text(p, "hello person")).await;
    let cp = ada.poll(&child.id, 0).await.expect("child poll");
    assert!(has_text(&cp, "hello child"), "the child heard the person");
    settle(&ada, &s.id, |p| has_text(p, "[posted to subagent")).await;

    // While the person holds the child, the parent agent's `subagent` calls
    // to it fail politely — scripted only now, because the call must name a
    // child id that did not exist when the server was built.
    parent_model.push(delegate(
        "call_sub2",
        serde_json::json!({ "prompt": "again", "session_id": child.id }),
    ));
    parent_model.push(answer("second delegation refused"));
    ada.post_message(
        &s.id,
        myco_api::PostMessage {
            text: "continue the child".into(),
        },
    )
    .await
    .expect("post");
    settle(&ada, &s.id, |p| has_text(p, "second delegation refused")).await;
    let p = ada.poll(&s.id, 0).await.expect("poll");
    let refusal = p
        .entries
        .iter()
        .filter_map(|e| match &e.body {
            EntryBody::ToolResults { results } => {
                results.iter().find(|r| r.id == "call_sub2").cloned()
            }
            _ => None,
        })
        .next()
        .expect("the second call has a result");
    assert!(refusal.is_error, "user-held child refuses the parent");
    assert!(
        myco_api::content_text(&refusal.content).contains("held by a person"),
        "{:?}",
        refusal.content
    );

    // Hand the child back (recorded too) and the parent may continue it.
    ada.subagent_lock(&s.id, &child.id, ShellLockMode::Assistant)
        .await
        .expect("unlock");
    settle(&ada, &s.id, |p| has_text(p, "[handed subagent")).await;
    parent_model.push(delegate(
        "call_sub3",
        serde_json::json!({ "prompt": "once more", "session_id": child.id }),
    ));
    parent_model.push(answer("third delegation: forty-three"));
    ada.post_message(
        &s.id,
        myco_api::PostMessage {
            text: "try again".into(),
        },
    )
    .await
    .expect("post");
    settle(&ada, &s.id, |p| {
        has_text(p, "third delegation: forty-three")
    })
    .await;
    let p = ada.poll(&s.id, 0).await.expect("poll");
    let third = p
        .entries
        .iter()
        .filter_map(|e| match &e.body {
            EntryBody::ToolResults { results } => {
                results.iter().find(|r| r.id == "call_sub3").cloned()
            }
            _ => None,
        })
        .next()
        .expect("the third call has a result");
    assert!(!third.is_error, "{:?}", third.content);
    assert!(
        myco_api::content_text(&third.content).contains("forty-three"),
        "the released child answered the parent again: {:?}",
        third.content
    );
}
