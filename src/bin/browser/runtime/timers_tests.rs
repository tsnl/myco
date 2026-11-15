use std::time::Duration;

use super::queue::MAX_QUEUED_MESSAGES;
use super::tests::app;
use super::*;
use myco::generative_model::{Content, Message, ToolUse};
use myco::tool_services::{HostDispatchContext, ToolService};

async fn tool(app: &Arc<App>, input: Value) -> myco::generative_model::ToolResult {
    call(app, "timer", input).await
}

async fn call(app: &Arc<App>, name: &str, input: Value) -> myco::generative_model::ToolResult {
    app.clone()
        .dispatch_tool_use(
            ToolUse {
                name: name.into(),
                input,

                thought_signature: None,
            },
            HostDispatchContext::new(Uuid::new_v4(), CancelToken::new()),
        )
        .await
}

#[tokio::test(start_paused = true)]
async fn stable_timer_ids_preserve_exact_absolute_times_and_retry_after_the_due_time() {
    let (app, _work) = app();
    app.live.lock().unwrap().snapshot.meta.cancel = Some(CancelToken::new());
    let id = Uuid::new_v4();
    let at = (app.clock.now() + chrono::Duration::seconds(1)).to_rfc3339();
    let input = json!({"action":"set", "event_id":id, "at":at, "message":"Once"});
    assert!(!tool(&app, input.clone()).await.is_error);
    tokio::time::advance(Duration::from_secs(2)).await;
    assert!(!tool(&app, input).await.is_error);
    assert_eq!(app.inbox_entries().len(), 1);
    assert_eq!(app.inbox_entries()[0].due_at.to_rfc3339(), at);
    assert!(
        tool(
            &app,
            json!({"action":"set", "event_id":id, "after_seconds":1, "message":"Once"})
        )
        .await
        .is_error
    );
}

#[tokio::test(start_paused = true)]
async fn timer_wakes_an_idle_session_once_without_a_browser_connection() {
    let (app, mut work) = app();
    let timer = app
        .set_timer(Some(5.0), None, "Check the build".into())
        .unwrap();
    assert!(!app.snapshot().change["snapshot"]["busy"].as_bool().unwrap());
    app.fire_timers();
    assert!(work.try_recv().is_err());
    tokio::time::advance(Duration::from_secs(5)).await;
    app.fire_timers();
    let work = work.try_recv().unwrap();
    let Action::Timer { message } = work.request.action else {
        panic!("expected timer wakeup")
    };
    assert_eq!(message.timer.unwrap().id, timer.id);
    assert_eq!(message.text, "Check the build");
    assert!(
        app.snapshot().change["snapshot"]["timers"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    app.fire_timers();
    assert!(
        app.snapshot().change["snapshot"]["queued"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}

#[tokio::test(start_paused = true)]
async fn due_timers_queue_behind_user_input_and_can_be_cancelled_before_delivery() {
    let (app, _work) = app();
    app.live.lock().unwrap().snapshot.meta.cancel = Some(CancelToken::new());
    app.accept(
        serde_json::from_value(json!({"request_id":Uuid::new_v4(),"session_id":"session",
        "action":{"kind":"submit","text":"User followup"}}))
        .unwrap(),
    )
    .unwrap();
    let timer = app
        .set_timer(Some(1.0), None, "Timer followup".into())
        .unwrap();
    tokio::time::advance(Duration::from_secs(1)).await;
    app.fire_timers();
    assert_eq!(
        app.snapshot().change["snapshot"]["queued"][0]["text"],
        "User followup"
    );
    assert_eq!(
        app.snapshot().change["snapshot"]["queued"][1]["text"],
        "Timer followup"
    );
    app.cancel_timer(timer.id).unwrap();
    assert_eq!(
        app.snapshot().change["snapshot"]["queued"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert!(
        app.cancel_timer(timer.id).is_ok(),
        "cancellation retries are idempotent"
    );
}

#[tokio::test(start_paused = true)]
async fn full_queue_retains_due_timers_and_delivery_respects_deadline_order() {
    let (app, _work) = app();
    app.live.lock().unwrap().snapshot.meta.cancel = Some(CancelToken::new());
    for _ in 0..MAX_QUEUED_MESSAGES {
        app.accept(
            serde_json::from_value(json!({"request_id":Uuid::new_v4(), "session_id":"session",
            "action":{"kind":"submit", "text":"Queued"}}))
            .unwrap(),
        )
        .unwrap();
    }
    let late = app.set_timer(Some(5.0), None, "Later".into()).unwrap();
    let early = app.set_timer(Some(1.0), None, "Earlier".into()).unwrap();
    tokio::time::advance(Duration::from_secs(5)).await;
    app.fire_timers();
    assert_eq!(
        app.snapshot().change["snapshot"]["timers"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    app.live.lock().unwrap().snapshot.meta.queued.clear();
    app.fire_timers();
    let first = app.claim_followup().unwrap().unwrap();
    assert_eq!(first.timer.unwrap().id, early.id);
    assert!(
        app.cancel_timer(early.id).is_err(),
        "claimed delivery must not be cancelled"
    );
    app.finish_followup(first.request_id, None);
    let second = app.claim_followup().unwrap().unwrap();
    assert_eq!(second.timer.unwrap().id, late.id);
}

#[tokio::test(start_paused = true)]
async fn shutdown_and_cancellation_do_not_start_timer_work() {
    let (app, mut work) = app();
    app.set_timer(Some(1.0), None, "Later".into()).unwrap();
    app.live.lock().unwrap().snapshot.meta.status = Status::Cancelling;
    tokio::time::advance(Duration::from_secs(2)).await;
    app.fire_timers();
    assert!(work.try_recv().is_err());
    assert_eq!(
        app.snapshot().change["snapshot"]["timers"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    app.live.lock().unwrap().snapshot.meta.status = Status::Ready;
    app.stop();
    app.fire_timers();
    assert!(work.try_recv().is_err());
    assert!(app.set_timer(Some(1.0), None, "No worker".into()).is_err());
}

#[tokio::test]
async fn timer_tool_validates_deadlines_and_arguments_without_mutating_the_queue() {
    let (app, _work) = app();
    for input in [
        json!({"action":"set", "message":"Missing time"}),
        json!({"action":"set", "after_seconds":-1, "message":"Past"}),
        json!({"action":"set", "after_seconds":0, "message":"Zero"}),
        json!({"action":"set", "after_seconds":2_592_001, "message":"Too far"}),
        json!({"action":"set", "after_seconds":1, "message":" "}),
        json!({"action":"set", "after_seconds":1, "message":"x".repeat(8001)}),
        json!({"action":"set", "after_seconds":1, "at":"2020-01-01T00:00:00Z", "message":"Both"}),
        json!({"action":"set", "at":"2020-01-01T00:00:00Z", "message":"Past"}),
        json!({"action":"set", "at":"2026-09-25T09:00:00", "message":"No timezone"}),
        json!({"action":"list", "message":"Ignored?"}),
        json!({"action":"cancel", "timer_id":"invalid"}),
        json!({"action":"set", "after_seconds":1, "message":"Unknown", "extra":true}),
    ] {
        assert!(tool(&app, input.clone()).await.is_error, "{input}");
    }
    assert!(
        app.snapshot().change["snapshot"]["timers"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let at = (Utc::now() + chrono::Duration::minutes(2))
        .with_timezone(&chrono::FixedOffset::west_opt(7 * 3600).unwrap())
        .to_rfc3339();
    let result = tool(
        &app,
        json!({"action":"set", "at":at, "message":"Review build"}),
    )
    .await;
    assert!(!result.is_error, "{result:?}");
    let id = app.snapshot().change["snapshot"]["timers"][0]["id"].clone();
    assert!(!tool(&app, json!({"action":"list"})).await.is_error);
    assert!(
        !tool(&app, json!({"action":"cancel", "timer_id":id}))
            .await
            .is_error
    );
    assert!(serde_json::from_value::<Action>(json!({"kind":"timer", "message":{}})).is_err());
}

#[tokio::test(start_paused = true)]
async fn timer_followups_are_labeled_system_messages_live_and_after_reload() {
    let (app, _work) = app();
    app.live.lock().unwrap().snapshot.meta.cancel = Some(CancelToken::new());
    app.set_timer(Some(0.1), None, "Check build".into())
        .unwrap();
    tokio::time::advance(Duration::from_secs(1)).await;
    app.fire_timers();
    let message = app.claim_followup().unwrap().unwrap();
    let content = message.content(1000).unwrap();
    assert!(matches!(&content[0], Content::System { kind, .. } if kind == "timer"));
    let live = Block::input(&content, Some(view::timestamp(&message.accepted_at)));
    let mut thread = Session::new("test").active_thread().clone();
    thread.messages.push(Message::UserMessage { content });
    thread.user_turn_timestamps.insert(0, message.accepted_at);
    assert_eq!(
        serde_json::to_value(&live).unwrap(),
        serde_json::to_value(&view::history(&thread)[0]).unwrap()
    );
    assert_eq!(serde_json::to_value(live).unwrap()["role"], "system");
}

#[tokio::test(start_paused = true)]
async fn timer_ownership_and_limits_include_fired_messages_waiting_for_delivery() {
    let (first, _work) = app();
    let (second, _other_work) = app();
    let timer = first.set_timer(Some(1.0), None, "Owned".into()).unwrap();
    assert!(second.cancel_timer(timer.id).is_err());
    for _ in 1..20 {
        first.set_timer(Some(1.0), None, "Pending".into()).unwrap();
    }
    first.live.lock().unwrap().snapshot.meta.cancel = Some(CancelToken::new());
    tokio::time::advance(Duration::from_secs(1)).await;
    first.fire_timers();
    assert_eq!(
        first.snapshot().change["snapshot"]["queued"]
            .as_array()
            .unwrap()
            .len(),
        20
    );
    assert!(first.set_timer(Some(1.0), None, "Overflow".into()).is_err());
    first.cancel_timer(timer.id).unwrap();
    assert!(
        first
            .set_timer(Some(1.0), None, "Replacement".into())
            .is_ok()
    );
    let cancel = CancelToken::new();
    cancel.cancel();
    let result = second
        .clone()
        .dispatch_tool_use(
            ToolUse {
                name: "timer".into(),
                input: json!({"action":"set", "after_seconds":1, "message":"Cancelled call"}),
                thought_signature: None,
            },
            HostDispatchContext::new(Uuid::new_v4(), cancel),
        )
        .await;
    assert!(result.is_error);
    assert!(
        second.snapshot().change["snapshot"]["timers"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}

/// Providers reject a top-level `oneOf`, so each tool is one flat object that
/// names only its own actions and identifiers.
#[test]
fn timer_and_inbox_schemas_are_flat_and_offer_only_their_own_actions() {
    let (app, _work) = app();
    let specs = app.tool_specs();
    let schema = |name: &str| {
        let spec = specs.iter().find(|spec| spec.name == name).unwrap();
        spec.input_schema.clone()
    };
    let (timer, inbox) = (schema("timer"), schema("inbox"));
    for schema in [&timer, &inbox] {
        assert_eq!(schema["type"], "object", "{schema}");
        assert!(schema.get("oneOf").is_none() && schema.get("anyOf").is_none());
    }
    assert_eq!(
        timer["properties"]["action"]["enum"],
        json!(["set", "list", "cancel"])
    );
    assert_eq!(
        inbox["properties"]["action"]["enum"],
        json!(["post", "list", "cancel"])
    );
    assert!(timer["properties"].get("timer_id").is_some());
    assert!(inbox["properties"].get("timer_id").is_none());
}

#[tokio::test]
async fn timer_and_inbox_calls_accept_their_own_shapes_and_reject_the_other_tool_s() {
    let (app, _work) = app();
    let at = (Utc::now() + chrono::Duration::minutes(5)).to_rfc3339();
    for (name, input) in [
        (
            "timer",
            json!({"action":"post", "after_seconds":1, "message":"Wrong tool"}),
        ),
        (
            "inbox",
            json!({"action":"set", "after_seconds":1, "message":"Wrong tool"}),
        ),
        (
            "timer",
            json!({"action":"set", "after_seconds":1, "message":"Id", "timer_id":Uuid::new_v4()}),
        ),
        ("timer", json!({"action":"list", "timer_id":Uuid::new_v4()})),
        (
            "timer",
            json!({"action":"cancel", "event_id":Uuid::new_v4()}),
        ),
        ("timer", json!({"action":"cancel"})),
        (
            "inbox",
            json!({"action":"post", "message":"Id", "timer_id":Uuid::new_v4()}),
        ),
        ("inbox", json!({"action":"list", "event_id":Uuid::new_v4()})),
        (
            "inbox",
            json!({"action":"cancel", "event_id":Uuid::new_v4(), "message":"Extra"}),
        ),
        ("inbox", json!({"action":"cancel"})),
        (
            "inbox",
            json!({"action":"post", "event_id":Uuid::new_v4(), "after_seconds":1, "message":"Relative retry"}),
        ),
    ] {
        assert!(
            call(&app, name, input.clone()).await.is_error,
            "{name} {input}"
        );
    }
    assert!(app.inbox_entries().is_empty());

    let event_id = Uuid::new_v4();
    let posted = json!({"action":"post", "event_id":event_id, "at":at, "message":"Scheduled",
        "priority":"urgent", "expires_at":(Utc::now() + chrono::Duration::hours(1)).to_rfc3339()});
    assert!(!call(&app, "inbox", posted).await.is_error);
    let immediate = call(&app, "inbox", json!({"action":"post", "message":"Now"})).await;
    assert!(!immediate.is_error, "{immediate:?}");
    let timer = call(
        &app,
        "timer",
        json!({"action":"set", "after_seconds":60, "message":"Later"}),
    )
    .await;
    assert!(!timer.is_error, "{timer:?}");
    let listed = |result: myco::generative_model::ToolResult| match &result.content[..] {
        [Content::Text { text }] => serde_json::from_str::<Vec<Value>>(text).unwrap().len(),
        _ => panic!("{result:?}"),
    };
    assert_eq!(
        listed(call(&app, "inbox", json!({"action":"list"})).await),
        3
    );
    assert_eq!(
        listed(call(&app, "timer", json!({"action":"list"})).await),
        1
    );
    let cancelled = json!({"action":"cancel", "event_id":event_id});
    assert!(!call(&app, "inbox", cancelled).await.is_error);
}
