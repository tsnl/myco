# myco-genai

One inference attempt, independent of agent behavior, storage, and tool execution.
`Client` is a concrete type; `Config` selects OpenAI Responses or Anthropic
Messages over HTTP. Model names, credentials, endpoint URLs, and generation
limits are supplied by the caller. Backend drivers are private.

```no_run
use myco_genai::{Client, Config, Event, Message, Request};

# async fn example() -> Result<(), Box<dyn std::error::Error>> {
let client = Client::new(Config::OpenAi {
    endpoint: "https://api.openai.com/v1/responses".into(),
    api_key: std::env::var("OPENAI_API_KEY")?,
})?;
let request = Request::new(
    std::env::var("OPENAI_MODEL")?,
    vec![Message::User("Explain this repository.".into())],
    1024,
);
let response = client.generate(request, |event| async move {
    if let Event::Progress { delta: Some(delta), .. } = event {
        print!("{}", delta.text);
    }
    Ok(())
}).await?;
println!("\nFinish: {:?}", response.finish());
# Ok(())
# }
```

For Anthropic, use `Config::Anthropic` and the complete endpoint
`https://api.anthropic.com/v1/messages`. An empty key omits authentication for a
local compatible endpoint. The caller supplies a Tokio runtime. Share a client
by reference or through `Arc<Client>` for concurrent requests. Additional
provider settings, such as `reasoning` or `thinking`, go in
`Request::provider_options`; these cannot replace the managed context, tool, or
stream fields. No model catalog, environment loading, or policy defaults are
embedded in the crate.

An effect interpreter uses this client by translating the session language into
a `Request` and translating observations and the returned `Response` into session
events and candidates. These genai types exist at the interpretation boundary;
the agent crate does not depend on them. The interpreter retains native provider
continuation and call-ID mappings as evidence referenced by the session. It
restores that evidence when assembling later requests.

Operation, turn, and checkpoint correlations remain outside this crate. State
transitions decide whether to accept a candidate into a new immutable checkpoint;
generation itself performs no transcript writes. Concurrent calls can produce
independent candidates from the same checkpoint context. Checkpoint storage and
memory-sharing choices belong to the caller. Exposing generation through a
model's tool catalog is a separate application choice.

## Contract

- `generate` is an async function. Polling it validates the request and awaits a
  `Request` observation with the exact body, excluding authentication headers,
  before dispatch. Validation failures return an error without observations.
  `Client::request_body` inspects the payload without starting generation.
- Each call represents one attempt. There are no automatic retries, redirects,
  background tasks, or shared conversation state. The caller owns retry policy
  and overall/idle deadlines; the connection timeout is 30 seconds.
- `Progress` carries provider JSON and an optional text/reasoning/tool-argument
  projection. Valid JSON error events are delivered before the error ends the
  attempt, so the application can retain the evidence. Streaming tool arguments
  are provisional.
- Observations are awaited in order, including the provider's terminal event.
  Slow observers apply backpressure. An observer failure immediately returns
  `Error::Observer` with the application's boxed error; no further observations
  or successful response follow. The observer can persist each event before
  allowing generation to proceed.
- Only the return value supplies a final `Response`. `Finish::Length`, `Refusal`,
  and `Other` remain distinct from normal completion. EOF and `[DONE]` without a
  provider terminal event are errors. Malformed tool JSON fails explicitly; in
  an Anthropic stream this can happen before a later output-limit indication.
- A completed response exposes ordered output, optional usage counters, and its
  provider data. Input-token totals include cache reads and writes. Additional
  usage fields and stop details remain in the provider body.
- Put the response in `Message::Assistant`, followed by linked `ToolResult`
  messages, to continue. Opaque reasoning, thinking signatures, content ordering,
  and provider call IDs are preserved. Continuation with another protocol is
  rejected. Applications may reconstruct recorded provider responses through
  `Response::from_provider`; public types have no prescribed storage encoding.
- Dropping the generation future drops its HTTP request. This releases local
  resources; it is not an acknowledgement that the provider stopped computing
  or billing.

Controller evaluations can supply scripted session events without this crate.
Tests of the interpreter's response translation can use `Response::new`. The
application chooses how request events, raw progress, final responses, and
failures enter its own records.

## Implementation

`client` exposes the public API and holds a private `Box<dyn Driver>`. `request`
validates shared input contracts; `http` owns request dispatch and awaited
observations; `sse` decodes event framing. Each backend separates request
encoding, stream interpretation, and response normalization into small modules.

## Scope and validation

The current interface covers text input/output, function tools with textual
results, and reasoning continuation. Image/audio input, Chat Completions,
provider-hosted tools, cross-protocol context conversion, and provider-specific
beta headers are outside this step. Unknown top-level events and opaque output
items are retained; unsupported Anthropic content deltas fail instead of
silently corrupting continuation data.

Tests use local HTTP fixtures and need no API credentials. They cover fragmented
SSE, Unicode, both providers, continuation, cumulative usage, truncation, errors,
observer ordering and failures, concurrent requests, and cancellation. They
establish protocol behavior; live provider/account compatibility has not been
exercised.

Protocol references:

- [OpenAI streaming](https://developers.openai.com/api/docs/guides/streaming-responses)
- [OpenAI reasoning continuation](https://developers.openai.com/api/docs/guides/reasoning)
- [Anthropic streaming](https://platform.claude.com/docs/en/build-with-claude/streaming)
- [Anthropic Messages](https://platform.claude.com/docs/en/api/messages/create)
