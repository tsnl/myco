# myco::thread

Versioned conversation history, independent of inference and workflow policy.
`Threads` delegates history transactions to an injected `ThreadStore`.
`MemoryStore` provides an in-memory implementation.

```rust
use std::sync::Arc;
use myco::thread::{
    AppendEntries, CreateThread, Entry, ForkThread, MemoryStore, OperationId,
    ThreadId, Threads,
};

# #[tokio::main]
# async fn main() -> Result<(), myco::thread::Error> {
let threads = Threads::new(Arc::new(MemoryStore::default()));
let first = threads.create(CreateThread {
    operation: OperationId(1),
    thread: ThreadId(10),
    entries: vec![Entry::User("Explain this repository.".into())],
    sources: vec![],
}).await?;

let next = threads.append(AppendEntries {
    operation: OperationId(2),
    expected: first,
    entries: vec![Entry::Notification("Review started.".into())],
}).await?;

let branch = threads.fork(ForkThread {
    operation: OperationId(3),
    thread: ThreadId(20),
    source: first,
    prefix_len: 1,
}).await?;

assert_eq!(threads.read(first).await?.entries().len(), 1);
assert_eq!(threads.read(next).await?.entries().len(), 2);
assert_eq!(threads.read(branch).await?.entries().len(), 1);
# Ok(())
# }
```

## History

Callers assign thread and operation IDs, unique within their store. Create and
fork require an unused thread ID and publish revision zero. Each nonempty append
checks the expected revision and publishes one new revision, regardless of its
entry count. Competing appends cannot silently attach to newer history.

`Thread` is an owned snapshot with dense vectors. Cloning copies its entries and
source references while preserving its identity and revision. Only store
mutations publish history; `Thread::from_parts` lets store implementations restore
a snapshot from their chosen serialization format without publishing it.
Cloning a `Threads` handle instead shares its store.

`HistoryRef` pins a revision and a half-open entry range. Create validates its
sources but does not insert their contents. Fork copies exactly the selected
prefix and records that fixed range as its immediate source. Later appends on
either thread leave the other unchanged. Empty histories and prefixes are valid;
unknown revisions and invalid ranges are rejected. Source references remain
unchanged across appends and do not recursively copy referenced histories.

## Entries and model evidence

Entries distinguish user input, assistant content, tool results, system
information, warnings, errors, and notifications. Assistant content is ordered;
tool calls and results share an operation ID assigned by the workflow. History
storage preserves entries without deciding which roles enter a prompt or which
tool calls may execute. There is no generation method on `Threads`.

An assistant entry can reference an immutable inference record with `EvidenceId`.
The workflow stores the complete model message, including signatures/encrypted
reasoning and provider call IDs, before publishing that reference. It retains the
record while referenced and resolves it when rebuilding model context. Missing
evidence must not be silently replaced with reconstructed reasoning. Synthetic
entries can omit it.

The thread module stores only the reference and portable content. Evidence
storage, resolution, model configuration compatibility, and context projection
belong to workflow/kernel code; they are not implemented here. `model` and
`thread` have no dependency on one another.

## Store contract

`ThreadStore` is dyn-compatible and returns ordinary async results. A store must
make history publication, operation receipts, and cancellation checks atomic:

- An operation ID binds one mutation value, including its expected revision.
  Retrying it returns the original outcome before checking current history.
  Reusing it for a different mutation returns `OperationConflict` and preserves
  the original record.
- Validation failure changes no history and records `Rejected(error)`. That
  outcome stays fixed even if the surrounding history later changes. A revised
  request needs a fresh operation ID.
- `cancel` records a tombstone if no outcome exists, preventing subsequent
  publication under that operation ID. If publication or rejection won the race,
  cancellation returns that existing outcome. It never deletes published history.
- `operation` reports the recorded terminal outcome. `None` means no receipt is
  visible; a request may still be in flight. `Error::Store` can leave an outcome
  uncertain. Query or retry the same mutation with the same ID to reconcile a lost
  response; do not resubmit it with a fresh ID.

These cancellations govern history publication only. They do not interrupt
inference or tool execution; the supervising workflow handles those operations.
Dropping a future is not an explicit cancellation request and may leave an
external store's outcome unknown.

`MemoryStore` keeps an append-only vector and revision lengths per thread, plus
the original mutation and receipt per operation. A single mutex serializes its
transactions without awaiting while locked. Reads and forks copy their selected
entries. All revisions and receipts are retained until the store is dropped.
It has no disk persistence or restart recovery; a durable store must retain the
same atomic publication and receipt semantics across restarts.
