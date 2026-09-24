# myco::thread

A thread is an owned, append-only conversation in memory. Its operations are
synchronous and work directly on its entries. The kernel manages distinct thread
instances, serialization, grouping, and persistence.

```rust
use myco::thread::{Entry, Thread, ThreadId};

let mut thread = Thread::new(ThreadId(1));
thread.append(Entry::User("Explain this repository.".into()));
let snapshot = thread.clone();

let mut branch = thread.fork(ThreadId(2), 1).unwrap();
branch.append(Entry::User("Focus on the model module.".into()));
thread.append(Entry::Notification("Review started.".into()));

assert_eq!(snapshot.entries().len(), 1);
assert_eq!(thread.entries().len(), 2);
assert_eq!(branch.id(), ThreadId(2));
```

## Ownership

`Thread` owns its ID and entry vector. Appending requires `&mut self` and leaves
existing entries unchanged. Cloning copies the entries, preserving
identity. The copy can grow independently; it has no connection to the original.
The kernel decides which instance is authoritative for an ID and uses fresh IDs
for branches that it manages independently.

`fork` copies a prefix under a caller-supplied ID. Empty and complete prefixes are
valid; an out-of-bounds prefix returns `None`. `from_parts` constructs a value from
an ID and entries. Workflows and the kernel track how threads were derived.

Collections of threads, snapshots, serialization formats, persistence, operation
receipts, cancellation, and publication checks are responsibilities of the kernel
and its callers. The thread module has no storage dependency, shared registry,
revision counter, or async runtime requirement.

## Entries and model evidence

Entries distinguish user input, assistant content, tool results, system
information, warnings, errors, and notifications. Assistant content is ordered;
tool calls and results share an operation ID assigned by the workflow. These
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
