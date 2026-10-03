# myco::thread

An owned conversation in memory, with bulky content held in a separate blob
store. The kernel manages thread instances, persistence, and workspace access.

```rust
use myco::thread::{Author, Content, ContentPart, ContentPartKind, Thread, Turn, TurnKind, UserTurn};

let mut thread = Thread::default();
thread.push(Turn::new(TurnKind::User(UserTurn {
    content: Content {
        parts: vec![ContentPart {
            author: Author::Human,
            kind: ContentPartKind::Text { content: "Explain this repository.".into() },
        }],
    },
    tool_use_responses: vec![],
})));
let snapshot = thread.clone();
let mut branch = Thread::new(thread[..1].to_vec());
branch.push(Turn::new(TurnKind::User(UserTurn {
    content: Content {
        parts: vec![ContentPart {
            author: Author::System,
            kind: ContentPartKind::Text { content: "Working directory: /workspace".into() },
        }],
    },
    tool_use_responses: vec![],
})));
assert_eq!(thread, snapshot);
assert_eq!(branch.turns().len(), 2);
```

## Ownership and turns

`Thread` owns a dense `Vec<Turn>`. `push` appends through an exclusive borrow;
existing turns remain unchanged. `clone` copies history and its metadata.
Indexing borrows a turn or range; `Thread::new(thread[..n].to_vec())` copies a
prefix. Invalid indices panic; `turns().get(range)` provides checked access.
No turn pairs or strict user/assistant alternation are imposed. Consecutive human
follow-ups, runtime notices, and tool completions are ordinary turns. The workflow
selects a valid generation context from that history.

A `Turn` contains its `TurnKind`. `UserTurn` carries
content and tool responses; `AssistantTurn` carries content and tool requests.
Each `ContentPart { author, kind }` records its contributor as `Author::Human`,
`Assistant`, `Tool`, or `System`. A turn can mix authors, and copying or regrouping
parts preserves their attribution. `ContentPartKind` holds text, image references,
reasoning, or refusals. Content contains no tool requests or responses, so the
type graph is not recursive.

Authorship is independent of conversational role and model instruction priority.
`Author::System` identifies runtime-supplied content; the workflow decides where
to place it in an inference request. `Author::Tool` describes provenance, while
`ToolUseResponse::id` correlates the response with its call. GUI-only warnings and
activity, including structured lifecycle facts, remain kernel observations.

`Thread` owns no store and has no identity or serialization format. The kernel
can own boxed threads and borrow their stable allocation addresses while alive.
Persistent/public handles, publication checks, and derivation relationships remain
external. Cloning history never starts or duplicates tool execution.

## Blob store

Images hold `Image { blob: BlobRef }`. `BlobRef` is a SHA-256 digest, with no URL,
inline bytes, or base64 field in the history. [`blob::BlobStore`](../blob/README.md)
stores immutable `Blob { media_type: MediaType, data }` values. `insert(blob)`
computes a reference from media type and bytes, deduplicating identical content;
`get` reports a missing blob explicitly. A store can serve multiple threads.

```rust
use myco::blob::{Blob, BlobStore, MediaType};

let store = BlobStore::default();
let reference = store.insert(Blob {
    media_type: MediaType::Png,
    data: vec![0x89, b'P', b'N', b'G'].into(),
})?;
assert_eq!(store.get(reference)?.media_type, MediaType::Png);
# Ok::<(), myco::blob::BlobError>(())
```

The store owns reference-counted immutable bytes. Thread copies retain digests;
store clones share the registry and bytes, including later insertions.
`blob_refs()` enumerates references in ordinary content and tool responses,
including repeats. `validate_content(&store)` checks
that every reference resolves; it performs no file/network I/O or image decoding.

The kernel must retain blobs referenced by live or saved histories, persist blobs
before publishing references, and verify their digests when restoring content.
Exporting a history also requires its referenced blobs. References confer no
workspace authorization. Fetching files, media and size validation, persistence,
and the store's lifetime belong to the application.
For inference, workflows keep the selected `BlobRef`s in image inputs and supply
the same store to `GenAiClient`. The client resolves references during request
encoding; its provider encoders construct the base64 wire representation.

## Tools and reasoning

`ToolCallId` wraps `uuid::Uuid` and correlates a `ToolUseRequest` with its
`ToolUseResponse` observations. Every response owns full multimodal `Content`.
Its kind is `Success`, `Error`, `Backgrounded`, or `Unknown`. `Unknown` records
uncertain effects after an interruption; it neither claims failure nor authorizes
a retry. A later observation can use the same ID without replacing earlier ones.
The workflow projects these observations into one result per model-facing call;
after a background acknowledgement, later completion can be a runtime notice.

`ToolUseRequest::provider_id: Option<String>` preserves the original wire call ID.
The workflow copies it between the thread request and its model `ToolCall`, while
the UUID remains the logical identity used by tool observations. Synthetic calls
can leave it absent. Wire IDs are opaque labels; they may collide across generations
and do not identify a provider or account. Model encoding correlates calls and
results, preserving supplied wire IDs when possible and resolving collisions.

Reasoning retains explicit `Text { text, signature }`,
`Encrypted { provider_id, summary, data }`, and `Redacted` payloads. The encrypted
ID is an opaque provider string. Preserve these fields and their
order when rebuilding context; switching providers requires an explicit policy
for incompatible reasoning. Preserving a call ID does not make signatures portable.
Changing, splitting, or merging turns must preserve call/result associations and
the reasoning needed for replay.

Execution records, timestamps, usage, retry decisions, cancellation, and partial
streamed drafts belong to the workflow/kernel. Only accepted generation outcomes
are appended. `gen_ai` and `thread` share `blob` types without depending on each other.
