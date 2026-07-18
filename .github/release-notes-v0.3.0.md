v0.3.0 rebuilds model selection around a user-configurable catalog: models and gateways now live in `config.toml` instead of the binary, so any Anthropic-Messages or OpenAI-Responses endpoint — Anthropic direct, OpenRouter, xAI, a local server — is a config entry away, with no code change. The release process itself was also streamlined.

## Highlights

### Configurable model catalog (#31) — action required
The hard-coded model list is gone; there are **no compiled-in models or gateways**. Both are defined in `config.toml`:

```toml
[gateways.openrouter]
protocol = "openai-responses"
base_url = "https://openrouter.ai/api/v1"
auth = { source = "env", var_name = "OPENROUTER_API_KEY" }

[models."deepseek/deepseek-v4-pro"]
gateway = "openrouter"
context_window = 1000000
```

- `[gateways.NAME]` sets `protocol` (`anthropic-messages` | `openai-responses`), `base_url`, and `auth`.
- `auth` is either the credential itself (a bare string) or a typed source: `{ source = "env", var_name = ... }`, `{ source = "file", path = ... }` (trimmed; supersedes the short-lived `tokens.toml`), or `{ source = "none" }`. Models inherit the gateway's auth unless they override it; absent everywhere, no auth header is sent (local servers).
- `[models.KEY]`: the key is what `--model` accepts and what sessions record. `gateway = "..."` pulls protocol/base_url/auth; model-level fields override. `api_id` defaults to the key; `context_window` is required; `thinking = adaptive|budget|effort|none` with per-protocol defaults; `max_output_tokens` optional.
- Default model resolution: `--model`, else the config's model key, else the sole catalog entry — otherwise an error listing the configured keys.
- Missing credentials defer to first use and name the env var or file that's missing.

**Upgrading from 0.2.x**: add at least one `[gateways.*]` and `[models.*]` entry to your `config.toml` before running — see the README and overview article for a starter catalog. Session files are unaffected: they already stored the model as a string, which is now the catalog key. The work in #31 began by wiring OpenRouter-served models (Kimi K3, DeepSeek V4, Gemini 3.x, GPT‑5.6, `anthropic/claude-*` slugs) into the old enum and ended by deleting the enum — all of those are now simply catalog entries pointed at an OpenRouter gateway.

### Streamlined releases (#30)
- The Publish workflow takes an optional `release_notes` dispatch input, so a release can ship hand-written notes above the install boilerplate (this release and v0.2.0 are the format).
- A guarded push-trigger fallback lets agent sessions — which can push but cannot call workflow dispatch — request a publish by committing `.github/publish-request.json` to a `claude/**` branch, with validated parameters and the notes sourced from a committed file.
- The underlying `semver-bump-and-cargo-publish` action (v1.0.4, pinned by SHA) now waits for CI checks on the publish branch's checked-out HEAD — the commit actually being released — rather than the workflow's trigger commit, and fails fast when a named check can never appear.

## Docs
- The CLI manual's banner description matches the lean one-line startup banner introduced in v0.2.0 (#32).
