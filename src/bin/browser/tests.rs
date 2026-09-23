use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
};
use futures::StreamExt;
use myco::generative_model::{Content, Message, ToolResult};

use super::super::http::{Server, SessionRequest, events, session_action, session_cancel};
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
                attachment_limits: attachments::Limits::new(
                    myco::config::DEFAULT_MAX_IMAGE_BASE64_BYTES,
                ),
                usage: None,
                context_window_tokens: 100_000,
                busy: false,
                status: "Ready".into(),
                tasks: vec![],
                queued: VecDeque::new(),
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
        myco::ConfigUserSettings {
            config_path: Some("/unused/config.toml".into()),
            ..Default::default()
        },
        |_| None,
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
        super::super::files::Files::open(&std::env::current_dir().unwrap()).unwrap(),
    ))
}

fn action_request() -> ActionRequest {
    ActionRequest {
        request_id: Uuid::new_v4(),
        session_id: "session".into(),
        action: Action::Submit {
            text: "task".into(),
            images: vec![],
        },
    }
}

#[test]
fn queued_messages_run_in_order_once_and_preserve_acceptance_time() {
    let (app, mut work) = app();
    app.accept(action_request()).unwrap();
    let first = work.try_recv().unwrap();
    let mut second = action_request();
    second.action = Action::Submit {
        text: "second".into(),
        images: vec![],
    };
    let mut third = action_request();
    third.action = Action::Submit {
        text: "third".into(),
        images: vec![],
    };
    for request in [&second, &third, &second] {
        app.accept(request.clone()).unwrap();
    }
    assert!(
        work.try_recv().is_err(),
        "queued messages must wait for the current turn"
    );
    let snapshot = app.snapshot().change;
    assert_eq!(snapshot["snapshot"]["queued"].as_array().unwrap().len(), 2);
    assert_eq!(snapshot["snapshot"]["queued"][0]["text"], "second");
    let accepted_at = app.live.lock().unwrap().snapshot.queued[0].accepted_at;
    app.start_next(&mut app.live.lock().unwrap()).unwrap();
    let next = work.try_recv().unwrap();
    assert_eq!(next.request, second);
    assert_eq!(next.accepted_at, accepted_at);
    assert!(!first.cancel.is_cancelled());
    app.start_next(&mut app.live.lock().unwrap()).unwrap();
    assert_eq!(work.try_recv().unwrap().request, third);
    app.start_next(&mut app.live.lock().unwrap()).unwrap();
    assert_eq!(app.snapshot().change["snapshot"]["busy"], false);
    app.accept(second).unwrap();
    assert!(work.try_recv().is_err());
}

#[test]
fn cancellation_preserves_followups_and_queue_limits_leave_requests_retryable() {
    let (app, mut work) = app();
    app.accept(action_request()).unwrap();
    let current = work.try_recv().unwrap();
    for _ in 0..MAX_QUEUED_MESSAGES {
        app.accept(action_request()).unwrap();
    }
    let overflow = action_request();
    assert!(matches!(
        app.accept(overflow.clone()),
        Err(Error::Conflict(_))
    ));
    assert!(
        !app.live
            .lock()
            .unwrap()
            .accepted
            .contains_key(&overflow.request_id)
    );
    app.cancel("session").unwrap();
    assert!(current.cancel.is_cancelled());
    assert_eq!(
        app.live.lock().unwrap().snapshot.queued.len(),
        MAX_QUEUED_MESSAGES
    );
    assert!(matches!(
        app.accept(overflow.clone()),
        Err(Error::Conflict(_))
    ));
    app.start_next(&mut app.live.lock().unwrap()).unwrap();
    let next = work.try_recv().unwrap();
    assert!(!next.cancel.is_cancelled());
    assert_eq!(
        app.live.lock().unwrap().snapshot.queued.len(),
        MAX_QUEUED_MESSAGES - 1
    );
    app.accept(overflow).unwrap();
    assert!(work.try_recv().is_err());
    assert_eq!(
        app.live.lock().unwrap().snapshot.queued.len(),
        MAX_QUEUED_MESSAGES
    );
}

#[test]
fn title_changes_reach_live_metadata_and_invalidate_session_listings() {
    let (app, _) = app();
    let active = ActiveSession::new(Session::new_with_id("test", "session"));
    active.with_mut(|session| {
        session
            .set_title(Some("Renamed while running".into()))
            .unwrap()
    });
    app.live.lock().unwrap().snapshot.busy = true;
    let mut updates = app.events.subscribe();
    app.refresh(&active, vec![]);
    let update = updates.try_recv().unwrap();
    assert_eq!(update.change["meta"]["title"], "Renamed while running");
    assert_eq!(update.change["meta"]["busy"], true);
    assert_eq!(app.generation.load(Ordering::Relaxed), 1);
    app.refresh(&active, vec![]);
    assert!(updates.try_recv().is_err());
    assert_eq!(
        app.snapshot().change["snapshot"]["title"],
        "Renamed while running"
    );
}

#[test]
fn recorded_usage_reaches_live_and_reconnect_metadata_without_inventing_unknown_counts() {
    let (app, _) = app();
    let mut session = Session::new_with_id("test", "session");
    session.title = Some("Test".into());
    let active = ActiveSession::new(session);
    app.live.lock().unwrap().snapshot.busy = true;
    let mut updates = app.events.subscribe();
    for usage in [
        Some(TokenUsage {
            input_tokens: 24_321,
            output_tokens: 30,
            cached_input_tokens: 12_000,
        }),
        Some(TokenUsage::default()),
        None,
    ] {
        active.with_mut(|session| session.replace_context(vec![], usage));
        app.refresh(&active, vec![]);
        let update = updates.try_recv().unwrap();
        assert_eq!(update.change["meta"]["usage"], json!(usage));
        assert_eq!(update.change["meta"]["context_window_tokens"], 100_000);
        assert_eq!(update.change["meta"]["busy"], true);
        assert_eq!(app.snapshot().change["snapshot"]["usage"], json!(usage));
        app.refresh(&active, vec![]);
        assert!(updates.try_recv().is_err());
    }
    assert_eq!(app.generation.load(Ordering::Relaxed), 0);
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
    assert!(blocks.iter().all(|block| block["elapsed_ms"].is_u64()));
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
    assert!(blocks[1]["elapsed_ms"].is_u64());
}

#[test]
fn tool_first_replies_have_one_heading_before_parallel_tools_live_and_replayed() {
    let (app, _) = app();
    let time = "2026-09-22T22:00:00Z";
    let content = vec![Content::Text {
        text: "inspect".into(),
    }];
    app.live
        .lock()
        .unwrap()
        .snapshot
        .blocks
        .push(Block::message("user", &content, Some(time.into())));
    let tool = ToolUse {
        name: "bash".into(),
        input: json!({"command":"pwd"}),
    };
    for _ in 0..2 {
        app.emit(AgentEvent::ToolStarted {
            tool_use: tool.clone(),
            context: Default::default(),
        });
    }
    let live = app.snapshot().change["snapshot"]["blocks"].clone();
    assert_eq!(live[1]["kind"], "assistant_heading");
    assert_eq!(live[1]["time"], time);
    assert_eq!(live[2]["kind"], "tool");
    assert_eq!(live[3]["kind"], "tool");
    let mut thread = Session::new("test").active_thread().clone();
    thread.messages = vec![
        Message::UserMessage { content },
        Message::AssistantMessage {
            content: vec![],
            tool_uses: vec![tool.clone(), tool],
            turn_end_reason: None,
        },
    ];
    thread.user_turn_timestamps.insert(0, time.parse().unwrap());
    let replay = serde_json::to_value(view::history(&thread)).unwrap();
    assert_eq!(replay[1], live[1]);
    assert_eq!(replay[2]["kind"], "tool");
    assert_eq!(replay[3]["kind"], "tool");
    assert_eq!(replay.as_array().unwrap().len(), 4);
}

#[test]
fn snapshots_keep_measured_durations_for_repeated_calls_without_inventing_history_timings() {
    let tool = ToolUse {
        name: "bash".into(),
        input: json!({"command":"same command"}),
    };
    let mut observed = Vec::new();
    for age in [None, Some(3), Some(1)] {
        let started = age.map(|seconds| Instant::now() - Duration::from_secs(seconds));
        let mut block = Block::tool(tool.clone(), started);
        block.finish(&ToolResult::text("done"));
        observed.push(block);
    }
    let mut rebuilt = (0..4)
        .map(|_| {
            let mut block = Block::tool(tool.clone(), None);
            block.finish(&ToolResult::text("done"));
            block
        })
        .collect::<Vec<_>>();
    view::retain_tool_timers(&mut rebuilt, &observed);
    let expected = serde_json::to_value(observed).unwrap();
    let actual = serde_json::to_value(rebuilt).unwrap();
    assert!(actual[0].get("elapsed_ms").is_none());
    assert!(actual[3].get("elapsed_ms").is_none());
    assert!(expected[1]["elapsed_ms"].as_u64().unwrap() >= 3000);
    assert!(expected[2]["elapsed_ms"].as_u64().unwrap() >= 1000);
    assert_eq!(actual[1]["elapsed_ms"], expected[1]["elapsed_ms"]);
    assert_eq!(actual[2]["elapsed_ms"], expected[2]["elapsed_ms"]);
}

#[test]
fn running_snapshots_measure_time_since_dispatch_instead_of_time_since_reconnect() {
    let (app, _) = app();
    let tool = ToolUse {
        name: "bash".into(),
        input: json!({"command":"waiting"}),
    };
    app.live.lock().unwrap().snapshot.blocks.push(Block::tool(
        tool.clone(),
        Some(Instant::now() - Duration::from_secs(5)),
    ));
    let running = app.snapshot();
    assert!(
        running.change["snapshot"]["blocks"][0]["elapsed_ms"]
            .as_u64()
            .unwrap()
            >= 5000
    );
    app.emit(AgentEvent::ToolFinished {
        tool_use: tool,
        result: ToolResult::err("cancelled"),
        context: Default::default(),
    });
    let finished = app.snapshot().change["snapshot"]["blocks"][0].clone();
    assert_eq!(finished["running"], false);
    assert!(finished["elapsed_ms"].as_u64().unwrap() >= 5000);
    assert_eq!(
        app.snapshot().change["snapshot"]["blocks"][0]["elapsed_ms"],
        finished["elapsed_ms"]
    );
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
        let mut block = Block::tool(
            ToolUse {
                name: "any-tool".into(),
                input: Value::Null,
            },
            None,
        );
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
    different.action = Action::Compact;
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

#[test]
fn markdown_text_parts_join_without_crossing_images_thinking_or_message_boundaries() {
    let text = |value: &str| Content::Text { text: value.into() };
    let assistant = |content| Message::AssistantMessage {
        content,
        tool_uses: vec![],
        turn_end_reason: None,
    };
    let session = Session::new("test");
    let mut thread = session.active_thread().clone();
    thread.messages = vec![
        assistant(vec![
            text(""),
            text("**bold "),
            text("text**"),
            Content::Image {
                source: "data:image/png;base64,AAAA".into(),
            },
            text("after "),
            text(""),
            text("image"),
            Content::Thinking {
                text: "thinking".into(),
                signature: None,
                redacted: false,
            },
            text("after thinking"),
        ]),
        assistant(vec![text(""), text("next response")]),
    ];
    let blocks = serde_json::to_value(view::history(&thread)).unwrap();
    let blocks = blocks.as_array().unwrap();
    assert_eq!(
        blocks
            .iter()
            .map(|block| block["text"].as_str().unwrap())
            .collect::<Vec<_>>(),
        [
            "**bold text**",
            "",
            "after image",
            "thinking",
            "after thinking",
            "next response"
        ]
    );
    assert_eq!(blocks[1]["images"][0], "data:image/png;base64,AAAA");
    assert_eq!(blocks[3]["role"], "thinking");
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
    assert!(blocks[2].get("elapsed_ms").is_none());
    assert!(blocks[3]["time"].is_null());
}
