# myco-llm-wire

**Internal to [myco](https://github.com/tsnl/myco).** Published only because
cargo requires a registry version for every dependency of a published crate.
There is no stability guarantee and no intent to serve callers outside myco —
the API changes whenever myco needs it to, without a major version. If you want
this functionality, use `myco`.

## What it is

myco's model layer: streaming wire drivers for three LLM HTTP protocols behind
one message model.

- Anthropic Messages
- OpenAI Responses
- OpenAI Chat Completions

A `GenerativeModel` turns a conversation into a stream of `MessagePart`s — text
and thinking deltas, tool calls as their arguments arrive, token usage, and the
`TurnEndReason` the provider reported.

It lives in its own crate because it depends on nothing else in myco, which
makes the boundary worth enforcing with the compiler rather than a convention.

## Why it isn't a thin HTTP wrapper

- **Prompt caching.** Anthropic `cache_control` breakpoints roll forward as the
  conversation grows, so a long session keeps paying cache-read prices instead
  of re-uploading the same prefix.
- **Thinking.** `ThinkingMode` covers the three shapes providers actually use —
  Anthropic adaptive (`output_config.effort`), Anthropic budgeted
  (`budget_tokens` derived from an `Effort`), and OpenAI `reasoning.effort` —
  because which one a model accepts is a property of the model, not the
  provider.
- **Turn end on the streaming path.** `TurnEndReason` is reported rather than
  used internally as an end-of-stream signal, so a caller can tell a finished
  turn from one `max_tokens` cut off mid-tool-call, and answer the dangling tool
  call instead of stranding the history.
- **Gateways that omit fields.** A missing `finish_reason` is tolerated rather
  than failing deserialization.

The protocol comes from the `BackendConfig` passed in, not a model-name lookup,
so any OpenAI-compatible or Anthropic-compatible gateway works by pointing
`base_url` at it. There is no built-in model list — models come from myco's
`[models]` / `[gateways]` config.

## License

MIT
