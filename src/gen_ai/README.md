# myco::gen_ai

One inference attempt, independent of agent behavior, persistence, and tool execution.
`GenAiClient` is a concrete type; `Config` selects OpenAI Responses, OpenAI Chat
Completions, or Anthropic Messages over HTTP. Model names, credentials, endpoint URLs, and generation
limits are supplied by the caller. Backend drivers are private.

```no_run
use futures_util::StreamExt;
use myco::blob::BlobStore;
use myco::gen_ai::{Config, DeltaKind, Event, GenAiClient, InputContentPart, Message, MessageKind, Request};

# async fn example() -> Result<(), Box<dyn std::error::Error>> {
let client = GenAiClient::new(Config::OpenAiResponses {
    endpoint: "https://api.openai.com/v1/responses".into(),
    api_key: std::env::var("OPENAI_API_KEY")?,
}, BlobStore::default())?;
let mut messages = vec![Message::new(MessageKind::User {
    content: vec![InputContentPart::Text { content: "Explain this repository.".into() }],
})];
let request = Request {
    model: std::env::var("OPENAI_MODEL")?,
    messages: messages.clone(),
    max_output_tokens: 1024,
    ..Default::default()
};
let mut generation = client.generate(request)?;
while let Some(event) = generation.next().await {
    match event? {
        Event::Delta(delta) if delta.kind == DeltaKind::Text => print!("{}", delta.text),
        Event::Completed { message, finish, .. } => {
            messages.push(message);
            println!("\nFinish: {finish:?}");
        }
        _ => {}
    }
}
# Ok(())
# }
```

For Chat Completions, use `Config::OpenAiCompletions` and the complete endpoint
`https://api.openai.com/v1/chat/completions`. This driver sends instructions as a
system message, requests one choice and streaming usage, and maps the output limit
to `max_completion_tokens`. It does not infer capabilities from model names or
fall back to legacy `max_tokens`.

For Anthropic, use `Config::Anthropic` and the complete endpoint
`https://api.anthropic.com/v1/messages`. An empty key omits authentication for a
local compatible endpoint. The caller supplies a Tokio runtime. Share a client
by reference or through `Arc<GenAiClient>` for concurrent requests. Additional
provider settings, such as `reasoning` or `thinking`, go in
`Request::driver_options`; these cannot replace managed context, tool, or
stream fields. No model catalog, environment loading, or policy defaults are
embedded in the `gen_ai` module.

`GenAiClient::new(config, blobs)` takes a `BlobStore` handle. Pass clones of one
store to clients that should share blobs. `client.blobs()` exposes that handle,
including `insert(blob)`, which returns a content-addressed `BlobRef`. Later
insertions are visible to all clients using the same store.

The planned kernel adapter translates history selected by pure `logic` into a
`Request` and feeds generation outcomes back into that logic. Every request supplies
the complete history it wants the model to see; the backend rebuilds the provider
request from that history. The `thread` module supplies history operations;
each workflow chooses its context and publication policy. The kernel publishes
accepted turns. These adapters and workflow logic are subsequent implementation
steps in [DESIGN.md](../../DESIGN.md).

Operation, turn, and attempt IDs belong to the caller. `Completed` carries the
assistant message, finish reason, and usage. Workflow code decides whether to
accept the outcome and persist the message before reporting a committed turn.
Concurrent calls can produce independent candidates from the same fixed history.

## Stream contract

- `generate` validates and encodes the request synchronously, returning
  `Result<Generation<'_>, Error>`. Invalid input returns an error immediately,
  without a stream or network dispatch. The concrete `Generation` implements
  `Stream<Item = Result<Event, Error>>`, `Send`, `Unpin`, and `FusedStream`.
  It borrows its client and owns its attempt. Creating it performs no network I/O.
- The first stream item is `Event::Request { body }` with the exact JSON body,
  excluding authentication headers. The HTTP request is sent only when polling continues.
  The caller can inspect and persist the request before polling again, or drop
  the stream if recording fails. Transport and provider failures arrive as stream
  errors.
- `Progress` carries raw provider JSON (or the string `"[DONE]"` for that SSE marker),
  yielded before its normalized `Delta` events or any decoding error. `Delta` carries text, reasoning, refusal, or
  tool-argument fragments. `index` identifies an output item or content block;
  `part` identifies its text/summary part, otherwise zero. Parts may arrive
  interleaved. Deltas are provisional and need not contain every final field.
  Chat Completions uses index zero for message content, with part zero for text and
  part one for refusal; tool arguments use the provider's tool index plus one,
  with part zero. These coordinates are not indices into the completed message.
- A successful attempt yields exactly one `Completed { message, finish, usage }` and then ends.
  A failed attempt yields one `Err` and then ends. Repeated polling after either
  terminal outcome returns `None`. No further work requires polling after
  `Completed`; the HTTP request is released as that item is yielded.
- The raw provider terminal event is yielded as `Progress` before `Completed`.
  Only `Completed` supplies the authoritative response. `Finish::Length`,
  `Refusal`, and `Other` remain distinct from a normal reply. EOF and `[DONE]`
  without a provider terminal event are errors. Malformed arguments in a completed
  response fail validation; in Anthropic malformed tool JSON can fail before a
  later output-limit indication.
  Chat Completions requires both `finish_reason` and `[DONE]`; it collects any usage
  trailer between them before completing. Usage counters remain optional.
- Polling drives request execution and decoding. Pausing consumption applies
  backpressure; no producer task runs in the background. Dropping a pending
  `next()` future leaves the stream and attempt intact. Dropping the stream itself
  releases its HTTP request, without proving the provider stopped computing or
  billing. A GUI subscription should not own this stream.
- Each stream represents one attempt. There are no automatic retries, redirects,
  shared conversation state, or application event buffers. The caller owns retry
  policy and overall/idle deadlines; the connection timeout is 30 seconds.
  A retried attempt must remain distinguishable from earlier partial output.
- A response exposes ordered output and optional usage counters. Input-token
  totals include cache reads and writes. Raw progress retains provider metadata
  for diagnostics; workflows need not interpret it.

`ToolCall::arguments` is `Result<Value, String>`: parsed JSON or a parse error.
Truncated OpenAI calls can retain an error while the raw arguments remain in
the raw progress events. Valid tool arguments must be JSON objects. Streaming argument
deltas remain text until the response is decoded.

Append the returned message directly to the next request, followed by linked
`ToolResult` messages when needed. `MessageKind::Assistant` holds an ordered
`content: Vec<ContentPart>` for both generated replies and request history;
finish reason and usage belong to the completion event. `Message` wraps this
`MessageKind`. Every request is rebuilt from supplied content.

Generated `ToolCall::id` values are fresh UUID strings, independent of a provider's
wire IDs. Caller-supplied history may use any unique nonempty logical call ID.
`ToolCall::provider_id: Option<String>` preserves the original wire ID directly
on the call. Copy it into the corresponding thread tool request and back. It is
an opaque call label, not a provider or account identifier; synthetic calls can
leave it absent. A present ID must be nonempty ASCII letters, digits, underscores,
or hyphens. Invalid IDs are rejected before network dispatch.

Request encoding maps both calls and results through one table. All current
drivers reuse valid `provider_id` values and assign deterministic wire IDs when
absent or colliding. A provider reusing a native ID in
another generation cannot merge two logical calls. Appending later messages does
not change earlier wire IDs. This mapping does not translate reasoning formats.

User messages and tool results contain `Vec<InputContentPart>`: `Text { content }`
or `Image { blob: BlobRef }`. Both history and inference use the references defined
in [`blob`](../blob/README.md). Workflows select references without loading bytes.
During `generate`, encoders resolve images through the client's store, check their
`MediaType` values and nonempty bytes, and construct the provider's base64
representation.
Image inputs accept PNG, JPEG, GIF, and WebP; text and binary blobs are rejected.
Responses and Anthropic preserve tool-result images inside their correlated result.
Chat Completions supports images in user messages but rejects images in tool results,
whose provider schema permits only text.

A missing reference returns `Error::Blob(BlobError::Missing(reference))` before
a stream or network request is created. Invalid image media or empty bytes return
`Error::InvalidRequest`. Store locks are released before encoding the bytes;
the generation owns the encoded request. Dropping it does not remove stored blobs.
No URL or file loading occurs in `gen_ai`; media verification, image decoding,
input-size policy, and retention belong to the application.

Provider uploads are a separate implementation step. Each driver will resolve
local `BlobRef`s, upload through its provider's Files API where supported, and cache the resulting
file handles within its configured endpoint/account. Those handles stay private;
requests and thread history continue to use local references. The canonical bytes
remain in `blob`; local persistence is independent of provider storage. Deleted or
expired remote copies can be uploaded again from those bytes on a later attempt.

Uploads require asynchronous preparation before the final inference body exists.
That step must expose upload failures and support cancellation and request tracing.
The current stream's first-event contract describes inline requests; the upload
implementation must extend it to observe preparation before inference dispatch.
Details are tracked in [DESIGN.md](../../DESIGN.md#thread-history).

Reasoning metadata is explicit in the completed message: `ContentPart::Reasoning`
has an optional `signature`; `EncryptedReasoning` carries its opaque string
`provider_id`, summaries, and data; `RedactedReasoning` carries a separate opaque block. Preserve these
fields and their ordering when replaying reasoning. Only the provider can verify
the opaque strings; editing the associated reasoning can invalidate them.
Signed/encrypted reasoning from an incompatible backend is rejected. Unsigned
`Reasoning` is available for display but omitted from later requests. Other
provider metadata remains in raw progress for diagnostics.

Workflow evaluations can inject scripted inference results without HTTP.
Adapter tests can construct messages, deltas, and completion events directly. The application
chooses how request events, raw progress, responses, and errors enter its records.

## Implementation

`mod.rs` contains the entire public interface. `GenAiClient` holds a private
`Box<dyn Driver>`; `Generation` owns each attempt's stream.

- `openai_responses_backend.rs`, `openai_completions_backend.rs`, and
  `anthropic_backend.rs` each implement request
  encoding, incremental interpretation, and response normalization for a provider.
- `backend_helpers.rs` contains the driver interface, shared validation, and
  stream lifecycle handling.
- `http_helpers.rs` handles HTTP transport and SSE framing.

`async-stream` expresses the yielding control flow without an extra task or channel.

## Scope and validation

The interface covers text/image input, text output, multimodal function-tool
results where supported, and signed/encrypted reasoning. Audio, legacy text completions,
deprecated `function_call` messages, provider-hosted
tools, conversion between reasoning formats, and provider-specific beta headers
are outside this step. Unknown events and output items remain in raw progress;
unsupported Anthropic content deltas fail explicitly.

Tests use local HTTP fixtures and need no credentials. They cover fragmented SSE,
Unicode, all three drivers, reuse of completed messages through fresh clients,
reasoning metadata, interleaved parts, cumulative usage, truncation, errors,
request-before-dispatch, ordered completion, stream termination, concurrent calls,
stream drop, retaining a partial frame across a dropped `next()` wait, provider
native-ID preservation and collisions, and text/image user and tool inputs.
Blob tests cover shared client access, missing references, invalid image inputs,
and retention after dropping a generation and client.
Chat Completions tests also cover multiple tool deltas in one chunk, the usage trailer,
required finish markers, malformed choices, and unsupported history.
Live provider/account compatibility has not been exercised.

Protocol references:

- [OpenAI streaming](https://developers.openai.com/api/docs/guides/streaming-responses)
- [OpenAI Chat Completions](https://developers.openai.com/api/reference/resources/chat/subresources/completions/methods/create)
- [OpenAI reasoning continuation](https://developers.openai.com/api/docs/guides/reasoning)
- [Anthropic streaming](https://platform.claude.com/docs/en/build-with-claude/streaming)
- [Anthropic Messages](https://platform.claude.com/docs/en/api/messages/create)

- [OpenAI text/image function outputs](https://developers.openai.com/api/docs/guides/tools-computer-use-integration#use-your-own-ui-tools)
- [Anthropic tool results](https://platform.claude.com/docs/en/agents-and-tools/tool-use/handle-tool-calls)
