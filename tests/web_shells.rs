//! The interactive shell surface: background bash sessions observable and
//! drivable by a person, with a keyboard lock deciding who may type.
//!
//! The claims: a shell the agent starts shows up on the rail with its
//! scrollback readable by offset; the user takes the keyboard, types, and the
//! keystrokes reach the child, echo into the scrollback, and land in the
//! transcript as a non-waking message the agent reads at its next boundary;
//! the lock gates the agent's writes while it is the user's; and handing the
//! keyboard back restores them.

use std::sync::Arc;

use myco::server::Server;
use myco_api::{Author, Content, MycoApi, ShellLockMode, TurnEndReason};
use myco_auth::AuthStore;
use myco_models::{GenerateOutput, GenerativeModel};
use myco_test_support::ScriptedModel;

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

/// A server whose scripted agent starts one `cat` bash session and ends the
/// turn — leaving a live background shell behind.
fn shell_server() -> Arc<Server> {
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
        Box::new(|_, _, _, _| {
            Ok(ScriptedModel::new(vec![
                GenerateOutput {
                    content: vec![],
                    tool_uses: vec![myco_api::ToolUse {
                        id: "call_term".into(),
                        name: "bash".into(),
                        input: serde_json::json!({
                            "action": "start",
                            "session_id": "term",
                            "command": "cat",
                            "timeout_ms": 500,
                            "idle_ms": 100,
                        }),
                    }],
                    turn_end_reason: TurnEndReason::ToolUse,
                    usage: None,
                },
                GenerateOutput {
                    content: vec![Content::Text {
                        text: "shell is up".into(),
                    }],
                    tool_uses: vec![],
                    turn_end_reason: TurnEndReason::EndTurn,
                    usage: None,
                },
                GenerateOutput {
                    content: vec![Content::Text {
                        text: "read your note".into(),
                    }],
                    tool_uses: vec![],
                    turn_end_reason: TurnEndReason::EndTurn,
                    usage: None,
                },
            ]) as Arc<dyn GenerativeModel>)
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

/// Poll the shell tail until `pred` holds on the accumulated text.
async fn tail_until(
    api: &dyn MycoApi,
    id: &str,
    shell: &str,
    pred: impl Fn(&str) -> bool,
) -> String {
    let mut text = String::new();
    let mut from = 0u64;
    for _ in 0..200 {
        let chunk = api.shell_tail(id, shell, from).await.expect("tail");
        text.push_str(&chunk.data);
        from = chunk.end;
        if pred(&text) {
            return text;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    panic!("shell never produced the expected output: {text:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_user_can_watch_take_and_drive_the_agents_shell() {
    let _home = myco_test_support::temp_home("web-shells");
    let server = shell_server();
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
            text: "start a shell".into(),
        },
    )
    .await
    .expect("post");
    settle(&ada, &s.id, |p| {
        p.entries.iter().any(|e| e.text() == "shell is up")
    })
    .await;

    // The rail lists the shell, assistant-locked, running.
    let shells = ada.shells(&s.id).await.expect("shells");
    assert_eq!(shells.shells.len(), 1);
    let shell = &shells.shells[0];
    assert_eq!(shell.id, "term");
    assert!(shell.running);
    assert_eq!(shell.lock, ShellLockMode::Assistant);

    // Typing without the keyboard is refused — the lock is what stops two
    // writers interleaving into one stdin.
    let refused = ada.shell_input(&s.id, "term", "sneaky\n".into()).await;
    assert!(refused.is_err(), "assistant-locked shell must refuse input");

    // Take the keyboard, type, and the keystrokes reach the child: `cat`
    // copies them back, and the echo shows what was typed.
    let taken = ada
        .shell_lock(&s.id, "term", ShellLockMode::User)
        .await
        .expect("lock");
    assert_eq!(taken.lock, ShellLockMode::User);
    ada.shell_input(&s.id, "term", "hello from ada\n".into())
        .await
        .expect("input");
    let text = tail_until(&ada, &s.id, "term", |t| {
        t.matches("hello from ada").count() >= 2
    })
    .await;
    assert!(text.contains("hello from ada"), "{text:?}");

    // The intervention is part of the conversation: keystrokes and the lock
    // transition land as entries — attributed, non-waking — so the agent
    // does not discover a mutated shell with no explanation in its history.
    settle(&ada, &s.id, |p| {
        p.entries
            .iter()
            .any(|e| e.text().contains("[typed into shell"))
            && p.entries
                .iter()
                .any(|e| e.text().contains("[took the keyboard"))
    })
    .await;
    // Exactly one agent answer so far: none of it woke the agent.
    let p = ada.poll(&s.id, 0).await.expect("poll");
    let answers = p
        .entries
        .iter()
        .filter(|e| matches!(e.body, myco_api::EntryBody::Agent { .. }))
        .count();
    assert_eq!(answers, 2, "start turn only: tool round + answer");

    // Handing the keyboard back re-opens the agent's writes and is recorded.
    let returned = ada
        .shell_lock(&s.id, "term", ShellLockMode::Assistant)
        .await
        .expect("unlock");
    assert_eq!(returned.lock, ShellLockMode::Assistant);
    settle(&ada, &s.id, |p| {
        p.entries
            .iter()
            .any(|e| e.text().contains("[returned the keyboard"))
    })
    .await;

    // And the next turn's model call reads the interventions as context —
    // the room inbox delivered them like any other message.
    ada.post_message(
        &s.id,
        myco_api::PostMessage {
            text: "did you see that?".into(),
        },
    )
    .await
    .expect("post");
    settle(&ada, &s.id, |p| {
        p.entries.iter().any(|e| e.text() == "read your note")
    })
    .await;
}
