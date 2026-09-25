# myco::thread

A thread is an owned, append-only conversation in memory. Its operations are
synchronous and work directly on its entries. The kernel manages distinct thread
instances, serialization, grouping, and persistence.

```rust
use myco::thread::{Entry, Thread};

let mut thread = Thread::default();
thread.push(Entry::User("Explain this repository.".into()));
let snapshot = thread.clone();

let mut branch = thread.fork(1).unwrap();
branch.push(Entry::User("Focus on the model module.".into()));
thread.push(Entry::Notification("Review started.".into()));

assert_eq!(snapshot.entries().len(), 1);
assert_eq!(thread.entries().len(), 2);
assert_eq!(branch.entries().len(), 2);
```

## Ownership

`Thread` owns its entry vector. Appending requires `&mut self` and leaves existing
entries unchanged. Cloning copies the entries, and each copy can grow
independently. Equality compares contents.

`default` creates an empty thread; `from_entries` takes an existing vector.
`fork` copies a prefix. Empty and complete prefixes are valid; an out-of-bounds
prefix returns `None`. Workflows and the kernel track how threads were derived.

The kernel can own a `Box<Thread>` and use references for in-process access. Its
allocation has a stable address while it remains allocated. Any address-based
identity is limited to that lifetime; identifiers used for HTTP or persistence
are managed separately by the kernel. A thread value carries no identity.

Collections of threads, snapshots, serialization formats, persistence, operation
receipts, cancellation, and publication checks are responsibilities of the kernel
and its callers. The thread module has no storage dependency, shared registry,
revision counter, or async runtime requirement.

## Entries and model evidence

Entries distinguish user input, assistant content, tool results, system
information, warnings, errors, and notifications. Assistant content is ordered.
`OperationId` is an alias for `uuid::Uuid`. The workflow assigns it once per logical
operation and reuses it for the tool call, its result, and retries. These
values describe history without deciding which entries enter a model prompt or
which tool calls may execute. Workflows compose thread operations with inference.

An assistant entry can reference an immutable inference record with `EvidenceId`.
The workflow retains the complete model message there, including signed/encrypted
reasoning and provider call IDs, and resolves it when rebuilding context. The
kernel persists that record before publishing a durable reference to it and
retains it while referenced. Missing evidence must not be silently replaced with
reconstructed reasoning. Synthetic entries can omit it.

The thread module only carries the reference and portable content. Evidence
management, model compatibility, and context projection belong to workflow/kernel
code. `model` and `thread` have no dependency on one another.
