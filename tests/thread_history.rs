use serde_json::json;

use myco::thread::{ContentPart, Entry, Thread, ToolCallId};

//
// Owned history
//

#[test]
fn appending_preserves_existing_entries_and_cloned_snapshots() {
    let mut thread = Thread::default();
    thread.push(Entry::User("first".into()));
    let snapshot = thread.clone();
    thread.push(Entry::Notification("second".into()));

    assert_eq!(snapshot.entries(), &[Entry::User("first".into())]);
    assert_eq!(&thread.entries()[..1], snapshot.entries());
    assert_eq!(thread.entries().len(), 2);
}

#[test]
fn cloned_values_can_grow_independently_without_shared_storage() {
    let mut original = Thread::default();
    original.push(Entry::User("shared".into()));
    let mut copy = original.clone();
    original.push(Entry::User("original only".into()));
    copy.push(Entry::User("copy only".into()));

    assert_eq!(original.entries()[1], Entry::User("original only".into()));
    assert_eq!(copy.entries()[1], Entry::User("copy only".into()));
}

#[test]
fn constructing_a_thread_preserves_entries() {
    let entries = vec![Entry::Assistant {
        content: vec![ContentPart::Text("summary".into())],
    }];
    let mut thread = Thread::from_entries(entries.clone());
    let snapshot = thread.clone();
    thread.push(Entry::User("continue".into()));

    assert_eq!(snapshot.entries(), entries);
}

//
// Slices
//

#[test]
fn slice_copies_grow_independently() {
    let mut source = Thread::default();
    source.push(Entry::User("excluded".into()));
    source.push(Entry::User("shared".into()));
    source.push(Entry::User("source only".into()));
    let mut branch = Thread::from_entries(source[1..2].to_vec());
    branch.push(Entry::User("branch only".into()));
    source.push(Entry::Notification("source advanced".into()));

    assert_eq!(
        branch.entries(),
        &[
            Entry::User("shared".into()),
            Entry::User("branch only".into())
        ]
    );
    assert_eq!(source[2], Entry::User("source only".into()));
    assert_eq!(&source[1..2], &branch[..1]);
}

#[test]
fn indexing_borrows_entries_and_ranges() {
    let mut source = Thread::default();
    assert!(source[..].is_empty());
    source.push(Entry::User("first".into()));
    source.push(Entry::User("second".into()));

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
        Entry::User("prompt".into()),
        Entry::System("instructions".into()),
        Entry::Assistant { content },
        Entry::ToolResult {
            call_id: read_call,
            output: "contents".into(),
            is_error: false,
        },
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
