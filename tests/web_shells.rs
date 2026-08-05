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

use myco::auth::AuthStore;
use myco::models::{GenerateOutput, GenerativeModel};
use myco::server::Server;
use myco::test_support::ScriptedModel;
use myco_api::{Author, Content, MycoApi, ShellLockMode, TurnEndReason};

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

/// A server whose scripted agent starts one `cat` bash session (on `host` —
/// `None` = local) and ends the turn — leaving a live background shell behind.
fn shell_server(host: Option<&str>) -> Arc<Server> {
    let mut config = myco::config::Config::resolve_with(
        Default::default(),
        |k| (k == "USER").then(|| "ada".to_string()),
        |_, _| myco::config::parse_file_config_str(CONFIG_TOML),
        |_| myco::config::parse_file_roster_str(ROSTER_TOML),
        || Ok(Vec::new()),
        |_| Err("no auth files in tests".into()),
    )
    .expect("test config resolves");
    if let Some(name) = host {
        // A "remote" that is really this build's own binary as a subprocess:
        // the full NDJSON protocol without any SSH.
        config
            .harness
            .remote_hosts
            .push(myco::machines::harness::HostConfig {
                name: name.into(),
                command: vec![
                    env!("CARGO_BIN_EXE_myco").into(),
                    "--mode".into(),
                    "host".into(),
                    "--name".into(),
                    name.into(),
                ],
            });
    }
    let mut start = serde_json::json!({
        "action": "start",
        "session_id": "term",
        "command": "cat",
        "timeout_ms": 2000,
        "idle_ms": 100,
    });
    if let Some(name) = host {
        start["host"] = serde_json::json!(name);
    }
    Server::with_model_factory_and_auth(
        config,
        Box::new(move |_, _, _, _| {
            Ok(ScriptedModel::new(vec![
                GenerateOutput {
                    content: vec![],
                    tool_uses: vec![myco_api::ToolUse {
                        id: "call_term".into(),
                        name: "bash".into(),
                        input: start.clone(),
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
    host: &str,
    shell: &str,
    pred: impl Fn(&str) -> bool,
) -> String {
    let mut text = String::new();
    let mut from = 0u64;
    for _ in 0..200 {
        let chunk = api.shell_tail(id, host, shell, from).await.expect("tail");
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
    let _home = myco::test_support::temp_home("web-shells");
    let server = shell_server(None);
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

    // The rail lists the shell, assistant-locked, running, on the local host.
    let shells = ada.shells(&s.id).await.expect("shells");
    assert_eq!(shells.shells.len(), 1);
    let shell = &shells.shells[0];
    assert_eq!(shell.id, "term");
    assert_eq!(shell.host, "local");
    assert!(shell.running);
    assert_eq!(shell.lock, ShellLockMode::Assistant);

    // Typing without the keyboard is refused — the lock is what stops two
    // writers interleaving into one stdin.
    let refused = ada
        .shell_input(&s.id, "local", "term", "sneaky\n".into())
        .await;
    assert!(refused.is_err(), "assistant-locked shell must refuse input");

    // Take the keyboard, type, and the keystrokes reach the child: `cat`
    // copies them back, and the echo shows what was typed.
    let taken = ada
        .shell_lock(&s.id, "local", "term", ShellLockMode::User)
        .await
        .expect("lock");
    assert_eq!(taken.lock, ShellLockMode::User);
    ada.shell_input(&s.id, "local", "term", "hello from ada\n".into())
        .await
        .expect("input");
    let text = tail_until(&ada, &s.id, "local", "term", |t| {
        t.matches("hello from ada").count() >= 2
    })
    .await;
    assert!(text.contains("hello from ada"), "{text:?}");

    // The rendered screen shows the same conversation with the child.
    let screen = ada
        .shell_screen(&s.id, "local", "term")
        .await
        .expect("screen");
    assert!(screen.text.contains("hello from ada"), "{:?}", screen.text);

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
        .shell_lock(&s.id, "local", "term", ShellLockMode::Assistant)
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

/// The same surface for a shell the agent started on a *remote* host: it
/// lists under its host name, its keyboard moves, keystrokes reach the child
/// over the NDJSON protocol, the screen renders, and the interventions land
/// in the transcript naming the host.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_remote_shell_is_watchable_and_drivable_over_the_web_api() {
    let _home = myco::test_support::temp_home("web-remote-shells");
    let server = shell_server(Some("rem"));
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
            text: "start a shell over there".into(),
        },
    )
    .await
    .expect("post");
    settle(&ada, &s.id, |p| {
        p.entries.iter().any(|e| e.text() == "shell is up")
    })
    .await;

    let shells = ada.shells(&s.id).await.expect("shells");
    assert_eq!(shells.shells.len(), 1, "{:?}", shells.shells);
    assert_eq!(shells.shells[0].host, "rem");
    assert_eq!(shells.shells[0].id, "term");
    assert!(shells.shells[0].running);

    let taken = ada
        .shell_lock(&s.id, "rem", "term", ShellLockMode::User)
        .await
        .expect("lock");
    assert_eq!(taken.lock, ShellLockMode::User);
    ada.shell_input(&s.id, "rem", "term", "typed across machines\n".into())
        .await
        .expect("input");
    let text = tail_until(&ada, &s.id, "rem", "term", |t| {
        t.matches("typed across machines").count() >= 2
    })
    .await;
    assert!(text.contains("typed across machines"), "{text:?}");

    let screen = ada
        .shell_screen(&s.id, "rem", "term")
        .await
        .expect("screen");
    assert!(
        screen.text.contains("typed across machines"),
        "{:?}",
        screen.text
    );

    // The transcript names the host, so "who typed what where" reads back.
    settle(&ada, &s.id, |p| {
        p.entries
            .iter()
            .any(|e| e.text().contains("[typed into shell \"term\" on rem]"))
    })
    .await;
}
