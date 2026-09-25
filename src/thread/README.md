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

## Conversation content

Entries distinguish user input, assistant content, tool results, system
information, warnings, errors, and notifications. Assistant content is ordered.

`ToolCallId` wraps `uuid::Uuid` and pairs a tool call's `id` with a result's
`call_id`. Clones and forks preserve that relationship. A model-originated call
also retains its original `provider_call_id` for context reconstruction; synthetic
calls can omit it. Workflows resolve a result's provider ID from the matching call
in the selected history. Execution IDs and retry policy belong to the kernel and
services.

Content owns the original reasoning text and signatures, encrypted reasoning IDs,
summaries and data, and redacted blocks. Preserve these values and their order when
rebuilding context. Clones and forks copy them directly; no external inference
record is needed to recover conversation content.

Workflows choose which entries enter a model prompt, check model compatibility,
and decide which tool calls may execute. Request traces, usage, timing, and
execution records stay outside the thread. `model` and `thread` have no dependency
on one another.
