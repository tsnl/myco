# myco::model

One inference attempt, independent of agent behavior, storage, and tool execution.
`GenAiClient` is a concrete type; `Config` selects OpenAI Responses or Anthropic
Messages over HTTP. Model names, credentials, endpoint URLs, and generation
limits are supplied by the caller. Backend drivers are private.

```no_run
use futures_util::StreamExt;
use myco::model::{Config, DeltaKind, Event, GenAiClient, Message, Request};

# async fn example() -> Result<(), Box<dyn std::error::Error>> {
let client = GenAiClient::new(Config::OpenAi {
    endpoint: "https://api.openai.com/v1/responses".into(),
    api_key: std::env::var("OPENAI_API_KEY")?,
})?;
let mut messages = vec![Message::User("Explain this repository.".into())];
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

For Anthropic, use `Config::Anthropic` and the complete endpoint
`https://api.anthropic.com/v1/messages`. An empty key omits authentication for a
local compatible endpoint. The caller supplies a Tokio runtime. Share a client
by reference or through `Arc<GenAiClient>` for concurrent requests. Additional
provider settings, such as `reasoning` or `thinking`, go in
`Request::driver_options`; these cannot replace managed context, tool, or
stream fields. No model catalog, environment loading, or policy defaults are
embedded in the model module.

Workflow code in `logic` translates selected thread history into a `Request`
and translates stream events into conversation entries. Every request supplies
the complete history it wants the model to see; the backend rebuilds the provider
request from that history. The `thread` module supplies history operations;
each workflow chooses its context and publication policy. These modules are specified in
[DESIGN.md](../../DESIGN.md) and are subsequent implementation steps.

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
- `Progress` carries raw provider JSON, yielded before its normalized `Delta`
  event or any decoding error. `Delta` carries text, reasoning, refusal, or
  tool-argument fragments. `index` identifies an output item or content block;
  `part` identifies its text/summary part, otherwise zero. Parts may arrive
  interleaved. Deltas are provisional and need not contain every final field.
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
Truncated Responses calls can retain an error while the raw arguments remain in
the raw progress events. Valid tool arguments must be JSON objects. Streaming argument
deltas remain text until the response is decoded.

Append the returned message directly to the next request, followed by linked
`ToolResult` messages when needed. `Message::Assistant` holds an ordered
`content: Vec<ContentPart>` for both generated replies and request history;
finish reason and usage belong to the completion event. Every request is rebuilt
from the supplied history. No whole native response is attached to it.

Reasoning metadata is explicit in the completed message: `ContentPart::Reasoning`
has an optional `signature`; `EncryptedReasoning` carries its ID, summaries, and
opaque data; `RedactedReasoning` carries a separate opaque block. Preserve these
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

- `openai_responses_backend.rs` and `anthropic_backend.rs` each implement request
  encoding, incremental interpretation, and response normalization for a provider.
- `backend_helpers.rs` contains the driver interface, shared validation, and
  stream lifecycle handling.
- `http_helpers.rs` handles HTTP transport and SSE framing.

`async-stream` expresses the yielding control flow without an extra task or channel.

## Scope and validation

The interface covers text input/output, function tools with textual results,
and signed/encrypted reasoning. Image/audio input, Chat Completions, provider-hosted
tools, conversion between reasoning formats, and provider-specific beta headers
are outside this step. Unknown events and output items remain in raw progress;
unsupported Anthropic content deltas fail explicitly.

Tests use local HTTP fixtures and need no credentials. They cover fragmented SSE,
Unicode, both providers, reuse of completed messages through fresh clients,
reasoning metadata, interleaved parts, cumulative usage, truncation, errors,
request-before-dispatch, ordered completion, stream termination, concurrent calls,
stream drop, and retaining a partial frame across a dropped `next()` wait.
Live provider/account compatibility has not been exercised.

Protocol references:

- [OpenAI streaming](https://developers.openai.com/api/docs/guides/streaming-responses)
- [OpenAI reasoning continuation](https://developers.openai.com/api/docs/guides/reasoning)
- [Anthropic streaming](https://platform.claude.com/docs/en/build-with-claude/streaming)
- [Anthropic Messages](https://platform.claude.com/docs/en/api/messages/create)
