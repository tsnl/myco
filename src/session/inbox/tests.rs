use super::*;
use crate::generative_model::Message;
use crate::test_support::temp_dir;

fn request(message: &str) -> EventRequest {
    EventRequest {
        event_id: Uuid::new_v4(),
        message: message.into(),
        priority: Priority::Normal,
        at: None,
        expires_at: None,
    }
}

#[test]
fn acceptance_is_durable_and_retries_do_not_duplicate_or_overwrite_events() {
    let dir = temp_dir("inbox-acceptance");
    let path = dir.path().join("inbox.json");
    let mut inbox = Inbox::open(path.clone(), "session").unwrap();
    let now = Utc::now();
    let original = request("Build finished");
    inbox
        .create(original.clone(), Source::Trigger, now)
        .unwrap();
    let mut restored = Inbox::open(path, "session").unwrap();
    restored
        .create(
            original.clone(),
            Source::Trigger,
            now + chrono::Duration::hours(1),
        )
        .unwrap();
    assert_eq!(restored.entries().len(), 1);
    let mut conflict = original.clone();
    conflict.priority = Priority::Urgent;
    assert!(restored.create(conflict, Source::Trigger, now).is_err());
    assert_eq!(restored.get(original.event_id).unwrap().request, original);
}

#[test]
fn failed_persistence_never_accepts_or_cancels_an_event() {
    let dir = temp_dir("inbox-save-failure");
    let path = dir.path().join("inbox.json");
    let mut inbox = Inbox::open(path.clone(), "session").unwrap();
    std::fs::create_dir(&path).unwrap();
    let event = request("Retry me");
    assert!(
        inbox
            .create(event.clone(), Source::Trigger, Utc::now())
            .is_err()
    );
    assert!(inbox.entries().is_empty());
    std::fs::remove_dir(&path).unwrap();
    inbox
        .create(event.clone(), Source::Trigger, Utc::now())
        .unwrap();
    std::fs::remove_file(&path).unwrap();
    std::fs::create_dir(&path).unwrap();
    assert!(inbox.cancel(event.event_id, Utc::now()).is_err());
    assert_eq!(inbox.get(event.event_id).unwrap().state, State::Pending);
}

#[test]
fn recovery_retries_only_claims_without_durable_transcript_markers() {
    let dir = temp_dir("inbox-recovery");
    let path = dir.path().join("inbox.json");
    let mut inbox = Inbox::open(path.clone(), "session").unwrap();
    let now = Utc::now();
    let committed = request("Committed");
    let uncommitted = request("Uncommitted");
    for event in [&committed, &uncommitted] {
        inbox.create(event.clone(), Source::Trigger, now).unwrap();
        inbox.claim(event.event_id, now).unwrap();
    }
    let mut session = Session::new("test");
    session
        .active_thread_mut()
        .messages
        .push(Message::UserMessage {
            content: vec![Content::System {
                kind: "inbox_event".into(),
                text: "Event arrived".into(),
                data: serde_json::json!({"id":committed.event_id}),
            }],
        });
    let mut inbox = Inbox::open(path, "session").unwrap();
    inbox.settle(&session, State::Interrupted, now).unwrap();
    assert_eq!(
        inbox.get(committed.event_id).unwrap().state,
        State::Interrupted
    );
    assert_eq!(
        inbox.get(uncommitted.event_id).unwrap().state,
        State::Pending
    );
    assert!(inbox.claim(committed.event_id, now).is_err());
    inbox.claim(uncommitted.event_id, now).unwrap();
}

#[test]
fn expiry_cancels_held_pending_input_but_never_running_work() {
    let dir = temp_dir("inbox-expiry");
    let mut inbox = Inbox::open(dir.path().join("inbox.json"), "session").unwrap();
    let now = Utc::now();
    let mut pending = request("Pending");
    pending.expires_at = Some(now + chrono::Duration::seconds(5));
    let mut running = pending.clone();
    running.event_id = Uuid::new_v4();
    for event in [&pending, &running] {
        inbox.create(event.clone(), Source::Trigger, now).unwrap();
    }
    inbox.edit(pending.event_id, None, true).unwrap();
    inbox.claim(running.event_id, now).unwrap();
    inbox.acknowledge(running.event_id).unwrap();
    inbox.expire(now + chrono::Duration::seconds(5)).unwrap();
    assert_eq!(inbox.get(pending.event_id).unwrap().state, State::Expired);
    assert_eq!(inbox.get(running.event_id).unwrap().state, State::Running);
    assert!(inbox.cancel(running.event_id, now).is_err());
}

#[test]
fn admission_and_retention_are_bounded_without_dropping_pending_work() {
    let dir = temp_dir("inbox-bounds");
    let mut inbox = Inbox::open(dir.path().join("inbox.json"), "session").unwrap();
    let now = Utc::now();
    for _ in 0..MAX_PENDING {
        inbox
            .create(request("Pending"), Source::Trigger, now)
            .unwrap();
    }
    assert!(
        inbox
            .create(request("Overflow"), Source::Trigger, now)
            .is_err()
    );
    let ids: Vec<_> = inbox.entries().iter().map(|e| e.request.event_id).collect();
    for id in ids {
        inbox.cancel(id, now).unwrap();
    }
    for _ in 0..MAX_RECENT + 1 {
        let event = request("Cancelled");
        inbox.create(event.clone(), Source::Trigger, now).unwrap();
        inbox.cancel(event.event_id, now).unwrap();
    }
    assert_eq!(inbox.entries().len(), MAX_RECENT);
}

#[test]
fn a_long_pending_event_keeps_its_recent_outcome_when_older_outcomes_are_pruned() {
    let dir = temp_dir("inbox-recent-outcomes");
    let mut inbox = Inbox::open(dir.path().join("inbox.json"), "session").unwrap();
    let now = Utc::now();
    let pending = request("A long-running wait");
    inbox.create(pending.clone(), Source::Trigger, now).unwrap();
    let mut oldest = None;
    for _ in 0..MAX_RECENT {
        let later = request("Later creation, earlier completion");
        oldest.get_or_insert(later.event_id);
        inbox.create(later.clone(), Source::Trigger, now).unwrap();
        inbox.cancel(later.event_id, now).unwrap();
    }
    inbox
        .cancel(pending.event_id, now + chrono::Duration::hours(1))
        .unwrap();
    assert!(
        inbox.get(pending.event_id).is_some(),
        "a new outcome must remain visible and idempotent"
    );
    assert!(inbox.get(oldest.unwrap()).is_none());
    assert_eq!(inbox.entries().len(), MAX_RECENT);
}

#[test]
fn invalid_or_foreign_documents_are_reported_without_replacement() {
    let dir = temp_dir("inbox-invalid");
    let path = dir.path().join("inbox.json");
    let mut inbox = Inbox::open(path.clone(), "owner").unwrap();
    inbox
        .create(request("Owned"), Source::Trigger, Utc::now())
        .unwrap();
    assert!(Inbox::open(path.clone(), "foreign").is_err());
    let bytes = std::fs::read(&path).unwrap();
    let mut value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    value["version"] = 99.into();
    std::fs::write(&path, value.to_string()).unwrap();
    assert!(Inbox::open(path.clone(), "owner").is_err());
    assert_eq!(std::fs::read_to_string(&path).unwrap(), value.to_string());
}
