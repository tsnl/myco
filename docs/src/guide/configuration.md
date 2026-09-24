# Models and configuration

A **gateway** describes an endpoint and its credentials. A **model** describes
one entry you can select with `--model`. You can give the same wire model
multiple catalog keys for different gateways or settings.

## Profiles and selection

`myco --profile work` selects `~/.myco/profiles/work/`. Profiles separate config,
sessions, workspace/prelude, and exported manuals. Selection precedence is
`--profile`, then `MYCO_PROFILE`, then `default`. `MYCO_HOME` changes the parent
directory; it does not itself name a profile directory.

The config path comes from `--config`, then `MYCO_CONFIG`, then
`<profile>/config.toml`. An explicitly selected file must exist. Select a model
with `--model KEY`; otherwise Myco uses the top-level `model` setting, or the
sole catalog entry. It reports an error when the choice is ambiguous.

## Choose a protocol

| `protocol` | Request path appended to `base_url` | Thinking modes |
| --- | --- | --- |
| `anthropic-messages` | `/v1/messages` | `adaptive`, `budget`, `none` |
| `openai-responses` | `/responses` | `effort`, `none` |
| `openai-completions` | `/chat/completions` | `effort`, `none` |

These names describe wire formats. Select the format your gateway serves.
The Anthropic base URL normally excludes `/v1`; the OpenAI dialect base URL
normally includes it. Set `thinking = "none"` if the served model does not
accept reasoning settings.

```toml
model = "coding"
attach_timeout_secs = 10

[gateways.provider]
protocol = "openai-responses"
base_url = "https://YOUR_GATEWAY/v1"
auth = { source = "env", var_name = "MY_MODEL_API_KEY" }
max_request_bytes = 30_000_000

[models.coding]
gateway = "provider"
api_id = "YOUR_SERVED_MODEL_ID"
context_window = 200000
max_output_tokens = 8192
thinking = "effort"
auto_compact_at = 0.8
```

The endpoint and model ID above are placeholders. Match `context_window` to
your served model; it drives context display and compaction policy.
Top-level settings must precede TOML tables.

## Credentials and overrides

Authentication can be a literal token, an environment source, a file source
such as `{ source = "file", path = "~/.secrets/model-token" }`, or
`{ source = "none" }`. Omitting auth also sends no authentication header.
Myco loads `.env` at startup. Credential lookup errors are reported when the
selected model is used; unknown config fields are rejected during startup.

Model fields override gateway fields. A model can inline `protocol`, `base_url`,
and `auth` and omit `gateway`. A model's `auth` or retry table replaces the
gateway's corresponding value rather than merging individual fields.

`max_request_bytes` sets a gateway's maximum serialized JSON request body
(default **30,000,000 bytes / 30 MB**; positive integers only). A model can
override it, including when configured without a gateway. The limit counts
the complete history, base64 images, system prompt, tool schemas, and JSON
overhead. Oversized requests are rejected locally before upload and rewind
the rejected turn. This is separate from the per-image limit below; images
are never silently resized or removed to fit a request.

## Control long runs

| Setting | Purpose |
| --- | --- |
| `max_output_tokens` | Output budget per model request; default 8192 |
| `max_truncated_resumes` | Consecutive continuations after output truncation; default 3, `0` disables |
| `auto_compact_at` | Automatic compaction fraction in `(0, 1]`; default `1.0` (full context window) |
| `max_image_base64_bytes` | Per-image uploaded base64 limit; default 5 MiB |
| `attach_timeout_secs` | Remote connection timeout; default 10, `0` disables |

Retry settings belong in `[gateways.NAME.retry]` or `[models.KEY.retry]`.
The default is three total attempts with bounded exponential backoff. Transient
failures also retry after partial output: the failed draft is discarded and a
fresh response is generated from unchanged context. Completed tools are not
replayed. Set `max_attempts = 1` to disable retries. The
[bundled overview](../manual/overview.md#models--credentials-the-catalog)
is the complete settings reference.
