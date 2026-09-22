# myco::model

One inference attempt, independent of agent behavior, storage, and tool execution.
`GenAiClient` is a concrete type; `Config` selects OpenAI Responses or Anthropic
Messages over HTTP. Model names, credentials, endpoint URLs, and generation
limits are supplied by the caller. Backend drivers are private.

```no_run
use futures_util::StreamExt;
use myco::model::{Config, Event, GenAiClient, Message, Request};

# async fn example() -> Result<(), Box<dyn std::error::Error>> {
let client = GenAiClient::new(Config::OpenAi {
    endpoint: "https://api.openai.com/v1/responses".into(),
    api_key: std::env::var("OPENAI_API_KEY")?,
})?;
let request = Request::new(
    std::env::var("OPENAI_MODEL")?,
    vec![Message::User("Explain this repository.".into())],
    1024,
);
let mut generation = client.generate(request)?;
while let Some(event) = generation.next().await {
    match event? {
        Event::Progress { delta: Some(delta), .. } => print!("{}", delta.text),
        Event::Completed(response) => println!("\nFinish: {:?}", response.finish()),
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
and translates stream events into conversation entries. It records native provider
continuation and call-ID mappings, then restores that evidence when assembling
subsequent requests. The `thread` module supplies history operations; each workflow
chooses its context and publication policy. These higher modules are specified in
[DESIGN.md](../../DESIGN.md) and are subsequent implementation steps.

Operation, turn, and attempt IDs belong to the caller. `Completed(Response)`
is a validated inference outcome, not a persisted turn. Workflow code decides
whether to accept it and append it to history before reporting a committed turn.
Concurrent calls can produce independent candidates from the same fixed history.

## Stream contract

- `generate` validates and encodes the request synchronously, returning
  `Result<Generation<'_>, Error>`. Invalid input returns an error immediately,
  without a stream or network dispatch. The concrete `Generation` implements
  `Stream<Item = Result<Event, Error>>`, `Send`, `Unpin`, and `FusedStream`.
  It borrows its client and owns its attempt. Creating it performs no network I/O.
- The first stream item is `Event::Request` with the exact JSON body, excluding
  authentication headers. The HTTP request is sent only when polling continues.
  The caller can inspect and persist the request before polling again, or drop
  the stream if recording fails. Transport and provider failures arrive as stream
  errors.
- `Progress` carries provider JSON and an optional text/reasoning/tool-argument
  delta. Valid JSON is yielded before decoding or final normalization errors,
  including provider failure events. Tool arguments remain provisional.
- A successful attempt yields exactly one `Completed(Response)` and then ends.
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
- A response exposes ordered output, optional usage counters, and native provider
  data. Input-token totals include cache reads and writes. Additional usage
  fields and stop details remain in the provider body.

`ToolCall::arguments` is `Result<Value, String>`: parsed JSON or a parse error.
Truncated Responses calls can retain an error while the raw arguments remain in
provider data. Valid tool arguments must be JSON objects. Streaming argument
deltas remain text until the response is decoded.

Put a response in `Message::Assistant`, followed by linked `ToolResult` messages,
to continue. Opaque reasoning, thinking signatures, content ordering, and provider
call IDs are preserved. Continuation with another protocol is rejected.
Applications can reconstruct recorded responses through `Response::from_provider`;
public types have no prescribed storage encoding.

Workflow evaluations can inject scripted inference results without HTTP.
Adapter tests can construct outcomes using `Response::new`. The application
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
and reasoning continuation. Image/audio input, Chat Completions, provider-hosted
tools, cross-protocol conversion, and provider-specific beta headers are outside
this step. Unknown top-level events and opaque output items are retained;
unsupported Anthropic content deltas fail explicitly.

Tests use local HTTP fixtures and need no credentials. They cover fragmented SSE,
Unicode, both providers, continuation, cumulative usage, truncation, errors,
request-before-dispatch, ordered completion, stream termination, concurrent calls,
stream drop, and retaining a partial frame across a dropped `next()` wait.
Live provider/account compatibility has not been exercised.

Protocol references:

- [OpenAI streaming](https://developers.openai.com/api/docs/guides/streaming-responses)
- [OpenAI reasoning continuation](https://developers.openai.com/api/docs/guides/reasoning)
- [Anthropic streaming](https://platform.claude.com/docs/en/build-with-claude/streaming)
- [Anthropic Messages](https://platform.claude.com/docs/en/api/messages/create)
