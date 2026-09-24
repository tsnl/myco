use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
};
use futures::StreamExt;
use myco::generative_model::ToolUse;
use myco::generative_model::{Content, Message, ToolResult};

use super::super::http::{
    Server, SessionRequest, events, live_events, session_action, session_cancel,
};
use super::*;

fn app() -> (Arc<App>, mpsc::Receiver<Work>) {
    app_for("session", broadcast::channel(4).0)
}

#[test]
fn retry_discards_only_the_current_generation_and_preserves_output_and_live_tools() {
    let (app, _) = app();
    let context = myco::TraceContext::root();
    for text in ["completed answer", "completed continuation"] {
        app.emit(AgentEvent::GenerationStarted {
            context: context.clone(),
        });
        app.emit(AgentEvent::TextDelta {
            text: text.into(),
            context: context.clone(),
        });
        app.emit(AgentEvent::GenerationFinished {
            context: context.clone(),
        });
    }
    app.emit(AgentEvent::GenerationStarted {
        context: context.clone(),
    });
    app.emit(AgentEvent::TextDelta {
        text: "abandoned draft".into(),
        context: context.clone(),
    });
    app.emit(AgentEvent::ThinkingDelta {
        text: "abandoned thinking".into(),
        context: context.clone(),
    });
    app.resources(vec![process_inventory("local", "retained")]);
    app.emit(AgentEvent::Failure {
        failure: myco::generative_model::GenerationFailure::transient(
            myco::generative_model::GenerateError::ExecutionError("upstream reset".into()),
            None,
        ),
        attempt: 1,
        max_attempts: 3,
        retry_in: Some(Duration::from_millis(500)),
        context: context.clone(),
    });
    let snapshot = app.snapshot().change["snapshot"].clone();
    assert_eq!(snapshot["status"], "Retrying");
    assert_eq!(snapshot["blocks"][0]["text"], "completed answer");
    assert_eq!(snapshot["blocks"][1]["text"], "completed continuation");
    assert_eq!(snapshot["blocks"][2]["resource"]["instance_id"], "retained");
    assert_eq!(snapshot["blocks"][2]["running"], true);
    assert!(!snapshot.to_string().contains("abandoned"));
    app.emit(AgentEvent::GenerationStarted {
        context: context.clone(),
    });
    app.emit(AgentEvent::TextDelta {
        text: "replacement".into(),
        context: context.clone(),
    });
    app.emit(AgentEvent::GenerationFinished { context });
    assert_eq!(
        app.snapshot().change["snapshot"]["blocks"][4]["text"],
        "replacement"
    );
}

fn app_for(id: &str, events: broadcast::Sender<Arc<Update>>) -> (Arc<App>, mpsc::Receiver<Work>) {
    let (work, receiver) = mpsc::channel(1);
    let app = Arc::new(App {
        live: Mutex::new(Live {
            generation_start: None,
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
            background: HashMap::new(),
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
        vec![],
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
fn editing_holds_the_queue_and_saving_drains_the_latest_content_once() {
    let (app, mut work) = app();
    app.accept(action_request()).unwrap();
    work.try_recv().unwrap();
    let second = action_request();
    let third = action_request();
    app.accept(second.clone()).unwrap();
    let time = app.live.lock().unwrap().snapshot.queued[0].accepted_at;
    let edit = queue_update(&second, 0, json!({"kind":"edit"}));
    app.accept(edit.clone()).unwrap();
    app.accept(edit).unwrap();
    app.start_next(&mut app.live.lock().unwrap()).unwrap();
    app.accept(third.clone()).unwrap();
    assert!(
        work.try_recv().is_err(),
        "Later input cannot overtake an edit"
    );
    let save = queue_update(
        &second,
        1,
        json!({"kind":"save", "text":"edited", "images":[]}),
    );
    app.accept(save.clone()).unwrap();
    let next = work.try_recv().unwrap();
    assert_eq!(next.request.request_id, second.request_id);
    assert_eq!(next.accepted_at, time);
    assert_eq!(
        next.request.action,
        Action::Submit {
            text: "edited".into(),
            images: vec![]
        }
    );
    app.accept(save).unwrap();
    app.accept(second.clone()).unwrap();
    assert!(
        work.try_recv().is_err(),
        "Retries must not restore the old payload"
    );
    assert!(matches!(
        app.accept(queue_update(&second, 1, json!({"kind":"remove"}))),
        Err(Error::Conflict(_))
    ));
    app.start_next(&mut app.live.lock().unwrap()).unwrap();
    assert_eq!(work.try_recv().unwrap().request, third);
}

fn queue_update(message: &ActionRequest, revision: u64, update: Value) -> ActionRequest {
    serde_json::from_value(json!({
        "request_id": Uuid::new_v4(), "session_id": message.session_id,
        "action": {"kind":"update_queued", "message_id":message.request_id, "revision":revision, "update":update}
    })).unwrap()
}

#[test]
fn stale_queue_edits_and_invalid_saves_preserve_the_held_message() {
    let (app, mut work) = app();
    app.accept(action_request()).unwrap();
    work.try_recv().unwrap();
    let queued = action_request();
    app.accept(queued.clone()).unwrap();
    app.accept(queue_update(&queued, 0, json!({"kind":"edit"})))
        .unwrap();
    for update in [
        json!({"kind":"remove"}),
        json!({"kind":"resume"}),
        json!({"kind":"save", "text":"stale"}),
    ] {
        assert!(matches!(
            app.accept(queue_update(&queued, 0, update)),
            Err(Error::Conflict(_))
        ));
    }
    assert!(matches!(
        app.accept(queue_update(&queued, 1, json!({"kind":"save", "text":" "}))),
        Err(Error::Invalid(_))
    ));
    app.start_next(&mut app.live.lock().unwrap()).unwrap();
    assert!(work.try_recv().is_err());
    assert_eq!(
        app.snapshot().change["snapshot"]["queued"][0]["text"],
        "task"
    );
    app.accept(queue_update(&queued, 1, json!({"kind":"resume"})))
        .unwrap();
    assert_eq!(work.try_recv().unwrap().request, queued);
}

#[test]
fn unqueue_during_cancellation_prevents_delivery_and_retries_do_not_restore_it() {
    let (app, mut work) = app();
    app.accept(action_request()).unwrap();
    let current = work.try_recv().unwrap();
    let removed = action_request();
    let kept = action_request();
    app.accept(removed.clone()).unwrap();
    app.accept(kept.clone()).unwrap();
    app.cancel("session").unwrap();
    let remove = queue_update(&removed, 0, json!({"kind":"remove"}));
    app.accept(remove.clone()).unwrap();
    app.accept(remove).unwrap();
    app.accept(removed).unwrap();
    assert!(current.cancel.is_cancelled());
    app.start_next(&mut app.live.lock().unwrap()).unwrap();
    let next = work.try_recv().unwrap();
    assert_eq!(next.request, kept);
    assert!(!next.cancel.is_cancelled());
}

#[test]
fn claimed_followups_reject_mutation_and_failed_delivery_leaves_them_editable() {
    let (app, mut work) = app();
    app.accept(action_request()).unwrap();
    work.try_recv().unwrap();
    let queued = action_request();
    app.accept(queued.clone()).unwrap();
    let claimed = app.claim_followup().unwrap();
    for update in [
        json!({"kind":"edit"}),
        json!({"kind":"remove"}),
        json!({"kind":"save", "text":"too late"}),
    ] {
        assert!(matches!(
            app.accept(queue_update(&queued, 0, update)),
            Err(Error::Conflict(_))
        ));
    }
    assert_eq!(
        app.snapshot().change["snapshot"]["queued"][0]["state"],
        "sending"
    );
    app.restore_followup(claimed.request_id);
    app.accept(queue_update(
        &queued,
        0,
        json!({"kind":"save", "text":"retry content"}),
    ))
    .unwrap();
    let retried = app.claim_followup().unwrap();
    assert_eq!(retried.text, "retry content");
    assert_eq!(retried.accepted_at, claimed.accepted_at);
    app.finish_followup(retried.request_id, None);
    assert!(app.claim_followup().is_none());
    app.accept(queued).unwrap();
    assert!(app.claim_followup().is_none());
}

#[test]
fn queue_delivery_and_unqueue_have_one_atomic_winner() {
    for _ in 0..32 {
        let (app, mut work) = app();
        app.accept(action_request()).unwrap();
        work.try_recv().unwrap();
        let queued = action_request();
        app.accept(queued.clone()).unwrap();
        let barrier = std::sync::Barrier::new(2);
        let remove = queue_update(&queued, 0, json!({"kind":"remove"}));
        std::thread::scope(|scope| {
            let delivery = scope.spawn(|| {
                barrier.wait();
                app.claim_followup()
            });
            barrier.wait();
            let removed = app.accept(remove);
            match delivery.join().unwrap() {
                Some(claimed) => {
                    assert!(matches!(removed, Err(Error::Conflict(_))));
                    assert_eq!(claimed.request_id, queued.request_id);
                    app.finish_followup(claimed.request_id, None);
                }
                None => assert!(removed.is_ok()),
            }
        });
        assert_eq!(app.snapshot().change["snapshot"]["queued"], json!([]));
    }
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
    for (index, tool) in [&first, &second].into_iter().enumerate() {
        app.emit(AgentEvent::ToolStarted {
            call_id: Uuid::from_u128(index as u128),
            background: CancelToken::new(),
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
        call_id: Uuid::from_u128(1),
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
fn background_controls_target_one_call_and_reject_stale_or_foreign_requests() {
    let (app, _) = app();
    let tool = ToolUse {
        name: "bash".into(),
        input: json!({"command":"sleep 10"}),
    };
    let first = Uuid::new_v4();
    let second = Uuid::new_v4();
    let first_signal = CancelToken::new();
    let second_signal = CancelToken::new();
    for (id, signal) in [
        (first, first_signal.clone()),
        (second, second_signal.clone()),
    ] {
        app.emit(AgentEvent::ToolStarted {
            call_id: id,
            tool_use: tool.clone(),
            background: signal,
            context: Default::default(),
        });
    }
    assert!(app.background("foreign", second).is_err());
    assert!(!second_signal.is_cancelled());
    app.background("session", second).unwrap();
    assert!(!first_signal.is_cancelled());
    assert!(second_signal.is_cancelled());
    app.emit(AgentEvent::ToolFinished {
        call_id: second,
        tool_use: tool,
        result: ToolResult::text("retained").with_status("backgrounded"),
        context: Default::default(),
    });
    let blocks = app.snapshot().change["snapshot"]["blocks"].clone();
    assert_eq!(blocks[0]["running"], true);
    assert_eq!(blocks[1]["status"], "backgrounded");
    assert!(app.background("session", second).is_err());
    app.live.lock().unwrap().snapshot.status = "Cancelling".into();
    assert!(app.background("session", first).is_err());
    assert!(!first_signal.is_cancelled());
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
            call_id: Uuid::nil(),
            background: CancelToken::new(),
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
        call_id: Uuid::nil(),
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

fn process_inventory(host: &str, instance: &str) -> myco::core::HostResources {
    myco::core::HostResources {
        host: host.into(),
        error: None,
        resources: Some(vec![myco::core::ToolResource {
            tool: "bash".into(),
            id: "shell".into(),
            details: json!({"instance_id":instance, "command":"cat", "started_at":Utc::now().timestamp_millis() - 5000,
                "output_closed":false, "exit_code":null, "exit_signal":null, "duration_ms":null}),
        }]),
    }
}

#[test]
fn live_process_cards_survive_compaction_and_keep_exit_results_after_close() {
    let (app, _) = app();
    let mut observed = process_inventory("remote", "first");
    app.resources(vec![observed.clone()]);
    let initial = app.snapshot().change["snapshot"]["blocks"].clone();
    assert_eq!(initial[0]["running"], true);
    assert!(initial[0]["elapsed_ms"].as_u64().unwrap() >= 5000);
    let mut compacted = vec![];
    view::retain_processes(&mut compacted, &app.live.lock().unwrap().snapshot.blocks);
    assert_eq!(compacted.len(), 1);
    app.live.lock().unwrap().snapshot.blocks = compacted;
    observed.resources.as_mut().unwrap()[0].details["output_closed"] = json!(true);
    observed.resources.as_mut().unwrap()[0].details["exit_code"] = json!(7);
    observed.resources.as_mut().unwrap()[0].details["duration_ms"] = json!(6500);
    app.resources(vec![observed.clone()]);
    let ended = app.snapshot().change["snapshot"]["blocks"][0].clone();
    assert_eq!(ended["running"], false);
    assert_eq!(ended["status"], "exit 7");
    assert_eq!(ended["error"], true);
    assert_eq!(ended["elapsed_ms"], 6500);
    observed.resources = Some(vec![]);
    app.resources(vec![observed]);
    assert_eq!(app.snapshot().change["snapshot"]["blocks"][0], ended);
}

#[test]
fn resource_observations_distinguish_hosts_reused_handles_and_unknown_connections() {
    let (app, _) = app();
    app.resources(vec![
        process_inventory("local", "first"),
        process_inventory("remote", "second"),
    ]);
    assert_eq!(
        app.snapshot().change["snapshot"]["blocks"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    let mut remote = process_inventory("remote", "second");
    remote.error = Some("connection lost".into());
    app.resources(vec![process_inventory("local", "first"), remote]);
    let blocks = app.snapshot().change["snapshot"]["blocks"].clone();
    assert_eq!(blocks[0]["running"], true);
    assert_eq!(blocks[1]["running"], false);
    assert_eq!(blocks[1]["status"], "state unknown");
    app.resources(vec![
        process_inventory("local", "third"),
        process_inventory("remote", "second"),
    ]);
    let blocks = app.snapshot().change["snapshot"]["blocks"].clone();
    assert_eq!(blocks.as_array().unwrap().len(), 3);
    assert_eq!(blocks[0]["running"], false);
    assert_eq!(blocks[0]["status"], "not running");
    assert_eq!(blocks[1]["running"], true);
    assert_eq!(blocks[2]["running"], true);
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
async fn live_observers_receive_invalidations_instead_of_every_sessions_history() {
    let updates = broadcast::channel(4).0;
    let apps: Vec<_> = (0..26)
        .map(|index| app_for(&index.to_string(), updates.clone()).0)
        .collect();
    for app in &apps {
        app.delta("assistant", "long history ".repeat(10_000));
    }
    let response = live_events(State(server(&apps))).await.into_response();
    let mut stream = response.into_body().into_data_stream();
    let connected = stream.next().await.unwrap().unwrap();
    assert_eq!(&connected[..], b": connected\n\n");

    apps[0].delta("assistant", "fresh delta".into());
    let delta = event_data(&stream.next().await.unwrap().unwrap());
    assert_eq!(delta["session_id"], "0");
    assert_eq!(delta["change"]["kind"], "append");
    assert_eq!(delta["change"]["text"], "fresh delta");

    assert!(updates.send(Arc::new(apps[0].snapshot())).is_ok());
    let replacement = stream.next().await.unwrap().unwrap();
    assert!(
        replacement.len() < 200,
        "full histories must be fetched explicitly"
    );
    let replacement = event_data(&replacement);
    assert_eq!(replacement["session_id"], "0");
    assert_eq!(replacement["revision"], apps[0].snapshot().revision);
    assert_eq!(replacement["change"], json!({"kind":"refresh"}));
}

#[tokio::test]
async fn lagged_live_observers_drop_stale_deltas_and_request_one_resync() {
    let (app, mut work) = app();
    let response = live_events(State(server(std::slice::from_ref(&app))))
        .await
        .into_response();
    let mut stream = response.into_body().into_data_stream();
    stream.next().await.unwrap().unwrap();
    for index in 0..10 {
        app.notice(format!("notice {index}"));
    }
    let resync = event_data(&stream.next().await.unwrap().unwrap());
    assert_eq!(resync, json!({"kind":"resync"}));

    app.status("Running");
    let fresh = event_data(&stream.next().await.unwrap().unwrap());
    assert_eq!(fresh["revision"], app.snapshot().revision);
    assert_eq!(fresh["change"]["kind"], "meta");
    assert_eq!(fresh["change"]["meta"]["status"], "Running");
    assert!(
        work.try_recv().is_err(),
        "resynchronizing must never replay work"
    );
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
fn compaction_reports_activity_and_separates_tool_first_continuation_without_a_message() {
    let (app, _) = app();
    app.delta("assistant", "Prior answer".into());
    app.compacting();
    let pending = app.snapshot().change["snapshot"].clone();
    assert_eq!(pending["status"], "Compacting");
    assert_eq!(pending["blocks"].as_array().unwrap().len(), 1);
    app.finish_compaction();
    let completed = app.snapshot().change["snapshot"].clone();
    assert_eq!(completed["status"], "Running");
    assert_eq!(completed["blocks"].as_array().unwrap().len(), 2);
    assert_eq!(completed["blocks"][1]["kind"], "boundary");
    assert!(completed["blocks"][1]["text"].is_null());
    app.emit(AgentEvent::ToolStarted {
        call_id: Uuid::nil(),
        background: CancelToken::new(),
        tool_use: ToolUse {
            name: "bash".into(),
            input: json!({"command":"pwd"}),
        },
        context: Default::default(),
    });
    let blocks = app.snapshot().change["snapshot"]["blocks"].clone();
    assert_eq!(blocks[0]["text"], "Prior answer");
    assert_eq!(blocks[2]["kind"], "assistant_heading");
    assert_eq!(blocks[2]["time"], blocks[1]["time"]);
    assert_eq!(blocks[3]["kind"], "tool");
    app.live.lock().unwrap().snapshot.status = "Cancelling".into();
    app.finish_compaction();
    assert_eq!(app.snapshot().change["snapshot"]["status"], "Cancelling");
}

#[test]
fn failed_compaction_replaces_progress_with_the_warning_and_restores_running_status() {
    let (app, _) = app();
    app.compacting();
    app.warning("Summary failed; continuing with the existing context".into());
    let snapshot = app.snapshot().change["snapshot"].clone();
    assert_eq!(snapshot["status"], "Running");
    assert_eq!(snapshot["blocks"].as_array().unwrap().len(), 1);
    assert_eq!(snapshot["blocks"][0]["kind"], "notice");
    assert!(
        snapshot["blocks"][0]["text"]
            .as_str()
            .unwrap()
            .contains("Summary failed")
    );
}

#[test]
fn completed_compaction_boundary_follows_retained_context_and_precedes_new_output() {
    let user = Message::UserMessage {
        content: vec![Content::Text {
            text: "task".into(),
        }],
    };
    let assistant = |text: &str| Message::AssistantMessage {
        content: vec![Content::Text { text: text.into() }],
        tool_uses: vec![],
        turn_end_reason: None,
    };
    let continuation = Message::UserMessage {
        content: vec![Content::System {
            kind: "continuation".into(),
            text: "internal resumption instructions".into(),
            data: json!({"reason":"auto_compaction"}),
        }],
    };
    let mut session = Session::new("test");
    // Retained internal continuation instructions must stay hidden too.
    session.replace_context(
        vec![
            user,
            assistant("old output"),
            continuation.clone(),
            assistant("recent output"),
        ],
        None,
    );
    for automatic in [false, true] {
        let (mut thread, _) = myco::session::compact_thread(&session, "internal summary").unwrap();
        if automatic {
            thread.messages.push(continuation.clone());
        }
        thread.messages.push(assistant("new output"));
        let blocks = serde_json::to_value(view::history(&thread)).unwrap();
        assert_eq!(blocks[2]["text"], "recent output");
        assert_eq!(blocks[3]["kind"], "boundary");
        assert_eq!(blocks[4]["text"], "new output");
        assert_eq!(blocks[4]["time"], view::timestamp(&thread.created_at));
        assert!(!blocks.to_string().contains("internal"));
    }
}

#[test]
fn legacy_compaction_hides_internal_context_without_guessing_a_boundary() {
    let mut session = Session::new("test");
    session.replace_context(
        vec![Message::UserMessage {
            content: vec![Content::Text {
                text: "retained task".into(),
            }],
        }],
        None,
    );
    for metadata in [json!({}), json!({"tail_messages":u64::MAX})] {
        let (mut thread, _) = myco::session::compact_thread(&session, "summary").unwrap();
        if let Message::UserMessage { content } = &mut thread.messages[0]
            && let Content::System { data, .. } = &mut content[1]
        {
            *data = metadata;
        }
        let blocks = serde_json::to_value(view::history(&thread)).unwrap();
        assert_eq!(blocks.as_array().unwrap().len(), 1);
        assert_eq!(blocks[0]["text"], "retained task");
    }
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
fn prelude_notices_stay_hidden_in_user_messages_and_tool_results() {
    let text = |text: &str| Content::Text { text: text.into() };
    let notices = vec![
        Content::System {
            kind: "generation_notice".into(),
            text: "internal update".into(),
            data: Value::Null,
        },
        text(
            "\n\n[myco: Prelude changes]\nThe prelude has changed since the snapshot in your context. Files under the profile's workspace/prelude/:\n- added: hidden.md",
        ),
        text(
            "\n\n[myco: Prelude changes]\nThis context may omit earlier prelude updates. Use prelude action=list to reload the full current prelude before relying on the old snapshot.",
        ),
    ];
    let mut session = Session::new("test");
    let mut user_content = vec![text("Explain [myco: Prelude changes]")];
    user_content.extend(notices.clone());
    let mut result = ToolResult::text("actual output");
    result.content.extend(notices.clone());
    session.replace_context(
        vec![
            Message::UserMessage { content: notices },
            Message::UserMessage {
                content: user_content,
            },
            Message::AssistantMessage {
                content: vec![],
                tool_uses: vec![ToolUse {
                    name: "bash".into(),
                    input: json!({"command":"pwd"}),
                }],
                turn_end_reason: None,
            },
            Message::ToolResults {
                tool_use_results: vec![result],
            },
        ],
        None,
    );
    let blocks = serde_json::to_value(view::history(session.active_thread())).unwrap();
    assert_eq!(blocks.as_array().unwrap().len(), 3);
    assert_eq!(blocks[0]["text"], "Explain [myco: Prelude changes]");
    assert_eq!(blocks[1]["kind"], "assistant_heading");
    assert_eq!(blocks[2]["text"], "actual output");
    assert!(!blocks.to_string().contains("hidden.md"));
    assert!(!blocks.to_string().contains("internal update"));
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
