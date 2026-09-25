# Inference API

`myco-model` provides backend-independent messages and streaming drivers for
Anthropic Messages, OpenAI Responses, and OpenAI Chat Completions. It has no
dependency on Myco's server, profile configuration, tool runtime, or session store.

Start with [`GenerativeModel`](../api/myco_model/trait.GenerativeModel.html),
[`GenerativeModelConfig`](../api/myco_model/struct.GenerativeModelConfig.html),
and [`Message`](../api/myco_model/enum.Message.html).

## Construct a driver

`myco_model::new` takes a `ModelSpec`, matching `BackendConfig`, system prompt,
and tool catalog. `ModelSpec::key` is your application's name for a model;
`api_id` is what the endpoint receives. The backend variant must match the
spec's protocol. Credentials and endpoint settings are supplied directly;
the library does not read `~/.myco` or load `.env` for you.

This complete example sends one prompt to a Chat Completions compatible
endpoint, streams text, and validates the completed response:

```rust,no_run
{{#include ../../../crates/myco-model/examples/inference.rs}}
```

Run it from the repository with a server and model you have configured:

```bash
MYCO_EXAMPLE_BASE_URL=http://localhost:11434/v1 \
MYCO_EXAMPLE_MODEL=YOUR_SERVED_MODEL_ID \
cargo run --locked -p myco-model --example inference
```

For endpoints requiring authentication, set `MYCO_EXAMPLE_API_KEY` in the
environment. This example makes a real model request. Its model window and
output budget are illustrative; adjust them for your endpoint.

## Consume the stream

Each `generate(&history)` call is **one attempt**. Its stream yields
`GenerationEvent::Part(MessagePart)` or terminates with a
`GenerationEvent::Failure(GenerationFailure)`. Dropping the stream cancels
the in-flight request.

`MessagePart` carries message start, content starts/deltas, tool starts/JSON
deltas, token usage, and a stop reason. Feed all parts to
[`MessageAccumulator`](../api/myco_model/struct.MessageAccumulator.html), then
call `finish()` to validate and obtain `GenerateOutput`. Text shown during
streaming is provisional until the whole attempt succeeds.

When incremental output is unnecessary,
`GenerateOutput::from_generation(model.generate(&history)).await` performs
that accumulation for you. It returns the error cause but discards retry
metadata; inspect `GenerationEvent::Failure` yourself if you need that metadata.

## Own policy above the driver

Drivers do not retry. A failure's `retryable` flag and optional `retry_after`
are inputs to caller policy. Retry only if **no response parts were emitted**,
with an attempt limit and bounded delay. `myco-agent` implements that policy;
passing retry settings to a backend alone does not create a retry loop.

`GenerateError::recovery()` distinguishes ordinary retry eligibility at the
history level from `Recovery::OmitLastMessage`, used when input must shrink.
It is not a promise that retrying will succeed or an instruction to retry all
errors indefinitely.

Myco's `SessionRunner` handles `RequestTooLargeError` before that fallback:
it compacts saved context, omits retained image payloads, and continues at the
failed generation boundary without resetting run accounting or replaying tools.
Another size recovery requires a successful model response; a repeated size
rejection uses the fallback. Other failures keep their existing retry policy.

## Preserve message structure

Messages contain user input, assistant output with optional tool calls, or
tool results. Results pair **positionally**: result `j` answers call `j` of
the immediately preceding assistant message. Keep the ordering and counts
when storing, slicing, or replaying history. Drivers generate protocol-specific
wire IDs; callers do not need to store provider call IDs.

A tool specification advertises a name, description, and JSON input schema.
The model crate never executes the tool. Use the [agent crate](agents.md),
or implement your own dispatch loop that preserves the message contract.

`Content` supports text, images, and thinking blocks. `answer_content` selects
text and image blocks for an answer. For images, provide a URL or a typed data
URL; raw base64 is treated as PNG. Token usage may be absent. Cached input
tokens are a subset of input tokens, not an extra quantity to add.
