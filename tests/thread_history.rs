use std::{
    future::Future,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use serde_json::json;
use tokio::sync::Barrier;

use myco::thread::{
    AppendEntries, ContentPart, CreateThread, Entry, Error, EvidenceId, ForkThread, HistoryRef,
    MemoryStore, Mutation, OperationId, OperationStatus, Revision, StoreFuture, Thread, ThreadId,
    ThreadStore, ThreadVersion, Threads,
};

fn threads() -> Threads {
    Threads::new(Arc::new(MemoryStore::default()))
}

fn create(operation: u128, thread: u128, text: &str) -> CreateThread {
    CreateThread {
        operation: OperationId(operation),
        thread: ThreadId(thread),
        entries: vec![Entry::User(text.into())],
        sources: vec![],
    }
}

fn append(operation: u128, expected: ThreadVersion, text: &str) -> AppendEntries {
    AppendEntries {
        operation: OperationId(operation),
        expected,
        entries: vec![Entry::User(text.into())],
    }
}

#[tokio::test]
async fn appends_preserve_published_revisions_and_owned_snapshots() {
    let threads = threads();
    let first = threads.create(create(1, 10, "first")).await.unwrap();
    let snapshot = threads.read(first).await.unwrap();
    let copy = snapshot.clone();
    let mut batch = append(2, first, "second");
    batch
        .entries
        .push(Entry::Notification("third entry".into()));
    let second = threads.append(batch).await.unwrap();
    assert_eq!(first.revision, Revision(0));
    assert_eq!(second.revision, Revision(1));
    assert_eq!(first.thread, second.thread);
    assert_eq!(threads.read(first).await.unwrap(), snapshot);
    assert_eq!(copy, snapshot);
    assert_ne!(copy.entries().as_ptr(), snapshot.entries().as_ptr());
    assert_eq!(threads.read(second).await.unwrap().entries().len(), 3);
}

#[tokio::test]
async fn retrying_a_committed_append_returns_its_original_revision() {
    let threads = threads();
    let first = threads.create(create(1, 10, "first")).await.unwrap();
    let request = append(2, first, "second");
    let second = threads.append(request.clone()).await.unwrap();
    let third = threads.append(append(3, second, "third")).await.unwrap();
    assert_eq!(threads.append(request).await.unwrap(), second);
    assert_eq!(threads.read(third).await.unwrap().entries().len(), 3);
    assert_eq!(
        threads.operation(OperationId(2)).await.unwrap(),
        Some(OperationStatus::Committed(second))
    );
}

#[tokio::test]
async fn forks_copy_a_fixed_prefix_and_record_their_source() {
    let threads = threads();
    let first = threads.create(create(1, 10, "shared")).await.unwrap();
    let second = threads
        .append(append(2, first, "source only"))
        .await
        .unwrap();
    let fork = threads
        .fork(ForkThread {
            operation: OperationId(3),
            thread: ThreadId(20),
            source: first,
            prefix_len: 1,
        })
        .await
        .unwrap();
    let fork_next = threads.append(append(4, fork, "fork only")).await.unwrap();
    assert_eq!(
        threads.read(second).await.unwrap().entries()[1],
        Entry::User("source only".into())
    );
    let branch = threads.read(fork_next).await.unwrap();
    assert_eq!(branch.entries()[1], Entry::User("fork only".into()));
    assert_eq!(
        branch.sources(),
        &[HistoryRef {
            version: first,
            entries: 0..1
        }]
    );
}

#[tokio::test]
async fn cancellation_before_publication_prevents_the_mutation() {
    let threads = threads();
    let first = threads.create(create(1, 10, "first")).await.unwrap();
    assert_eq!(
        threads.cancel(OperationId(2)).await.unwrap(),
        OperationStatus::Cancelled
    );
    assert_eq!(
        threads.append(append(2, first, "cancelled")).await,
        Err(Error::Cancelled(OperationId(2)))
    );
    assert_eq!(threads.read(first).await.unwrap().entries().len(), 1);
    let next = threads.append(append(3, first, "accepted")).await.unwrap();
    assert_eq!(next.revision, Revision(1));
    assert_eq!(
        threads.cancel(OperationId(3)).await.unwrap(),
        OperationStatus::Committed(next)
    );
}

#[tokio::test]
async fn creation_and_fork_retries_reuse_their_original_identities() {
    let threads = threads();
    let request = create(1, 10, "original");
    let first = threads.create(request.clone()).await.unwrap();
    assert_eq!(threads.create(request).await.unwrap(), first);
    let fork = ForkThread {
        operation: OperationId(2),
        thread: ThreadId(20),
        source: first,
        prefix_len: 1,
    };
    let branch = threads.fork(fork.clone()).await.unwrap();
    threads.append(append(3, branch, "later")).await.unwrap();
    assert_eq!(threads.fork(fork).await.unwrap(), branch);
    assert_eq!(
        threads.create(create(4, 10, "duplicate")).await,
        Err(Error::AlreadyExists(ThreadId(10)))
    );
}

#[tokio::test]
async fn an_operation_id_cannot_be_reused_for_a_different_request() {
    let threads = threads();
    let first = threads.create(create(1, 10, "original")).await.unwrap();
    let second = threads.append(append(2, first, "accepted")).await.unwrap();
    assert_eq!(
        threads.create(create(1, 20, "other id")).await,
        Err(Error::OperationConflict(OperationId(1)))
    );
    assert_eq!(
        threads.append(append(1, first, "other kind")).await,
        Err(Error::OperationConflict(OperationId(1)))
    );
    assert_eq!(
        threads.append(append(2, first, "other content")).await,
        Err(Error::OperationConflict(OperationId(2)))
    );
    assert_eq!(
        threads.append(append(2, second, "accepted")).await,
        Err(Error::OperationConflict(OperationId(2)))
    );
    assert_eq!(
        threads.operation(OperationId(2)).await.unwrap(),
        Some(OperationStatus::Committed(second))
    );
}

#[tokio::test]
async fn rejected_operations_keep_their_original_outcome_after_history_changes() {
    let threads = threads();
    let first = threads.create(create(1, 10, "first")).await.unwrap();
    let second = threads.append(append(2, first, "second")).await.unwrap();
    let request = append(3, first, "stale");
    let error = Error::Conflict {
        expected: first,
        actual: second,
    };
    assert_eq!(threads.append(request.clone()).await, Err(error.clone()));
    threads.append(append(4, second, "third")).await.unwrap();
    assert_eq!(threads.append(request).await, Err(error.clone()));
    assert_eq!(
        threads.cancel(OperationId(3)).await.unwrap(),
        OperationStatus::Rejected(error)
    );
    assert_eq!(
        threads.append(append(3, second, "stale")).await,
        Err(Error::OperationConflict(OperationId(3)))
    );
}

#[tokio::test]
async fn invalid_references_and_empty_appends_publish_nothing() {
    let threads = threads();
    let first = threads.create(create(1, 10, "source")).await.unwrap();
    let missing = ThreadVersion {
        thread: first.thread,
        revision: Revision(99),
    };
    assert_eq!(threads.read(missing).await, Err(Error::NotFound(missing)));
    for (index, source) in [
        HistoryRef {
            version: missing,
            entries: 0..1,
        },
        HistoryRef {
            version: first,
            entries: 0..2,
        },
        HistoryRef {
            version: first,
            entries: std::ops::Range { start: 1, end: 0 },
        },
    ]
    .into_iter()
    .enumerate()
    {
        let mut request = create(10 + index as u128, 20, "derived");
        request.sources.push(source);
        assert!(threads.create(request).await.is_err());
    }
    let invalid_fork = ForkThread {
        operation: OperationId(20),
        thread: ThreadId(20),
        source: first,
        prefix_len: 2,
    };
    assert!(matches!(
        threads.fork(invalid_fork).await,
        Err(Error::InvalidRequest(_))
    ));
    let empty = AppendEntries {
        operation: OperationId(21),
        expected: first,
        entries: vec![],
    };
    assert!(matches!(
        threads.append(empty).await,
        Err(Error::InvalidRequest(_))
    ));
    let created = threads.create(create(22, 20, "valid")).await.unwrap();
    let appended = threads.append(append(23, first, "valid")).await.unwrap();
    assert_eq!(created.revision, Revision(0));
    assert_eq!(appended.revision, Revision(1));
}

#[tokio::test]
async fn empty_histories_and_empty_fork_prefixes_are_valid() {
    let threads = threads();
    let mut request = create(1, 10, "");
    request.entries.clear();
    let empty = threads.create(request).await.unwrap();
    let fork = threads
        .fork(ForkThread {
            operation: OperationId(2),
            thread: ThreadId(20),
            source: empty,
            prefix_len: 0,
        })
        .await
        .unwrap();
    assert!(threads.read(fork).await.unwrap().entries().is_empty());
    assert_eq!(
        threads.read(fork).await.unwrap().sources(),
        &[HistoryRef {
            version: empty,
            entries: 0..0
        }]
    );
    let next = threads
        .append(append(3, fork, "first entry"))
        .await
        .unwrap();
    assert_eq!(
        threads.read(next).await.unwrap().entries(),
        &[Entry::User("first entry".into())]
    );
}

#[tokio::test]
async fn entries_and_evidence_references_survive_storage_and_forking() {
    let threads = threads();
    let mut request = create(1, 10, "prompt");
    request.entries.extend([
        Entry::System("instructions".into()),
        Entry::Assistant {
            content: vec![
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
            ],
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
    ]);
    let expected = request.entries.clone();
    let original = threads.create(request).await.unwrap();
    let fork = threads
        .fork(ForkThread {
            operation: OperationId(2),
            thread: ThreadId(20),
            source: original,
            prefix_len: expected.len(),
        })
        .await
        .unwrap();
    assert_eq!(threads.read(original).await.unwrap().entries(), expected);
    assert_eq!(threads.read(fork).await.unwrap().entries(), expected);
}

#[tokio::test]
async fn derived_histories_keep_fixed_sources_without_copying_source_entries() {
    let threads = threads();
    let original = threads.create(create(1, 10, "original")).await.unwrap();
    let sources = vec![HistoryRef {
        version: original,
        entries: 0..1,
    }];
    let mut request = create(2, 20, "summary");
    request.sources = sources.clone();
    let summary = threads.create(request).await.unwrap();
    threads
        .append(append(3, original, "new source material"))
        .await
        .unwrap();
    let summary = threads
        .append(append(4, summary, "follow-up"))
        .await
        .unwrap();
    let snapshot = threads.read(summary).await.unwrap();
    assert_eq!(snapshot.sources(), sources);
    assert_eq!(
        snapshot.entries(),
        &[
            Entry::User("summary".into()),
            Entry::User("follow-up".into())
        ]
    );
}

//
// Concurrent publication
//

async fn race<A, B>(left: A, right: B) -> (A::Output, B::Output)
where
    A: Future + Send + 'static,
    B: Future + Send + 'static,
    A::Output: Send + 'static,
    B::Output: Send + 'static,
{
    let left_gate = Arc::new(Barrier::new(2));
    let right_gate = left_gate.clone();
    let left = tokio::spawn(async move {
        left_gate.wait().await;
        left.await
    });
    let right = tokio::spawn(async move {
        right_gate.wait().await;
        right.await
    });
    let (left, right) = tokio::join!(left, right);
    (left.unwrap(), right.unwrap())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn competing_appends_publish_one_revision_and_reject_the_other_writer() {
    let threads = threads();
    let first = threads.create(create(1, 10, "first")).await.unwrap();
    let left = threads.clone();
    let right = threads.clone();
    let (left, right) = race(
        async move { left.append(append(2, first, "left")).await },
        async move { right.append(append(3, first, "right")).await },
    )
    .await;
    assert_eq!(usize::from(left.is_ok()) + usize::from(right.is_ok()), 1);
    let second = ThreadVersion {
        thread: first.thread,
        revision: Revision(1),
    };
    for (id, result) in [(OperationId(2), left), (OperationId(3), right)] {
        let status = match result {
            Ok(version) => {
                assert_eq!(version, second);
                OperationStatus::Committed(version)
            }
            Err(error) => {
                assert_eq!(
                    error,
                    Error::Conflict {
                        expected: first,
                        actual: second
                    }
                );
                OperationStatus::Rejected(error)
            }
        };
        assert_eq!(threads.operation(id).await.unwrap(), Some(status));
    }
    assert_eq!(threads.read(second).await.unwrap().entries().len(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_retries_of_one_operation_commit_once() {
    let threads = threads();
    let first = threads.create(create(1, 10, "first")).await.unwrap();
    let left = threads.clone();
    let right = threads.clone();
    let (left, right) = race(
        async move { left.append(append(2, first, "second")).await },
        async move { right.append(append(2, first, "second")).await },
    )
    .await;
    assert_eq!(left, right);
    let second = left.unwrap();
    assert_eq!(second.revision, Revision(1));
    let third = threads.append(append(3, second, "third")).await.unwrap();
    assert_eq!(third.revision, Revision(2));
    assert_eq!(threads.read(third).await.unwrap().entries().len(), 3);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancellation_and_publication_agree_on_one_terminal_outcome() {
    let threads = threads();
    let first = threads.create(create(1, 10, "first")).await.unwrap();
    let writer = threads.clone();
    let canceller = threads.clone();
    let (result, status) = race(
        async move { writer.append(append(2, first, "second")).await },
        async move { canceller.cancel(OperationId(2)).await },
    )
    .await;
    let second = ThreadVersion {
        thread: first.thread,
        revision: Revision(1),
    };
    let expected_status = match result {
        Ok(version) => {
            assert_eq!(version, second);
            OperationStatus::Committed(version)
        }
        Err(error) => {
            assert_eq!(error, Error::Cancelled(OperationId(2)));
            OperationStatus::Cancelled
        }
    };
    assert_eq!(status.unwrap(), expected_status);
    assert_eq!(
        threads.operation(OperationId(2)).await.unwrap(),
        Some(expected_status.clone())
    );
    assert_eq!(
        threads.read(second).await.is_ok(),
        matches!(expected_status, OperationStatus::Committed(_))
    );
}

//
// Lost acknowledgement through an injected store
//

#[derive(Default)]
struct LostAcknowledgement {
    store: MemoryStore,
    lost: AtomicBool,
}

impl ThreadStore for LostAcknowledgement {
    fn read(&self, version: ThreadVersion) -> StoreFuture<'_, Thread> {
        self.store.read(version)
    }

    fn apply(&self, mutation: Mutation) -> StoreFuture<'_, ThreadVersion> {
        Box::pin(async move {
            let operation = mutation.operation();
            let version = self.store.apply(mutation).await?;
            if operation == OperationId(2) && !self.lost.swap(true, Ordering::SeqCst) {
                return Err(Error::Store("acknowledgement lost".into()));
            }
            Ok(version)
        })
    }

    fn operation(&self, operation: OperationId) -> StoreFuture<'_, Option<OperationStatus>> {
        self.store.operation(operation)
    }

    fn cancel(&self, operation: OperationId) -> StoreFuture<'_, OperationStatus> {
        self.store.cancel(operation)
    }
}

#[tokio::test]
async fn a_lost_commit_acknowledgement_is_recovered_without_duplicating_entries() {
    let store = Arc::new(LostAcknowledgement::default());
    let threads = Threads::new(store.clone());
    let first = threads.create(create(1, 10, "first")).await.unwrap();
    let request = append(2, first, "second");
    assert_eq!(threads.operation(request.operation).await.unwrap(), None);
    assert!(matches!(
        threads.append(request.clone()).await,
        Err(Error::Store(_))
    ));
    drop(threads);
    let reconnected = Threads::new(store);
    let second = reconnected.append(request).await.unwrap();
    assert_eq!(
        reconnected.operation(OperationId(2)).await.unwrap(),
        Some(OperationStatus::Committed(second))
    );
    assert_eq!(reconnected.read(second).await.unwrap().entries().len(), 2);
}
