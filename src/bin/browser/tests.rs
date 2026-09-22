use futures::StreamExt;
use myco::generative_model::{Content, Message, ToolResult};

use super::*;

fn app() -> (Arc<App>, mpsc::Receiver<Work>) {
    let (work, receiver) = mpsc::channel(1);
    let app = Arc::new(App {
        token: "test-token".into(),
        origin: "http://127.0.0.1:8765".into(),
        cookie: "myco_8765".into(),
        live: Mutex::new(Live {
            snapshot: Snapshot {
                revision: 0,
                session_id: "session".into(),
                thread_id: "thread".into(),
                title: "Test".into(),
                model: "test".into(),
                busy: false,
                status: "Ready".into(),
                blocks: vec![],
            },
            cancel: None,
            accepted: HashMap::new(),
        }),
        events: broadcast::channel(4).0,
        work,
        shutdown: CancelToken::new(),
    });
    (app, receiver)
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

#[tokio::test]
async fn retries_keep_one_action_and_its_original_cancellation() {
    let (app, mut work) = app();
    let request = action_request();
    for _ in 0..2 {
        assert_eq!(
            action(State(app.clone()), Json(request.clone()))
                .await
                .unwrap(),
            StatusCode::ACCEPTED
        );
    }
    let accepted = work.try_recv().unwrap();
    assert!(work.try_recv().is_err());
    let mut different = request.clone();
    different.request_id = Uuid::new_v4();
    assert_eq!(
        action(State(app.clone()), Json(different))
            .await
            .unwrap_err()
            .0,
        StatusCode::CONFLICT
    );
    cancel(
        State(app.clone()),
        Json(SessionRequest {
            session_id: "session".into(),
        }),
    )
    .await
    .unwrap();
    assert!(accepted.cancel.is_cancelled());
    app.live.lock().unwrap().snapshot.busy = false;
    action(State(app.clone()), Json(request.clone()))
        .await
        .unwrap();
    assert!(
        work.try_recv().is_err(),
        "A completed request must not run again"
    );
    let mut reused = request;
    reused.action = Action::New;
    assert_eq!(
        action(State(app.clone()), Json(reused))
            .await
            .unwrap_err()
            .0,
        StatusCode::CONFLICT
    );
    let mut stale = action_request();
    stale.session_id = "another-session".into();
    assert_eq!(
        action(State(app.clone()), Json(stale)).await.unwrap_err().0,
        StatusCode::CONFLICT
    );
    assert!(work.try_recv().is_err());
}

#[tokio::test]
async fn stopping_cancels_the_active_turn_and_rejects_new_work() {
    let (app, mut work) = app();
    action(State(app.clone()), Json(action_request()))
        .await
        .unwrap();
    let accepted = work.try_recv().unwrap();
    app.stop();
    assert!(accepted.cancel.is_cancelled());
    assert_eq!(
        action(State(app.clone()), Json(action_request()))
            .await
            .unwrap_err()
            .0,
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert!(work.try_recv().is_err());
}

#[tokio::test]
async fn an_unavailable_worker_does_not_accept_a_turn() {
    let (app, receiver) = app();
    drop(receiver);
    assert_eq!(
        action(State(app.clone()), Json(action_request()))
            .await
            .unwrap_err()
            .0,
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
    action(State(app.clone()), Json(action_request()))
        .await
        .unwrap();
    let _accepted = work.try_recv().unwrap();
    app.delta("assistant", "hello".into());
    for _ in 0..2 {
        let response = events(State(app.clone())).await.into_response();
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

#[test]
fn browser_actions_require_the_launch_cookie_and_same_origin() {
    let (app, _) = app();
    let mut headers = HeaderMap::new();
    headers.insert(header::HOST, "127.0.0.1:8765".parse().unwrap());
    assert!(!allowed(&headers, &app, false));
    headers.insert(
        header::COOKIE,
        "other=value; myco_8765=test-token".parse().unwrap(),
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
