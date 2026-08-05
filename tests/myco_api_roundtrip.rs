//! The [`MycoApi`] contract, exercised against the in-process [`Server`]
//! through `dyn MycoApi` only — the same calls `HttpClient` makes over HTTP.
//! Models are scripted via the factory seam; no provider, no network.
//!
//! Like `web_auth`, these tests share the process-global `MYCO_HOME` override
//! and serialize on `temp_home`'s lock.

use std::sync::Arc;

use myco::server::Server;
use myco_api::{Content, TurnEndReason};
use myco_api::{CreateSession, EntryBody, MycoApi, PostMessage, UpdateSession};
use myco_models::{GenerateError, GenerateOutput, GenerativeModel};
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

/// The identity gate is not what these tests are about: one registered user,
/// selected by the injected `$USER`.
const ROSTER_TOML: &str = "[[users]]\nid = \"tester\"\nname = \"Tester\"\n";

fn test_config() -> myco::config::Config {
    myco::config::Config::resolve_with(
        Default::default(),
        |k| (k == "USER").then(|| "tester".to_string()),
        |_| myco::config::parse_file_config_str(CONFIG_TOML),
        |_| myco::config::parse_file_roster_str(ROSTER_TOML),
        || Ok(Vec::new()),
        |_| Err("no auth files in tests".into()),
    )
    .expect("test config resolves")
}

fn scripted_server(
    make: impl Fn() -> Arc<dyn GenerativeModel> + Send + Sync + 'static,
) -> Arc<Server> {
    Server::with_model_factory(test_config(), Box::new(move |_, _, _, _, _| Ok(make())))
}

/// Poll until the queued turn lands (idle + condition), bounded.
async fn poll_until(
    server: &dyn MycoApi,
    id: &str,
    pred: impl Fn(&myco_api::Poll) -> bool,
) -> myco_api::Poll {
    for _ in 0..200 {
        let p = server.poll(id, 0).await.expect("poll");
        if !p.busy && pred(&p) {
            return p;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    panic!("turn never settled");
}

#[tokio::test]
async fn a_turn_round_trips_through_the_trait() {
    let _home = myco_test_support::temp_home("api-roundtrip");
    let server = scripted_server(|| {
        let turn = |text: &str| GenerateOutput {
            content: vec![Content::Text { text: text.into() }],
            tool_uses: vec![],
            turn_end_reason: TurnEndReason::EndTurn,
            usage: None,
        };
        ScriptedModel::new(vec![
            turn("scripted answer"),
            turn("second answer"),
            turn("third answer"),
        ])
    });
    let server = server.as_local();
    let server: &dyn MycoApi = &server;

    let s = server
        .create_session(CreateSession {
            model: None,
            parent_session: None,
            fork: false,
        })
        .await
        .expect("create");
    assert!(s.live);

    server
        .post_message(
            &s.id,
            PostMessage {
                text: "hello".into(),
            },
        )
        .await
        .expect("post");

    let p = poll_until(server, &s.id, |p| {
        p.entries
            .iter()
            .any(|e| matches!(e.body, EntryBody::Agent { .. }))
    })
    .await;
    assert!(p.last_error.is_none(), "{:?}", p.last_error);
    assert!(matches!(p.entries[0].body, EntryBody::User { .. }));
    assert!(matches!(p.entries[1].body, EntryBody::Agent { .. }));
    assert_eq!(p.entries[1].text(), "scripted answer");

    // Later messages keep running turns through the same trait. (Posted one
    // settled turn at a time: a message posted while a turn is in flight may
    // legally fold into that turn instead of running its own — that
    // pre-emption contract is pinned in `web_multiplayer`.)
    for (text, replies) in [("second", 2), ("third", 3)] {
        server
            .post_message(&s.id, PostMessage { text: text.into() })
            .await
            .expect("post");
        poll_until(server, &s.id, |p| {
            p.entries
                .iter()
                .filter(|e| matches!(e.body, EntryBody::Agent { .. }))
                .count()
                == replies
        })
        .await;
    }
    let p = poll_until(server, &s.id, |p| {
        p.entries
            .iter()
            .filter(|e| matches!(e.body, EntryBody::Agent { .. }))
            .count()
            == 3
    })
    .await;
    assert_eq!(
        p.entries
            .iter()
            .filter(|e| matches!(e.body, EntryBody::User { .. }))
            .count(),
        3
    );
    assert_eq!(p.entries.last().unwrap().text(), "third answer");

    // The session is listed, and retiring it works through the trait too.
    let listed = server.list_sessions(false).await.expect("list");
    assert!(listed.iter().any(|e| e.id == s.id));
    server.retire(&s.id).await.expect("retire");
    let listed = server.list_sessions(false).await.expect("list");
    let entry = listed.iter().find(|e| e.id == s.id).expect("still on disk");
    assert!(!entry.live, "retired session must not be live");
}

#[tokio::test]
async fn a_failing_turn_surfaces_on_last_error() {
    let _home = myco_test_support::temp_home("api-fail");
    let server = scripted_server(|| {
        ScriptedModel::new(vec![]).then_fail(GenerateError::ExecutionError(
            "provider down (scripted)".into(),
        ))
    });
    let server = server.as_local();
    let server: &dyn MycoApi = &server;

    let s = server
        .create_session(CreateSession {
            model: None,
            parent_session: None,
            fork: false,
        })
        .await
        .expect("create");
    server
        .post_message(&s.id, PostMessage { text: "hi".into() })
        .await
        .expect("post");

    let p = poll_until(server, &s.id, |p| p.last_error.is_some()).await;
    let err = p.last_error.expect("failed turn reports why");
    assert!(err.contains("provider down"), "{err}");
    // The user's message survives; no phantom assistant reply.
    assert!(
        p.entries
            .iter()
            .any(|e| matches!(e.body, EntryBody::User { .. }))
    );
    assert!(
        !p.entries
            .iter()
            .any(|e| matches!(e.body, EntryBody::Agent { .. }))
    );
}

/// Streaming is the contract, not a nicety: a turn must publish text deltas on
/// the live feed *before* it finishes, so a client can render as it goes.
#[tokio::test]
async fn a_turn_streams_deltas_before_it_finishes() {
    let _home = myco_test_support::temp_home("api-stream");
    let server = scripted_server(|| {
        ScriptedModel::new(vec![GenerateOutput {
            content: vec![
                Content::Thinking {
                    text: "pondering".into(),
                    signature: None,
                    redacted: false,
                },
                Content::Text {
                    text: "# Heading\n\nstreamed **body**".into(),
                },
            ],
            tool_uses: vec![],
            turn_end_reason: TurnEndReason::EndTurn,
            usage: None,
        }])
    });
    let server = server.as_local();
    let server: &dyn MycoApi = &server;

    let s = server
        .create_session(CreateSession {
            model: None,
            parent_session: None,
            fork: false,
        })
        .await
        .expect("create");

    // Subscribe first: the feed must carry the turn we are about to start.
    let mut stream = server.events(&s.id).await.expect("events");
    server
        .post_message(&s.id, PostMessage { text: "go".into() })
        .await
        .expect("post");

    let mut text = String::new();
    let mut thinking = String::new();
    let mut started = false;
    let collect = async {
        use futures::StreamExt;
        while let Some(ev) = stream.next().await {
            match ev {
                myco_api::StreamEvent::TurnStarted => started = true,
                myco_api::StreamEvent::TextDelta { text: t } => text.push_str(&t),
                myco_api::StreamEvent::ThinkingDelta { text: t } => thinking.push_str(&t),
                myco_api::StreamEvent::TurnFinished => break,
                _ => {}
            }
        }
    };
    tokio::time::timeout(std::time::Duration::from_secs(10), collect)
        .await
        .expect("the feed produced a complete turn");

    assert!(started, "the feed announces the turn start");
    assert_eq!(thinking, "pondering");
    assert_eq!(text, "# Heading\n\nstreamed **body**");

    // And the same content is in the transcript afterwards.
    let p = poll_until(server, &s.id, |p| {
        p.entries
            .iter()
            .any(|e| matches!(e.body, EntryBody::Agent { .. }))
    })
    .await;
    assert!(
        p.entries
            .iter()
            .any(|e| e.text().contains("streamed **body**"))
    );
}

/// The wire's turn-boundary contract: exactly one `TurnFinished` per turn,
/// sent only after the turn's entries are persisted — so a client that
/// refetches on it reads a transcript that already holds the answer, instead
/// of watching the answer vanish and reappear. Tool calls stream with their
/// ids, and each call's result arrives as `ToolFinished` before the turn
/// ends, so a live card completes in place.
#[tokio::test]
async fn the_wire_ends_a_turn_once_and_only_after_persistence() {
    use futures::StreamExt;

    let _home = myco_test_support::temp_home("api-turn-end");
    let server = scripted_server(|| {
        ScriptedModel::new(vec![
            GenerateOutput {
                content: vec![],
                tool_uses: vec![myco_api::ToolUse {
                    id: "call_wire".into(),
                    name: "no_such_tool".into(),
                    input: serde_json::json!({}),
                }],
                turn_end_reason: TurnEndReason::ToolUse,
                usage: None,
            },
            GenerateOutput {
                content: vec![Content::Text {
                    text: "the persisted answer".into(),
                }],
                tool_uses: vec![],
                turn_end_reason: TurnEndReason::EndTurn,
                usage: None,
            },
        ])
    });
    let server = server.as_local();
    let server: &dyn MycoApi = &server;

    let s = server
        .create_session(CreateSession {
            model: None,
            parent_session: None,
            fork: false,
        })
        .await
        .expect("create");
    let mut stream = server.events(&s.id).await.expect("events");
    server
        .post_message(&s.id, PostMessage { text: "go".into() })
        .await
        .expect("post");

    let mut tool_started_at = None;
    let mut tool_finished_at = None;
    let mut seen = 0usize;
    let collect = async {
        while let Some(ev) = stream.next().await {
            seen += 1;
            match ev {
                myco_api::StreamEvent::ToolStarted { id, .. } => {
                    assert_eq!(id, "call_wire", "the running card's identity");
                    tool_started_at = Some(seen);
                }
                myco_api::StreamEvent::ToolFinished { result } => {
                    assert_eq!(result.id, "call_wire");
                    tool_finished_at = Some(seen);
                }
                myco_api::StreamEvent::TurnFinished => break,
                _ => {}
            }
        }
    };
    tokio::time::timeout(std::time::Duration::from_secs(10), collect)
        .await
        .expect("the feed produced a complete turn");

    // The call streamed with its id and completed, in order, mid-turn.
    let (started, finished) = (
        tool_started_at.expect("ToolStarted on the feed"),
        tool_finished_at.expect("ToolFinished on the feed"),
    );
    assert!(started < finished);

    // TurnFinished is the persistence signal: the transcript already holds
    // the final answer the moment it arrives — no re-poll loop, one read.
    let d = server.session_detail(&s.id).await.expect("detail");
    assert!(
        d.entries
            .iter()
            .any(|e| e.text().contains("the persisted answer")),
        "TurnFinished arrived before the turn was persisted: {:?}",
        d.entries
    );

    // And it is the only one: the feed stays quiet after it.
    let extra = tokio::time::timeout(std::time::Duration::from_millis(300), stream.next()).await;
    match extra {
        Err(_) => {}
        Ok(Some(myco_api::StreamEvent::TurnFinished)) => {
            panic!("a second TurnFinished reached the wire")
        }
        Ok(other) => panic!("unexpected trailing event: {other:?}"),
    }
}

/// A session becomes durable when it has content. Opening one and walking
/// away must leave nothing behind — otherwise every stray "new session"
/// click accretes an empty file in the store.
#[tokio::test]
async fn an_abandoned_session_leaves_nothing_on_disk() {
    let home = myco_test_support::temp_home("api-empty");
    let server = scripted_server(|| {
        ScriptedModel::new(vec![GenerateOutput {
            content: vec![Content::Text {
                text: "written".into(),
            }],
            tool_uses: vec![],
            turn_end_reason: TurnEndReason::EndTurn,
            usage: None,
        }])
    });
    let api = server.as_local();
    let api: &dyn MycoApi = &api;

    let count_sessions = || {
        // v2 keeps its whole world under `$MYCO_HOME/v2`.
        let root = home.path().join("v2").join("session");
        let mut n = 0;
        if let Ok(shards) = std::fs::read_dir(&root) {
            for shard in shards.flatten() {
                if let Ok(files) = std::fs::read_dir(shard.path()) {
                    n += files
                        .flatten()
                        .filter(|f| f.path().extension().is_some_and(|e| e == "json"))
                        .count();
                }
            }
        }
        n
    };
    assert_eq!(count_sessions(), 0, "store starts empty");

    // Created but never used: nothing is written, and retiring it is clean.
    let abandoned = api
        .create_session(CreateSession {
            model: None,
            parent_session: None,
            fork: false,
        })
        .await
        .expect("create");
    assert_eq!(count_sessions(), 0, "an empty session is not persisted");
    api.retire(&abandoned.id).await.expect("retire");
    assert_eq!(count_sessions(), 0, "and leaves nothing behind");

    // A session that receives a message does persist.
    let used = api
        .create_session(CreateSession {
            model: None,
            parent_session: None,
            fork: false,
        })
        .await
        .expect("create");
    api.post_message(&used.id, PostMessage { text: "hi".into() })
        .await
        .expect("post");
    poll_until(api, &used.id, |p| {
        p.entries
            .iter()
            .any(|e| matches!(e.body, EntryBody::Agent { .. }))
    })
    .await;
    assert_eq!(count_sessions(), 1, "a used session is on disk");
}

/// Archiving files a session away without losing it: gone from the default
/// listing, still readable, still there when archived ones are asked for.
#[tokio::test]
async fn archiving_hides_a_session_without_losing_it() {
    let _home = myco_test_support::temp_home("api-archive");
    let server = scripted_server(|| {
        ScriptedModel::new(vec![GenerateOutput {
            content: vec![Content::Text {
                text: "kept".into(),
            }],
            tool_uses: vec![],
            turn_end_reason: TurnEndReason::EndTurn,
            usage: None,
        }])
    });
    let api = server.as_local();
    let api: &dyn MycoApi = &api;

    let s = api
        .create_session(CreateSession {
            model: None,
            parent_session: None,
            fork: false,
        })
        .await
        .expect("create");
    api.post_message(&s.id, PostMessage { text: "hi".into() })
        .await
        .expect("post");
    poll_until(api, &s.id, |p| {
        p.entries
            .iter()
            .any(|e| matches!(e.body, EntryBody::Agent { .. }))
    })
    .await;
    assert!(
        api.list_sessions(false)
            .await
            .unwrap()
            .iter()
            .any(|e| e.id == s.id)
    );

    let updated = api
        .update_session(
            &s.id,
            UpdateSession {
                title: Some("filed".into()),
                archived: Some(true),
            },
        )
        .await
        .expect("archive");
    assert!(updated.archived);
    assert_eq!(updated.title.as_deref(), Some("filed"));

    // Out of the default listing, present when asked for, still readable.
    let plain = api.list_sessions(false).await.unwrap();
    assert!(!plain.iter().any(|e| e.id == s.id), "archived is hidden");
    let all = api.list_sessions(true).await.unwrap();
    assert!(
        all.iter().any(|e| e.id == s.id),
        "archived is listed on request"
    );
    let detail = api.session_detail(&s.id).await.expect("still readable");
    assert!(detail.entries.iter().any(|e| e.text() == "kept"));

    // And it comes back.
    api.update_session(
        &s.id,
        UpdateSession {
            title: None,
            archived: Some(false),
        },
    )
    .await
    .expect("unarchive");
    assert!(
        api.list_sessions(false)
            .await
            .unwrap()
            .iter()
            .any(|e| e.id == s.id)
    );
}
