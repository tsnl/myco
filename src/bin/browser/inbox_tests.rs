use super::tests::{action_request, app};
use super::*;
use myco::generative_model::{ToolResult, ToolUse};
use myco::session::inbox::{EventRequest, Priority, Source, State};

fn request(message: &str, priority: Priority) -> EventRequest {
    EventRequest {
        event_id: Uuid::new_v4(),
        message: message.into(),
        priority,
        at: None,
        expires_at: None,
    }
}

fn running_bash(app: &App) -> (Uuid, CancelToken) {
    let id = Uuid::new_v4();
    let background = CancelToken::new();
    app.emit(AgentEvent::ToolStarted {
        call_id: id,
        tool_use: ToolUse {
            name: "bash".into(),
            input: json!({"command":"long-running-build"}),
        },
        background: background.clone(),
        context: myco::TraceContext::root(),
    });
    (id, background)
}

fn finish_bash(app: &App, id: Uuid) {
    app.emit(AgentEvent::ToolFinished {
        call_id: id,
        tool_use: ToolUse {
            name: "bash".into(),
            input: json!({"command":"long-running-build"}),
        },
        result: ToolResult::text("Retained as bg-1").with_status("backgrounded"),
        context: myco::TraceContext::root(),
    });
}

#[tokio::test]
async fn urgent_delivery_waits_for_every_bash_handoff_before_cancelling_the_turn() {
    let (app, mut work) = app();
    app.accept(action_request()).unwrap();
    let current = work.try_recv().unwrap();
    let (first, signal1) = running_bash(&app);
    let (second, signal2) = running_bash(&app);
    let urgent = request("Build needs attention", Priority::Urgent);
    app.post_event(urgent.clone(), Source::Trigger).unwrap();
    assert!(signal1.is_cancelled() && signal2.is_cancelled());
    assert!(
        !current.cancel.is_cancelled(),
        "backgrounding must win before cancellation"
    );
    finish_bash(&app, first);
    assert!(!current.cancel.is_cancelled());
    finish_bash(&app, second);
    assert!(current.cancel.is_cancelled());
    assert!(
        work.try_recv().is_err(),
        "the next turn waits for the old one to settle"
    );
    app.start_next(&mut app.live.lock().unwrap()).unwrap();
    let next = work.try_recv().unwrap();
    assert_eq!(next.request.request_id, urgent.event_id);
    assert!(!next.cancel.is_cancelled());
}

#[tokio::test]
async fn normal_events_background_bash_but_preserve_fifo_and_do_not_cancel_generation() {
    let (app, mut work) = app();
    app.accept(action_request()).unwrap();
    let current = work.try_recv().unwrap();
    let human = action_request();
    app.accept(human.clone()).unwrap();
    let (id, signal) = running_bash(&app);
    let event = request("Build complete", Priority::Normal);
    app.post_event(event.clone(), Source::Trigger).unwrap();
    assert!(signal.is_cancelled());
    finish_bash(&app, id);
    assert!(!current.cancel.is_cancelled());
    assert_eq!(
        app.claim_followup().unwrap().unwrap().request_id,
        human.request_id
    );
    app.finish_followup(human.request_id, None);
    let next = app.claim_followup().unwrap().unwrap();
    assert_eq!(next.request_id, event.event_id);
    assert!(
        matches!(&next.content(1000).unwrap()[0], myco::generative_model::Content::System { kind, .. } if kind == "inbox_event")
    );
}

#[tokio::test]
async fn urgent_events_bypass_a_full_or_held_queue_without_dropping_user_input() {
    let (app, _work) = app();
    app.live.lock().unwrap().snapshot.busy = true;
    for _ in 0..MAX_QUEUED_MESSAGES {
        app.accept(action_request()).unwrap();
    }
    app.live.lock().unwrap().snapshot.queued[0].state = queue::QueueState::Editing;
    let event = request("Time-sensitive signal", Priority::TimeSensitive);
    app.post_event(event.clone(), Source::Trigger).unwrap();
    let live = app.live.lock().unwrap();
    assert_eq!(live.snapshot.queued.len(), MAX_QUEUED_MESSAGES + 1);
    assert_eq!(live.snapshot.queued[0].request_id, event.event_id);
    assert_eq!(live.snapshot.queued[1].state, queue::QueueState::Editing);
}

#[tokio::test(start_paused = true)]
async fn expired_events_are_visible_but_cannot_preempt_or_deliver() {
    let (app, mut work) = app();
    app.accept(action_request()).unwrap();
    let current = work.try_recv().unwrap();
    let mut event = request("Stale alert", Priority::Urgent);
    event.at = Some(app.clock.now() + chrono::Duration::seconds(2));
    event.expires_at = Some(app.clock.now() + chrono::Duration::seconds(3));
    app.post_event(event.clone(), Source::Trigger).unwrap();
    tokio::time::advance(Duration::from_secs(4)).await;
    app.fire_timers();
    assert!(!current.cancel.is_cancelled());
    assert!(app.claim_followup().unwrap().is_none());
    assert_eq!(app.inbox_entries()[0].state, State::Expired);
}

#[tokio::test]
async fn retries_are_idempotent_across_claims_and_event_ids_cannot_alias_actions() {
    let (app, mut work) = app();
    let event = request("A single event", Priority::Normal);
    app.post_event(event.clone(), Source::Trigger).unwrap();
    let first = work.try_recv().unwrap();
    app.post_event(event.clone(), Source::Trigger).unwrap();
    assert!(work.try_recv().is_err());
    assert_eq!(first.request.request_id, event.event_id);
    let mut collision = action_request();
    collision.request_id = event.event_id;
    assert!(app.accept(collision).is_err());
    let human = action_request();
    app.accept(human.clone()).unwrap();
    let mut collision = request("Another event", Priority::Normal);
    collision.event_id = human.request_id;
    assert!(app.post_event(collision, Source::Trigger).is_err());
}

#[tokio::test]
async fn events_are_cancelled_only_by_their_own_session_and_running_events_are_protected() {
    let (first, mut work) = app();
    let (second, _other_work) = app();
    let event = request("Owned event", Priority::Normal);
    first.post_event(event.clone(), Source::Trigger).unwrap();
    work.try_recv().unwrap();
    assert!(second.cancel_timer(event.event_id).is_err());
    assert!(first.cancel_timer(event.event_id).is_err());
    first.emit(AgentEvent::GenerationStarted {
        context: myco::TraceContext::root(),
    });
    assert_eq!(first.inbox_entries()[0].state, State::Running);
}

#[tokio::test]
async fn scheduled_event_ids_cannot_be_reused_by_native_service_turns() {
    let (app, mut work) = app();
    let mut event = request("Scheduled event", Priority::Normal);
    event.at = Some(app.clock.now() + chrono::Duration::minutes(1));
    app.post_event(event.clone(), Source::Trigger).unwrap();
    let submission = crate::service_protocol::Submit {
        instance: Uuid::new_v4(),
        request_id: event.event_id,
        text: "A different native request".into(),
        images: vec![],
    };
    assert!(matches!(
        app.accept_service(submission, "session".into()),
        Err(Error::Conflict(_))
    ));
    assert!(work.try_recv().is_err());
    assert_eq!(app.inbox_entries().len(), 1);
}

#[tokio::test]
async fn failed_event_acceptance_does_not_interrupt_existing_work() {
    let (app, mut work) = app();
    app.accept(action_request()).unwrap();
    let current = work.try_recv().unwrap();
    let path = std::env::temp_dir().join(format!("myco-failed-inbox-{}", Uuid::new_v4()));
    let inbox = Inbox::open(path.clone(), "session").unwrap();
    std::fs::create_dir(&path).unwrap();
    app.live.lock().unwrap().inbox = inbox;
    assert!(
        app.post_event(request("Not accepted", Priority::Urgent), Source::Trigger)
            .is_err()
    );
    assert!(!current.cancel.is_cancelled());
    assert!(app.inbox_entries().is_empty());
    std::fs::remove_dir(path).unwrap();
}
