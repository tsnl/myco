use axum::{
    Json,
    extract::{Path, State},
    http::{HeaderMap, StatusCode, header},
    response::IntoResponse,
};
use futures::StreamExt;
use myco::generative_model::{Content, Message, ToolResult};

use super::super::http::{Server, SessionRequest, allowed, events, session_action, session_cancel};
use super::*;

fn app() -> (Arc<App>, mpsc::Receiver<Work>) {
    app_for("session", broadcast::channel(4).0)
}

fn app_for(id: &str, events: broadcast::Sender<Arc<Update>>) -> (Arc<App>, mpsc::Receiver<Work>) {
    let (work, receiver) = mpsc::channel(1);
    let app = Arc::new(App {
        live: Mutex::new(Live {
            snapshot: Snapshot {
                revision: 0,
                session_id: id.into(),
                thread_id: "thread".into(),
                title: "Test".into(),
                model: "test".into(),
                models: vec!["test".into(), "second".into()],
                busy: false,
                status: "Ready".into(),
                tasks: vec![],
                blocks: vec![],
            },
            cancel: None,
            accepted: HashMap::new(),
        }),
        events,
        work,
        shutdown: CancelToken::new(),
        generation: Arc::new(AtomicU64::new(0)),
    });
    (app, receiver)
}

fn server(apps: &[Arc<App>]) -> Arc<Server> {
    let config = Config::resolve_with(
        crate::ConfigUserSettings {
            config_path: Some("/unused/config.toml".into()),
            ..Default::default()
        },
        |_| None,
        false,
        |_, _| {
            Ok(toml::from_str(
                r#"
            [models.test]
            protocol = "openai-responses"
            base_url = "http://127.0.0.1:1"
            auth = { source = "none" }
            context_window = 100000
        "#,
            )
            .unwrap())
        },
        || Ok(vec![]),
        |_| unreachable!(),
    )
    .unwrap();
    let mut sessions = Sessions::new(
        <Args as clap::Parser>::parse_from(["myco", "--web"]),
        config,
        StartupPreflight::default(),
    );
    sessions.running = tokio::sync::Mutex::new(
        apps.iter()
            .map(|app| {
                (
                    app.snapshot().session_id,
                    RunningSession {
                        app: app.clone(),
                        session: ActiveSession::new(Session::new("test")),
                        worker: tokio::spawn(async {}),
                    },
                )
            })
            .collect(),
    );
    sessions.events = apps
        .first()
        .map_or_else(|| broadcast::channel(4).0, |app| app.events.clone());
    Arc::new(Server::new(
        sessions,
        "127.0.0.1:8765".parse().unwrap(),
        "/".into(),
    ))
}

fn action_request() -> ActionRequest {
    ActionRequest {
        request_id: Uuid::new_v4(),
        session_id: "session".into(),
        action: Action::Submit {
            text: "task".into(),
        },
    }
}

#[test]
fn tool_calls_appear_before_results_and_finish_independently() {
    let (app, _) = app();
    let first = ToolUse {
        name: "bash".into(),
        input: json!({"command":"sleep 10"}),
    };
    let second = ToolUse {
        name: "editor".into(),
        input: json!({"text":"write this"}),
    };
    for tool in [&first, &second] {
        app.emit(AgentEvent::ToolStarted {
            tool_use: tool.clone(),
            context: Default::default(),
        });
    }
    let started = app.snapshot().change;
    let blocks = started["snapshot"]["blocks"].as_array().unwrap();
    assert_eq!(blocks.len(), 2);
    assert!(blocks.iter().all(|block| block["running"] == true));
    assert_eq!(blocks[0]["tool"]["input"], first.input);
    app.emit(AgentEvent::ToolFinished {
        tool_use: second,
        result: ToolResult::text("saved"),
        context: Default::default(),
    });
    let finished = app.snapshot().change;
    let blocks = finished["snapshot"]["blocks"].as_array().unwrap();
    assert_eq!(blocks[0]["running"], true);
    assert_eq!(blocks[1]["running"], false);
    assert_eq!(blocks[1]["status"], "done");
    assert_eq!(blocks[1]["text"], "saved");
}

#[tokio::test]
async fn background_tasks_update_idle_clients_and_survive_reconnect() {
    let (app, _) = app();
    let server = server(std::slice::from_ref(&app));
    let response = events(State(server.clone())).await.into_response();
    let mut stream = response.into_body().into_data_stream();
    let _ = stream.next().await.unwrap().unwrap();
    let tasks = vec!["bash session build: cargo build (up 1s, idle 1s)".into()];
    app.tasks(tasks.clone());
    let updated = event_data(&stream.next().await.unwrap().unwrap());
    assert_eq!(updated["change"], json!({"kind":"tasks", "tasks":tasks}));
    assert_eq!(app.snapshot().change["snapshot"]["busy"], false);
    app.tasks(tasks.clone());
    assert_eq!(app.snapshot().revision, updated["revision"]);
    let response = events(State(server)).await.into_response();
    let mut reconnect = response.into_body().into_data_stream();
    let snapshot = event_data(&reconnect.next().await.unwrap().unwrap());
    assert_eq!(snapshot["change"]["snapshot"]["tasks"], json!(tasks));
    app.tasks(vec![]);
    let cleared = event_data(&stream.next().await.unwrap().unwrap());
    assert_eq!(cleared["change"], json!({"kind":"tasks", "tasks":[]}));
}

#[test]
fn cancelled_and_timed_out_calls_are_not_successful_outcomes() {
    for status in [
        "cancel requested; partial result recorded",
        "timed out after 10ms; process group killed",
        "exit 7",
        "signal 9",
    ] {
        let mut block = Block::tool(ToolUse {
            name: "any-tool".into(),
            input: Value::Null,
        });
        block.finish(&ToolResult::text("partial output").with_status(status));
        let block = serde_json::to_value(block).unwrap();
        assert_eq!(block["error"], true, "{status}");
        assert_eq!(block["running"], false);
    }
}

#[tokio::test]
async fn retries_keep_one_action_and_its_original_cancellation() {
    let (app, mut work) = app();
    let request = action_request();
    for _ in 0..2 {
        app.accept(request.clone()).unwrap();
    }
    let accepted = work.try_recv().unwrap();
    assert!(work.try_recv().is_err());
    let mut different = request.clone();
    different.request_id = Uuid::new_v4();
    assert_eq!(
        app.accept(different).unwrap_err().into_response().status(),
        StatusCode::CONFLICT
    );
    app.cancel("session").unwrap();
    assert!(accepted.cancel.is_cancelled());
    app.live.lock().unwrap().snapshot.busy = false;
    app.accept(request.clone()).unwrap();
    assert!(
        work.try_recv().is_err(),
        "A completed request must not run again"
    );
    let mut reused = request;
    reused.action = Action::Compact;
    assert_eq!(
        app.accept(reused).unwrap_err().into_response().status(),
        StatusCode::CONFLICT
    );
    let mut stale = action_request();
    stale.session_id = "another-session".into();
    assert_eq!(
        app.accept(stale).unwrap_err().into_response().status(),
        StatusCode::CONFLICT
    );
    assert!(work.try_recv().is_err());
}

#[tokio::test]
async fn model_selection_requires_an_idle_current_session() {
    let (app, mut work) = app();
    let mut request = action_request();
    request.action = Action::SelectModel {
        key: "second".into(),
    };
    app.live.lock().unwrap().snapshot.busy = true;
    assert_eq!(
        app.accept(request.clone())
            .unwrap_err()
            .into_response()
            .status(),
        StatusCode::CONFLICT
    );
    app.live.lock().unwrap().snapshot.busy = false;
    let mut stale = request.clone();
    stale.session_id = "another-session".into();
    assert_eq!(
        app.accept(stale).unwrap_err().into_response().status(),
        StatusCode::CONFLICT
    );
    assert!(work.try_recv().is_err());
    for _ in 0..2 {
        app.accept(request.clone()).unwrap();
    }
    assert_eq!(work.try_recv().unwrap().request.action, request.action);
    assert!(work.try_recv().is_err());
    let snapshot = app.snapshot().change;
    assert_eq!(snapshot["snapshot"]["model"], "test");
    assert_eq!(snapshot["snapshot"]["models"], json!(["test", "second"]));
}

#[tokio::test]
async fn stopping_cancels_the_active_turn_and_rejects_new_work() {
    let (app, mut work) = app();
    app.accept(action_request()).unwrap();
    let accepted = work.try_recv().unwrap();
    app.stop();
    assert!(accepted.cancel.is_cancelled());
    assert_eq!(
        app.accept(action_request())
            .unwrap_err()
            .into_response()
            .status(),
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert!(work.try_recv().is_err());
}

#[tokio::test]
async fn an_unavailable_worker_does_not_accept_a_turn() {
    let (app, receiver) = app();
    drop(receiver);
    assert_eq!(
        app.accept(action_request())
            .unwrap_err()
            .into_response()
            .status(),
        StatusCode::SERVICE_UNAVAILABLE
    );
    let live = app.live.lock().unwrap();
    assert!(!live.snapshot.busy);
    assert!(live.accepted.is_empty());
}

fn event_data(bytes: &[u8]) -> Value {
    let event = std::str::from_utf8(bytes).unwrap();
    let data = event
        .lines()
        .find_map(|line| line.strip_prefix("data: "))
        .unwrap();
    serde_json::from_str(data).unwrap()
}

#[tokio::test]
async fn reconnect_and_slow_observers_receive_state_without_replaying_work() {
    let (app, mut work) = app();
    let server = server(std::slice::from_ref(&app));
    app.accept(action_request()).unwrap();
    let _accepted = work.try_recv().unwrap();
    app.delta("assistant", "hello".into());
    for _ in 0..2 {
        let response = events(State(server.clone())).await.into_response();
        let mut stream = response.into_body().into_data_stream();
        let initial = event_data(&stream.next().await.unwrap().unwrap());
        assert!(initial["change"]["snapshot"]["busy"].as_bool().unwrap());
        assert_eq!(initial["change"]["snapshot"]["blocks"][0]["text"], "hello");
        for index in 0..10 {
            app.notice(format!("notice {index}"));
        }
        let latest = event_data(&stream.next().await.unwrap().unwrap());
        assert_eq!(latest["revision"], app.snapshot().revision);
        assert_eq!(latest["change"]["kind"], "snapshot");
        assert_eq!(
            latest["change"]["snapshot"]["blocks"]
                .as_array()
                .unwrap()
                .last()
                .unwrap()["text"],
            "notice 9"
        );
        assert!(work.try_recv().is_err());
    }
}

#[tokio::test]
async fn session_routes_keep_parallel_runs_and_cancellation_independent() {
    let updates = broadcast::channel(8).0;
    let (first, mut first_work) = app_for("first", updates.clone());
    let (second, mut second_work) = app_for("second", updates);
    let server = server(&[first.clone(), second.clone()]);
    let response = events(State(server.clone())).await.into_response();
    let mut stream = response.into_body().into_data_stream();
    let mut initial_ids = vec![];
    for _ in 0..2 {
        initial_ids.push(
            event_data(&stream.next().await.unwrap().unwrap())["session_id"]
                .as_str()
                .unwrap()
                .to_owned(),
        );
    }
    initial_ids.sort();
    assert_eq!(initial_ids, ["first", "second"]);
    let mut request = action_request();
    request.session_id = "first".into();
    assert_eq!(
        session_action(
            State(server.clone()),
            Path("second".into()),
            Json(request.clone())
        )
        .await
        .unwrap_err()
        .into_response()
        .status(),
        StatusCode::CONFLICT
    );
    assert!(first_work.try_recv().is_err());
    assert!(second_work.try_recv().is_err());
    for id in ["first", "second"] {
        request.session_id = id.into();
        session_action(
            State(server.clone()),
            Path(id.into()),
            Json(request.clone()),
        )
        .await
        .unwrap();
        let update = event_data(&stream.next().await.unwrap().unwrap());
        assert_eq!(update["session_id"], id);
        assert_eq!(update["change"]["meta"]["busy"], true);
    }
    let first_call = first_work.try_recv().unwrap();
    let second_call = second_work.try_recv().unwrap();
    session_cancel(
        State(server.clone()),
        Path("first".into()),
        Json(SessionRequest {
            session_id: "first".into(),
        }),
    )
    .await
    .unwrap();
    assert!(first_call.cancel.is_cancelled());
    assert!(!second_call.cancel.is_cancelled());
    assert_eq!(second.snapshot().change["snapshot"]["status"], "Running");
    server.sessions.stop().await;
    assert!(second_call.cancel.is_cancelled());
    assert!(first.shutdown.is_cancelled() && second.shutdown.is_cancelled());
}

#[tokio::test]
async fn browser_actions_require_the_launch_cookie_and_same_origin() {
    let app = server(&[]);
    let mut headers = HeaderMap::new();
    headers.insert(header::HOST, "127.0.0.1:8765".parse().unwrap());
    assert!(!allowed(&headers, &app, false));
    headers.insert(
        header::COOKIE,
        format!("other=value; {}={}", app.cookie, app.token)
            .parse()
            .unwrap(),
    );
    assert!(allowed(&headers, &app, false));
    assert!(!allowed(&headers, &app, true));
    headers.insert(header::ORIGIN, app.origin.parse().unwrap());
    assert!(allowed(&headers, &app, true));
    headers.insert(header::ORIGIN, "https://other.example".parse().unwrap());
    assert!(!allowed(&headers, &app, true));
    headers.insert(header::HOST, "other.example:8765".parse().unwrap());
    assert!(!allowed(&headers, &app, false));
}

#[test]
fn recorded_history_hides_runtime_context_and_preserves_outcomes_and_turn_times() {
    let session = Session::new("test");
    let mut thread = session.active_thread().clone();
    let runtime = Content::System {
        kind: "runtime".into(),
        text: "hidden".into(),
        data: Value::Null,
    };
    thread.messages = vec![
        Message::UserMessage {
            content: vec![runtime.clone()],
        },
        Message::UserMessage {
            content: vec![
                runtime,
                Content::Text {
                    text: "prompt".into(),
                },
            ],
        },
        Message::AssistantMessage {
            content: vec![Content::Text {
                text: "answer".into(),
            }],
            tool_uses: vec![ToolUse {
                name: "any-tool".into(),
                input: json!({"text":"input"}),
            }],
            turn_end_reason: None,
        },
        Message::ToolResults {
            tool_use_results: vec![ToolResult::text("output").with_status("exit 7")],
        },
        Message::UserMessage {
            content: vec![Content::Text {
                text: "legacy".into(),
            }],
        },
    ];
    thread
        .user_turn_timestamps
        .insert(1, "2026-09-21T22:00:00.123Z".parse().unwrap());
    let blocks = serde_json::to_value(view::history(&thread)).unwrap();
    assert_eq!(blocks.as_array().unwrap().len(), 4);
    assert_eq!(blocks[0]["text"], "prompt");
    assert_eq!(blocks[0]["time"], "2026-09-21T22:00:00Z");
    assert_eq!(blocks[1]["time"], blocks[0]["time"]);
    assert_eq!(blocks[2]["text"], "output");
    assert_eq!(blocks[2]["status"], "exit 7");
    assert_eq!(blocks[2]["error"], true);
    assert_eq!(blocks[2]["running"], false);
    assert!(blocks[3]["time"].is_null());
}
