use serde_json::json;

use myco::thread::{ContentPart, Entry, EvidenceId, OperationId, Thread, ThreadId};

//
// Owned history
//

#[test]
fn appending_preserves_existing_entries_and_cloned_snapshots() {
    let mut thread = Thread::new(ThreadId(1));
    thread.append(Entry::User("first".into()));
    let snapshot = thread.clone();
    thread.append(Entry::Notification("second".into()));

    assert_eq!(thread.id(), snapshot.id());
    assert_eq!(snapshot.entries(), &[Entry::User("first".into())]);
    assert_eq!(&thread.entries()[..1], snapshot.entries());
    assert_eq!(thread.entries().len(), 2);
}

#[test]
fn cloned_values_can_grow_independently_without_shared_storage() {
    let mut original = Thread::new(ThreadId(1));
    original.append(Entry::User("shared".into()));
    let mut copy = original.clone();
    original.append(Entry::User("original only".into()));
    copy.append(Entry::User("copy only".into()));

    assert_eq!(original.entries()[1], Entry::User("original only".into()));
    assert_eq!(copy.entries()[1], Entry::User("copy only".into()));
    assert_eq!(original.id(), copy.id());
}

#[test]
fn constructing_a_thread_preserves_entries_and_evidence_references() {
    let entries = vec![Entry::Assistant {
        content: vec![ContentPart::Text("summary".into())],
        evidence: Some(EvidenceId(200)),
    }];
    let mut thread = Thread::from_parts(ThreadId(1), entries.clone());
    let snapshot = thread.clone();
    thread.append(Entry::User("continue".into()));

    assert_eq!(snapshot.entries(), entries);
    assert_eq!(thread.id(), ThreadId(1));
}

//
// Forks
//

#[test]
fn forks_copy_only_the_selected_prefix_and_then_grow_independently() {
    let mut source = Thread::new(ThreadId(1));
    source.append(Entry::User("shared".into()));
    source.append(Entry::User("source only".into()));
    let mut branch = source.fork(ThreadId(2), 1).unwrap();
    branch.append(Entry::User("branch only".into()));
    source.append(Entry::Notification("source advanced".into()));

    assert_eq!(branch.id(), ThreadId(2));
    assert_eq!(
        branch.entries(),
        &[
            Entry::User("shared".into()),
            Entry::User("branch only".into())
        ]
    );
    assert_eq!(source.entries()[1], Entry::User("source only".into()));
    assert_eq!(&source.entries()[..1], &branch.entries()[..1]);
}

#[test]
fn empty_and_full_prefixes_fork_but_an_out_of_bounds_prefix_does_not() {
    let mut source = Thread::new(ThreadId(1));
    assert!(source.fork(ThreadId(2), 0).unwrap().entries().is_empty());
    source.append(Entry::User("first".into()));
    let snapshot = source.clone();

    assert!(source.fork(ThreadId(3), 0).unwrap().entries().is_empty());
    assert_eq!(
        source.fork(ThreadId(4), 1).unwrap().entries(),
        source.entries()
    );
    assert_eq!(source.fork(ThreadId(5), 2), None);
    assert_eq!(source.fork(ThreadId(6), usize::MAX), None);
    assert_eq!(source, snapshot);
}

//
// Conversation content
//

#[test]
fn forks_preserve_ordered_content_tool_correlation_and_evidence() {
    let content = vec![
        ContentPart::Reasoning("thinking".into()),
        ContentPart::Text("answer".into()),
        ContentPart::Refusal("refusal".into()),
        ContentPart::ToolCall {
            operation: OperationId(100),
            name: "read".into(),
            arguments: Ok(json!({"path": "a.txt"})),
        },
        ContentPart::ToolCall {
            operation: OperationId(101),
            name: "edit".into(),
            arguments: Err("incomplete JSON".into()),
        },
    ];
    let entries = vec![
        Entry::User("prompt".into()),
        Entry::System("instructions".into()),
        Entry::Assistant {
            content,
            evidence: Some(EvidenceId(200)),
        },
        Entry::ToolResult {
            operation: OperationId(100),
            output: "contents".into(),
            is_error: false,
        },
        Entry::Warning("warning".into()),
        Entry::Error("error".into()),
        Entry::Notification("notice".into()),
    ];
    let source = Thread::from_parts(ThreadId(1), entries.clone());
    let branch = source.fork(ThreadId(2), entries.len()).unwrap();
    assert_eq!(source.entries(), entries);
    assert_eq!(branch.entries(), entries);
}
