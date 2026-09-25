# myco::thread

A thread is an owned, append-only conversation in memory. Its operations are
synchronous and work directly on its entries. The kernel manages distinct thread
instances, serialization, grouping, and persistence.

```rust
use myco::thread::{ContentPart, Entry, Sender, Thread, Turn};

let mut thread = Thread::default();
thread.push(Entry::Turn(Turn {
    sender: Sender::User,
    content: vec![ContentPart::Text {
        content: "Explain this repository.".into(),
    }],
}));
let snapshot = thread.clone();

let mut branch = Thread::from_entries(thread[..1].to_vec());
branch.push(Entry::Turn(Turn {
    sender: Sender::User,
    content: vec![ContentPart::Text {
        content: "Focus on the model module.".into(),
    }],
}));
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
Indexing borrows entries or ranges: `&thread[0]`, `&thread[..n]`, or
`&thread[start..end]`. A slice's `to_vec()` copies its entries into an independent
vector. Invalid indices panic; use `thread.entries().get(range)` for checked
access. Workflows and the kernel track how threads were derived.

The kernel can own a `Box<Thread>` and use references for in-process access. Its
allocation has a stable address while it remains allocated. Any address-based
identity is limited to that lifetime; identifiers used for HTTP or persistence
are managed separately by the kernel. A thread value carries no identity.

Collections of threads, snapshots, serialization formats, persistence, operation
receipts, cancellation, and publication checks are responsibilities of the kernel
and its callers. The thread module has no storage dependency, shared registry,
revision counter, or async runtime requirement.

## Conversation content

An entry holds a `Turn`, warning, error, or notification. Each turn has a `Sender`
(assistant, user, tool, or system) and ordered content parts. All senders share the
same content representation, including `Text { content }` and `Image { url }`.
Image URLs are retained as supplied; image loading and provider encoding belong
to application code.

`ToolCallId` wraps `uuid::Uuid` and pairs the `id` fields of `ToolCall` and
`ToolResponse` content parts. Copies preserve that relationship. A model-originated
call also retains its original `provider_call_id` for context reconstruction;
synthetic calls can omit it. Workflows resolve a response's provider ID from the
matching call in the selected history. Execution IDs and retry policy belong to
the kernel and services.

`ToolResponseResult` distinguishes `Completed { result, is_error }` from
`Backgrounded`. A later completion can be appended with the same call ID while
retaining the backgrounding observation. Workflows decide how these observations
are represented in model context; the kernel manages the actual execution state.

Reasoning uses one content variant, `ContentPart::Reasoning(Reasoning)`. Its payload
is `Text { text, signature }`, `Encrypted { id, summary, data }`, or
`Redacted(String)`. Preserve these fields, summary boundaries, and content order
when rebuilding context. History copies retain them directly; no external
inference record is needed to recover conversation content.

Workflows choose which entries enter a model prompt, check sender/content and
model compatibility, and decide which tool calls may execute. The current model
API remains text-only; these history types do not add image inference support.
Request traces, usage, timing, and execution records stay outside the thread.
`model` and `thread` have no dependency on one another.
