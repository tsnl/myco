use serde_json::json;

use myco::thread::{ContentPart, Entry, Sender, Thread, ToolCallId, ToolResponseResult, Turn};

//
// Owned history
//

#[test]
fn appending_preserves_existing_entries_and_cloned_snapshots() {
    let mut thread = Thread::default();
    thread.push(user("first"));
    let snapshot = thread.clone();
    thread.push(Entry::Notification("second".into()));

    assert_eq!(snapshot.entries(), &[user("first")]);
    assert_eq!(&thread.entries()[..1], snapshot.entries());
    assert_eq!(thread.entries().len(), 2);
}

#[test]
fn cloned_values_can_grow_independently_without_shared_storage() {
    let mut original = Thread::default();
    original.push(user("shared"));
    let mut copy = original.clone();
    original.push(user("original only"));
    copy.push(user("copy only"));

    assert_eq!(original.entries()[1], user("original only"));
    assert_eq!(copy.entries()[1], user("copy only"));
}

#[test]
fn completed_tool_response_preserves_backgrounded_history() {
    let id = ToolCallId("5cb5a034-074d-4c5a-90b0-a2fdf8a9c100".parse().unwrap());
    let backgrounded = tool_response(id, ToolResponseResult::Backgrounded);
    let completed = tool_response(
        id,
        ToolResponseResult::Completed {
            result: "finished".into(),
            is_error: false,
        },
    );
    let mut thread = Thread::from_entries(vec![backgrounded.clone()]);
    let snapshot = thread.clone();
    thread.push(completed.clone());

    assert_eq!(snapshot.entries(), std::slice::from_ref(&backgrounded));
    assert_eq!(thread.entries(), &[backgrounded, completed]);
}

//
// Slices
//

#[test]
fn slice_copies_grow_independently() {
    let mut source = Thread::default();
    source.push(user("excluded"));
    source.push(user("shared"));
    source.push(user("source only"));
    let mut branch = Thread::from_entries(source[1..2].to_vec());
    branch.push(user("branch only"));
    source.push(Entry::Notification("source advanced".into()));

    assert_eq!(branch.entries(), &[user("shared"), user("branch only")]);
    assert_eq!(source[2], user("source only"));
    assert_eq!(&source[1..2], &branch[..1]);
}

#[test]
fn indexing_borrows_entries_and_ranges() {
    let mut source = Thread::default();
    assert!(source[..].is_empty());
    source.push(user("first"));
    source.push(user("second"));

    assert!(source[..0].is_empty());
    assert!(source[2..].is_empty());
    assert_eq!(&source[..], source.entries());
    assert_eq!(&source[..1], &source[..=0]);
    assert_eq!(&source[1..], &source[1..=1]);
    assert!(std::ptr::eq(source[1..].as_ptr(), &source.entries()[1]));
}

#[test]
#[should_panic]
fn out_of_bounds_indexing_panics() {
    let source = Thread::default();
    let _ = &source[1..];
}

//
// Conversation content
//

#[test]
fn clones_and_slice_copies_preserve_replay_content_and_tool_correlation() {
    let read_call = ToolCallId("5cb5a034-074d-4c5a-90b0-a2fdf8a9c100".parse().unwrap());
    let edit_call = ToolCallId("5cb5a034-074d-4c5a-90b0-a2fdf8a9c101".parse().unwrap());
    let mut content = reasoning_content();
    content.extend([
        ContentPart::Text("answer".into()),
        ContentPart::Refusal("refusal".into()),
        ContentPart::ToolCall {
            id: read_call,
            provider_call_id: Some("call_read_original".into()),
            name: "read".into(),
            arguments: Ok(json!({"path": "a.txt"})),
        },
        ContentPart::ToolCall {
            id: edit_call,
            provider_call_id: None,
            name: "edit".into(),
            arguments: Err("incomplete JSON".into()),
        },
    ]);
    let entries = vec![
        turn(
            Sender::User,
            vec![
                ContentPart::Text("prompt".into()),
                ContentPart::Image("diagram.png".into()),
            ],
        ),
        turn(
            Sender::System,
            vec![ContentPart::Text("instructions".into())],
        ),
        turn(Sender::Assistant, content),
        tool_response(
            read_call,
            ToolResponseResult::Completed {
                result: "contents".into(),
                is_error: false,
            },
        ),
        tool_response(
            edit_call,
            ToolResponseResult::Completed {
                result: "invalid arguments".into(),
                is_error: true,
            },
        ),
        Entry::Warning("warning".into()),
        Entry::Error("error".into()),
        Entry::Notification("notice".into()),
    ];
    let source = Thread::from_entries(entries.clone());
    let snapshot = source.clone();
    let branch = Thread::from_entries(source[..].to_vec());
    drop(source);
    assert_eq!(snapshot.entries(), entries);
    assert_eq!(branch.entries(), entries);
}

//
// Fixtures
//

fn user(text: &str) -> Entry {
    turn(Sender::User, vec![ContentPart::Text(text.into())])
}

fn turn(sender: Sender, content: Vec<ContentPart>) -> Entry {
    Entry::Turn(Turn { sender, content })
}

fn tool_response(id: ToolCallId, result: ToolResponseResult) -> Entry {
    turn(Sender::Tool, vec![ContentPart::ToolResponse { id, result }])
}

fn reasoning_content() -> Vec<ContentPart> {
    vec![
        ContentPart::Reasoning {
            text: "thinking".into(),
            signature: Some("original-signature".into()),
        },
        ContentPart::Reasoning {
            text: "unsigned observation".into(),
            signature: None,
        },
        ContentPart::EncryptedReasoning {
            id: "reasoning_original".into(),
            summary: vec!["first summary".into(), "second summary".into()],
            data: "opaque-encrypted-data".into(),
        },
        ContentPart::RedactedReasoning("opaque-redacted-data".into()),
    ]
}
