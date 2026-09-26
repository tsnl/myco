use std::sync::Arc;

use myco::thread::{
    AssistantTurn, Author, Blob, BlobRef, BlobStore, Content, ContentError, ContentPart,
    ReasoningContentPart, RefusalContentPart, Thread, ToolCallId, ToolUseRequest, ToolUseResponse,
    ToolUseResponseKind, Turn, TurnKind, UserTurn,
};
use serde_json::json;

//
// Owned history
//

#[test]
fn consecutive_human_and_system_turns_preserve_provenance_and_snapshots() {
    let mut thread = Thread::default();
    thread.push(user(Author::Human, "first"));
    let snapshot = thread.clone();
    thread.push(user(Author::System, "runtime notice"));
    thread.push(user(Author::Human, "follow-up"));

    assert_eq!(snapshot.turns(), &[user(Author::Human, "first")]);
    assert_eq!(&thread[..1], snapshot.turns());
    assert_eq!(thread[1], user(Author::System, "runtime notice"));
    assert_eq!(thread.turns().len(), 3);
}

#[test]
fn clones_and_slice_copies_grow_independently() {
    let mut source = Thread::new(vec![
        user(Author::Human, "excluded"),
        user(Author::Human, "shared"),
    ]);
    let snapshot = source.clone();
    let mut branch = Thread::new(source[1..2].to_vec());
    branch.push(user(Author::Human, "branch only"));
    source.push(user(Author::System, "source advanced"));

    assert_eq!(snapshot.turns().len(), 2);
    assert_eq!(
        branch.turns(),
        &[
            user(Author::Human, "shared"),
            user(Author::Human, "branch only")
        ]
    );
    assert_eq!(&source[1..2], &branch[..1]);
    assert_ne!(source, branch);
}

#[test]
fn indexing_borrows_turns_and_ranges() {
    let source = Thread::new(vec![
        user(Author::Human, "first"),
        user(Author::Human, "second"),
    ]);
    assert!(source[..0].is_empty());
    assert!(source[2..].is_empty());
    assert_eq!(&source[..], source.turns());
    assert_eq!(&source[..1], &source[..=0]);
    assert_eq!(&source[1..], &source[1..=1]);
    assert!(std::ptr::eq(source[1..].as_ptr(), &source.turns()[1]));
    assert!(source.turns().get(..3).is_none());
}

#[test]
#[should_panic]
fn out_of_bounds_indexing_panics() {
    let source = Thread::default();
    let _ = &source[1..];
}

//
// Tool observations
//

#[test]
fn tool_results_keep_multimodal_content_correlated_without_recursive_tool_payloads() {
    let id = call_id(1);
    let reference = blob_ref(1);
    let mut thread = Thread::new(vec![assistant_request(id)]);
    thread.push(response(
        id,
        ToolUseResponseKind::Success,
        Content {
            parts: vec![
                ContentPart::Text {
                    content: "screenshot".into(),
                },
                ContentPart::Image { blob: reference },
            ],
        },
    ));
    let TurnKind::User(turn) = &thread[1].kind else {
        panic!("expected tool response")
    };
    assert_eq!(turn.author, Author::System);
    assert_eq!(turn.tool_use_responses[0].id, id);
    assert_eq!(turn.tool_use_responses[0].content.parts.len(), 2);
    assert_eq!(thread.blob_refs().collect::<Vec<_>>(), vec![reference]);
}

#[test]
fn backgrounded_and_unknown_outcomes_remain_observations_after_completion() {
    let id = call_id(1);
    let initial = response(id, ToolUseResponseKind::Backgrounded, text("still running"));
    let uncertain = response(
        id,
        ToolUseResponseKind::Unknown,
        text("connection lost; effects unknown"),
    );
    let mut thread = Thread::new(vec![
        assistant_request(id),
        initial.clone(),
        uncertain.clone(),
    ]);
    let snapshot = thread.clone();
    thread.push(response(
        id,
        ToolUseResponseKind::Success,
        text("reconciled result"),
    ));

    assert_eq!(snapshot.turns(), &thread[..3]);
    assert_eq!(thread[1], initial);
    assert_eq!(thread[2], uncertain);
    assert_eq!(thread.turns().len(), 4);
}

#[test]
fn copies_preserve_provider_metadata_reasoning_refusals_and_tool_errors() {
    let mut turn = assistant_request(call_id(1));
    turn.provider_info
        .insert("example.backend".into(), json!({"opaque":"original"}));
    let TurnKind::Assistant(assistant) = &mut turn.kind else {
        unreachable!()
    };
    assistant.content = Content {
        parts: vec![
            ContentPart::Reasoning(ReasoningContentPart::Text {
                text: "thinking".into(),
                signature: Some("signed".into()),
            }),
            ContentPart::Reasoning(ReasoningContentPart::Encrypted {
                id: "reasoning".into(),
                summary: vec!["first".into(), "second".into()],
                data: "encrypted".into(),
            }),
            ContentPart::Reasoning(ReasoningContentPart::Redacted("redacted".into())),
            ContentPart::Refusal(RefusalContentPart {
                kind: Some("policy".into()),
                message: "cannot comply".into(),
            }),
        ],
    };
    assistant.tool_use_requests[0].arguments = Err("incomplete JSON".into());
    let expected = vec![
        turn,
        response(
            call_id(1),
            ToolUseResponseKind::Error,
            text("invalid arguments"),
        ),
    ];
    let source = Thread::new(expected.clone());
    let snapshot = source.clone();
    let branch = Thread::new(source[..].to_vec());
    drop(source);
    assert_eq!(snapshot.turns(), expected);
    assert_eq!(branch.turns(), expected);
}

//
// Blob store
//

#[test]
fn history_copies_retain_references_without_copying_blob_bytes() {
    let reference = blob_ref(1);
    let data: Arc<[u8]> = Arc::from(vec![7; 1024 * 1024]);
    let mut store = BlobStore::default();
    store
        .insert(
            reference,
            Blob {
                media_type: "image/png".into(),
                data: data.clone(),
            },
        )
        .unwrap();
    let thread = Thread::new(vec![response(
        call_id(1),
        ToolUseResponseKind::Success,
        Content {
            parts: vec![ContentPart::Image { blob: reference }],
        },
    )]);
    let branch = Thread::new(thread[..].to_vec());
    let snapshot = thread.clone();
    drop(thread);
    for history in [snapshot, branch] {
        history.validate_content(&store).unwrap();
        assert_eq!(history.blob_refs().collect::<Vec<_>>(), vec![reference]);
    }
    assert!(Arc::ptr_eq(&store.get(reference).unwrap().data, &data));
    assert!(Arc::ptr_eq(
        &store.clone().get(reference).unwrap().data,
        &data
    ));
}

#[test]
fn missing_blobs_in_turns_and_tool_responses_fail_explicitly() {
    let store = BlobStore::default();
    let reference = blob_ref(1);
    let content = Content {
        parts: vec![ContentPart::Image { blob: reference }],
    };
    for kind in [
        TurnKind::User(UserTurn {
            author: Author::Human,
            content: content.clone(),
            tool_use_responses: vec![],
        }),
        TurnKind::Assistant(AssistantTurn {
            content: content.clone(),
            ..Default::default()
        }),
        response(call_id(1), ToolUseResponseKind::Success, content).kind,
    ] {
        let thread = Thread::new(vec![Turn::new(kind)]);
        assert_eq!(
            thread.validate_content(&store),
            Err(ContentError::Missing(reference))
        );
    }
}

#[test]
fn a_blob_reference_cannot_be_rebound_to_different_content() {
    let reference = blob_ref(1);
    let mut store = BlobStore::default();
    let original = Blob {
        media_type: "image/png".into(),
        data: Arc::from([1, 2, 3]),
    };
    store.insert(reference, original.clone()).unwrap();
    let replacement = Blob {
        media_type: "image/jpeg".into(),
        data: Arc::from([4, 5, 6]),
    };
    assert_eq!(
        store.insert(reference, replacement),
        Err(ContentError::AlreadyExists(reference))
    );
    assert_eq!(store.get(reference), Ok(&original));
}

//
// Fixtures
//

fn user(author: Author, message: &str) -> Turn {
    Turn::new(TurnKind::User(UserTurn {
        author,
        content: text(message),
        tool_use_responses: vec![],
    }))
}

fn text(content: &str) -> Content {
    Content {
        parts: vec![ContentPart::Text {
            content: content.into(),
        }],
    }
}

fn call_id(value: u128) -> ToolCallId {
    ToolCallId(uuid::Uuid::from_u128(value))
}
fn blob_ref(value: u128) -> BlobRef {
    BlobRef(uuid::Uuid::from_u128(value))
}

fn assistant_request(id: ToolCallId) -> Turn {
    Turn::new(TurnKind::Assistant(AssistantTurn {
        content: text("I'll check"),
        tool_use_requests: vec![ToolUseRequest {
            id,
            name: "read".into(),
            arguments: Ok(json!({"path":"note.txt"})),
        }],
    }))
}

fn response(id: ToolCallId, kind: ToolUseResponseKind, content: Content) -> Turn {
    Turn::new(TurnKind::User(UserTurn {
        author: Author::System,
        content: Content::default(),
        tool_use_responses: vec![ToolUseResponse { id, kind, content }],
    }))
}
