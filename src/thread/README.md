# myco::thread

An owned conversation in memory, with bulky content held in a separate blob
store. The kernel manages thread instances, persistence, and workspace access.

```rust
use myco::thread::{Author, Content, ContentPart, Thread, Turn, TurnKind, UserTurn};

let mut thread = Thread::default();
thread.push(Turn::new(TurnKind::User(UserTurn {
    author: Author::Human,
    content: Content {
        parts: vec![ContentPart::Text { content: "Explain this repository.".into() }],
    },
    tool_use_responses: vec![],
})));
let snapshot = thread.clone();
let mut branch = Thread::new(thread[..1].to_vec());
branch.push(Turn::new(TurnKind::User(UserTurn {
    author: Author::System,
    content: Content::default(),
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

A `Turn` contains its `TurnKind` and a `provider_info` map. `UserTurn` carries
`Author::Human` or `Author::System`, content, and tool responses. `AssistantTurn`
carries content and tool requests. Content contains no tool requests or responses,
so the type graph is not recursive. `Author::System` records provenance; it does
not promote text to a model's system-instruction field. GUI-only warnings and
activity, including structured lifecycle facts, remain kernel observations.

`Thread` has no identity, serialization format, or storage dependency. The kernel
can own boxed threads and borrow their stable allocation addresses while alive.
Persistent/public handles, publication checks, and derivation relationships remain
external. Cloning history never starts or duplicates tool execution.

## Blob store

Images hold `Image { blob: BlobRef }`. `BlobRef` is a UUID newtype, with no URL,
inline bytes, or base64 field in the history. `BlobStore` registers immutable
`Blob { media_type, data }` values under these references. `insert` rejects an
already registered UUID; `get` reports a missing blob explicitly. UUID allocation
and persistence belong to the caller. A store can serve multiple threads.

```rust
use myco::thread::{Blob, BlobRef, BlobStore};

let reference = BlobRef(uuid::Uuid::from_u128(1));
let mut store = BlobStore::default();
store.insert(reference, Blob {
    media_type: "image/png".into(),
    data: vec![0x89, b'P', b'N', b'G'].into(),
})?;
assert_eq!(store.get(reference)?.media_type, "image/png");
# Ok::<(), myco::thread::ContentError>(())
```

The store owns reference-counted immutable bytes. Thread copies retain UUIDs;
store copies share the bytes. `blob_refs()` enumerates references in ordinary
content and tool responses, including repeats. `validate_content(&store)` checks
that every reference resolves; it performs no file/network I/O or image decoding.

The kernel must retain blobs referenced by live or saved histories, persist blobs
before publishing references, and restore the same UUID-to-content bindings.
Exporting a history also requires its referenced blobs. References confer no
workspace authorization. Fetching files, content hashing/deduplication, MIME and
size validation, retention, and garbage collection belong to the application.
Currently, the workflow resolves selected blobs into model input bytes.
The model encoder alone constructs the provider's base64 wire representation.
The planned `GenAiClient` integration exposes the shared store and resolves
references during request encoding; see [DESIGN.md](../../DESIGN.md#thread-history).

## Tools and provider metadata

`ToolCallId` wraps `uuid::Uuid` and correlates a `ToolUseRequest` with its
`ToolUseResponse` observations. Every response owns full multimodal `Content`.
Its kind is `Success`, `Error`, `Backgrounded`, or `Unknown`. `Unknown` records
uncertain effects after an interruption; it neither claims failure nor authorizes
a retry. A later observation can use the same ID without replacing earlier ones.
The workflow projects these observations into one result per model-facing call;
after a background acknowledgement, later completion can be a runtime notice.

`provider_info` is a map from backend namespace to opaque JSON. Model messages
expose the same map shape. The initial namespaces are `openai.responses` and
`anthropic.messages`; each model driver preserves its native call IDs there and
rebuilds wire IDs for other backends. The adapter carries the map between a thread
turn and its model message, preserving logical call IDs (generated model IDs are
UUID strings). A tool request has no dedicated provider-call-ID field.

Reasoning retains explicit `Text { text, signature }`,
`Encrypted { id, summary, data }`, and `Redacted` payloads. Preserve them and their
order when rebuilding context; switching providers requires an explicit policy
for incompatible reasoning. The call-ID map does not make signatures portable.
Changing, splitting, or merging turns requires corresponding metadata handling.
Neither map nor content contains executable callbacks.

Execution records, timestamps, usage, retry decisions, cancellation, and partial
streamed drafts belong to the workflow/kernel. Only accepted generation outcomes
are appended. `model` and `thread` remain independent modules.
