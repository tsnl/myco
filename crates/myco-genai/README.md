# myco-genai

One inference attempt, independent of agent behavior, storage, and tool execution.
`Model` is the injectable interface; `HttpModel` implements OpenAI Responses and
Anthropic Messages over HTTP. Model names, credentials, endpoint URLs, and
generation limits are supplied by the caller.

```no_run
use futures::StreamExt;
use myco_genai::{Event, HttpModel, Message, Model, Protocol, Request};

# async fn example() -> Result<(), Box<dyn std::error::Error>> {
let model = HttpModel::new(
    Protocol::OpenAiResponses,
    "https://api.openai.com/v1/responses",
    &std::env::var("OPENAI_API_KEY")?,
)?;
let request = Request::new(
    std::env::var("OPENAI_MODEL")?,
    vec![Message::User("Explain this repository.".into())],
    1024,
);
let mut stream = model.generate(request);
while let Some(event) = stream.next().await {
    match event? {
        Event::Request { body, .. } => {
            // The application may record the exact input before dispatch.
            let _ = body;
        }
        Event::Progress { delta: Some(delta), .. } => print!("{}", delta.text),
        Event::Completed(response) => println!("\nFinish: {:?}", response.finish()),
        _ => {}
    }
}
# Ok(())
# }
```

For Anthropic, use `Protocol::AnthropicMessages` and the complete endpoint
`https://api.anthropic.com/v1/messages`. An empty key omits authentication for a
local compatible endpoint. The caller supplies a Tokio runtime when polling HTTP
streams. Additional provider settings, such as `reasoning` or `thinking`, go in
`Request::provider_options`; these cannot replace the managed context, tool, or
stream fields. No model catalog, environment loading, or policy defaults are
embedded in the crate.

## Contract

- Calling `generate` prepares a request without sending it. Its first successful
  event captures the request body, excluding authentication headers. Polling
  further starts the HTTP request. Validation failures produce one error.
- Each stream represents one attempt. There are no automatic retries, redirects,
  background tasks, or shared conversation state. The caller owns retry policy
  and overall/idle deadlines; the connection timeout is 30 seconds.
- `Progress` carries provider JSON and an optional text/reasoning/tool-argument
  projection. Valid JSON error events are delivered before the error ends the
  stream, so the application can retain the evidence. Streaming tool arguments
  are provisional.
- Only `Completed` supplies a final `Response`. `Finish::Length`, `Refusal`, and
  `Other` remain distinct from normal completion. EOF and `[DONE]` without a
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
- Dropping the stream drops its HTTP request. This releases local resources; it
  is not an acknowledgement that the provider stopped computing or billing.

Scripted models can implement `Model` using `Response::new`. The application
chooses how request events, raw progress, final responses, and failures enter its
own records.

## Scope and validation

The current interface covers text input/output, function tools with textual
results, and reasoning continuation. Image/audio input, Chat Completions,
provider-hosted tools, cross-protocol context conversion, and provider-specific
beta headers are outside this step. Unknown top-level events and opaque output
items are retained; unsupported Anthropic content deltas fail instead of
silently corrupting continuation data.

Tests use local HTTP fixtures and need no API credentials. They cover fragmented
SSE, Unicode, both providers, continuation, cumulative usage, truncation, errors,
concurrent requests, and cancellation. They establish protocol behavior; live
provider/account compatibility has not been exercised.

Protocol references:

- [OpenAI streaming](https://developers.openai.com/api/docs/guides/streaming-responses)
- [OpenAI reasoning continuation](https://developers.openai.com/api/docs/guides/reasoning)
- [Anthropic streaming](https://platform.claude.com/docs/en/build-with-claude/streaming)
- [Anthropic Messages](https://platform.claude.com/docs/en/api/messages/create)
