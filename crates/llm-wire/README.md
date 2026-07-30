# llm-wire

Streaming wire drivers for three LLM HTTP protocols, behind one message model:

- **Anthropic Messages**
- **OpenAI Responses**
- **OpenAI Chat Completions**

A `GenerativeModel` turns a conversation into a stream of `MessagePart`s — text
and thinking deltas, tool calls as their arguments arrive, token usage, and the
`TurnEndReason` the provider reported.

```rust
let model = llm_wire::new(config)?;
let parts = model.generate(&[Message::UserMessage {
    content: vec![Content::Text { text: "hello".into() }],
}]);
```

The protocol comes from the `BackendConfig` you pass, not from a model-name
lookup, so any OpenAI-compatible or Anthropic-compatible gateway works by
pointing `base_url` at it. There is no built-in model list: a new model is a
config entry on your side, not a release of this crate.

## What it handles

- **Prompt caching.** Anthropic `cache_control` breakpoints roll forward as the
  conversation grows, so a long session keeps paying cache-read prices instead
  of re-uploading the same prefix.
- **Thinking.** `ThinkingMode` covers the three shapes providers actually use —
  Anthropic adaptive (`output_config.effort`), Anthropic budgeted
  (`budget_tokens` derived from an `Effort`), and OpenAI `reasoning.effort` —
  because which one a model accepts is a property of the model, not the
  provider.
- **Turn end on the streaming path.** `TurnEndReason` is reported, not just used
  internally as an end-of-stream signal, so a caller can tell a finished turn
  from one `max_tokens` cut off mid-tool-call — and answer the dangling tool
  call rather than stranding the history.
- **Gateways that omit fields.** A missing `finish_reason` is tolerated rather
  than failing deserialization.

## Testing against it

```toml
[dev-dependencies]
llm-wire = { version = "0.1", features = ["test-util"] }
```

`llm_wire::test_support` gives you conversation builders (`user`, `assistant`,
`assistant_tool`, `tool_results`) and `MessagePart` stream assertions
(`expect_text_delta`, `expect_tool_start`, `expect_turn_end`, …).

## Status

Extracted from [myco](https://github.com/tsnl/myco), where it has been the model
layer in daily use. The API is shaped by that one caller so far; expect it to
move before 1.0.

## License

MIT
