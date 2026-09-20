# myco-model

Backend-independent message types and streaming model drivers for Anthropic
Messages, OpenAI Responses, and OpenAI Chat Completions.

Supply resolved model settings, credentials, and history. Each `generate` call
performs one attempt; callers own retry policy, tool execution, and persistence.
Use `MessageAccumulator` for incremental output or
`GenerateOutput::from_generation` for a completed response.

Read the [inference guide](https://tsnl.github.io/myco/developers/inference.html)
and [API reference](https://tsnl.github.io/myco/api/myco_model/).
The `inference` example takes an endpoint and model ID through environment
variables; see the guide before running it. `cargo test -p myco-model` exercises
the library with local fixtures.
