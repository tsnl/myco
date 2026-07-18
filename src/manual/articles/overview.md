# Myco overview

**myco** is a coding agent CLI: one conversation can drive tools on your laptop and on remote
machines over SSH. Supervisors can spawn subagents; tools run on **hosts** (local or remote).

## Architecture (one sentence)

**Agents orchestrate; hosts run tools on machines.** The **local** host is always enabled
**in-process** (no subprocess). Remotes use `ssh … myco --mode host` over NDJSON. The same
`myco` binary runs the agent (`--mode interactive`) and the remote host runtime (`--mode host`).
Subagents share the supervisor harness and host pool.

```
myco (interactive) / Agent
  └── Harness (routing, config, root-configured services)
        ├── HostController "local"   → in-process HostWorker (always on)
        └── HostController "…"       → ssh … myco --mode host (lazy remote)
              └── bash, str_replace_based_edit_tool, manual, text search (per host)
```

- **Agent process:** model, conversation history, cancel, event sink, and the in-process
  **local** host worker (standard tools plus root-only services such as `session_meta` /
  `subagent` / `memory`).
- **Remote host process (`myco --mode host`):** standard host tool services (`bash`, editor, `manual`,
  text search) over NDJSON via SSH.
- **Subagents** stay in the agent process and share this harness (same host pool).

## Config & paths

| Path | Role |
|------|------|
| `~/.ssh/config` | Remote hosts: every concrete `Host` alias (no `*`/`?`/`!` patterns; `Include`s followed) is a remote host of the same name. Local is always on. |
| `~/.myco/config.toml` | Model catalog (`[gateways]` / `[models]`, default `model`) + knobs (`enable_subagent`, `attach_timeout_secs`). Override: `$MYCO_CONFIG` or `myco --config`. |
| `~/.myco/session/{shard}/{id}.json` | Conversation + metadata (title, links, scratchpad). Not shell/file state. Subagent runs use the same store with `kind: subagent` (hidden in default listings) and `id == agent_id`. |
| `~/.myco/session/{shard}/{id}.history` | Readline history for that session. |
| `~/.myco/memory/{uuid[..2]}/{uuid}.md` | Shared cross-agent, cross-session memory (immutable UUID-keyed entry files; see below). |
| `.myco/subagent-logs/{agent_id}.log` | Durable subagent transcripts (cwd-relative). |

Minimal config shape (`~/.myco/config.toml` — hosts are **not** listed here;
top-level keys must come before the tables, per TOML):

```toml
model = "grok-4.5-build"      # default model key (--model overrides)
enable_subagent = true
# Per-remote connect timeout in seconds on first tool use (0 disables).
attach_timeout_secs = 10

[gateways.xai]
protocol = "openai-responses"
base_url = "https://api.x.ai/v1"
auth = { source = "env", var_name = "XAI_API_KEY" }

[models."grok-4.5-build"]
gateway = "xai"
context_window = 500_000
```

- Remote hosts come from `~/.ssh/config`: each concrete `Host` alias attaches as
  `ssh -o BatchMode=yes <alias> myco --mode host`. `Include` directives are
  followed. Put user / port / identity / `ProxyJump` in `~/.ssh/config`;
  wildcard (`*`/`?`) and negated (`!`) patterns are ignored. The alias `local`
  is reserved (skipped).
- Remotes need `myco` on the **remote** PATH used by non-interactive SSH
  (`~/.local/bin` and `~/.cargo/bin` are common).
- Missing files → local-only (safe default). There is no `default_host` setting; default is always `local`.

## Models & credentials (the catalog)

Myco ships **no built-in models**: the `[gateways]` / `[models]` tables in
config.toml are the entire catalog. A **gateway** is a place models are served
from (`protocol` + `base_url` + `auth`); a **model** entry is the key you pass
to `--model` (and what sessions record). Model-level fields override the
referenced gateway; a model may also inline all three and skip `gateway`.

```toml
[gateways.anthropic]
protocol = "anthropic-messages"        # or "openai-responses"
base_url = "https://api.anthropic.com"
auth = { source = "env", var_name = "ANTHROPIC_API_KEY" }

[gateways.openrouter]
protocol = "openai-responses"          # requests go to {base_url}/responses
base_url = "https://openrouter.ai/api/v1"
auth = { source = "env", var_name = "OPENROUTER_API_KEY" }

[models.claude-opus-4-8]
gateway = "anthropic"
context_window = 1_000_000             # required on every model

[models.claude-haiku-4-5]
gateway = "anthropic"
thinking = "budget"                    # older models reject adaptive thinking
context_window = 200_000

[models.kimi-k3]
gateway = "openrouter"
api_id = "moonshotai/kimi-k3"          # wire id; defaults to the key
context_window = 1_000_000

[models.local-qwen]                    # inline, no gateway ref; no auth
protocol = "openai-responses"
base_url = "http://localhost:11434/v1"
context_window = 32768
```

Per-model fields: `api_id` (wire id, defaults to the key), required
`context_window` (drives `USER n/m` + auto-compact), `thinking`
(`anthropic-messages`: `adaptive` (default) | `budget` | `none`;
`openai-responses`: `effort` (default) | `none`), `max_output_tokens`
(default 8192).

**Auth** is per gateway, overridable per model. The `auth` value is either
the credential itself (`auth = "sk-…"`) or a source table:
`{ source = "env", var_name = "…" }` reads the process environment (`dotenvy`
loads a `.env` from the cwd at startup); `{ source = "file", path = "…" }`
reads the file's trimmed contents (`~/` expands; keeps secrets out of a
shareable config); `{ source = "none" }` — or omitting `auth` — sends no auth
header (local servers). A credential that fails to look up does **not** fail
startup resolution — the error (naming the env var / file) surfaces when the
model is used.

Default model: `--model` → config.toml `model` → the sole `[models]` entry.
Anything else is a startup error listing the configured keys. Rerouting a
model through a different gateway is a config edit (e.g. point a
`claude-opus-4-8` entry at `gateway = "openrouter"` with
`api_id = "anthropic/claude-opus-4.8"`) — note the native Anthropic gateway
keeps prompt caching and adaptive thinking, which generic Responses gateways
do not.

All resolution happens in one startup step (`myco::config::Config`), which
also loads the config file (`--config` → `$MYCO_CONFIG` →
`~/.myco/config.toml`) and decides color output: sections are colored when
stdout is a TTY, controlled by `--color auto|always|never` plus `NO_COLOR` /
`CLICOLOR_FORCE` / `TERM=dumb`.

## Host routing

- Host tools (`bash`, `str_replace_based_edit_tool`, `manual`, text search, …) accept optional input field **`host`**.
- Omitted `host` → **`local`** (always in-process).
- Bash `session_id`s are **per host** (and per agent id). Do not assume a session on `local`
  exists on `devbox`.
- **Local** is always ready. **Remotes** are lazy: SSH workers spawn on first tool use.
- Connect failures surface as tool errors; `/hosts` shows ok (local/in-process or live remote),
  idle, or DOWN after a failed remote connect.
- **Text search** (per host): persistent watched roots via `index_directory` /
  `drop_directory_index`, query with `indexed_exact_text_search` (Tantivy over
  file bodies **and** path/filename tokens) /
  `indexed_semantic_text_search` (Candle **MiniLM**, weights baked in at compile
  time). On host start, auto-registers `.claude/skills`, `SKILL.md` directories, and
  `AGENTS.md`/`CLAUDE.md` under a bounded walk of cwd. Prefer `bash` + `rg` for large
  code trees; only register small repeated scopes.

## Cross-session memory

Root-only `memory` tool (agent process; shared by supervisor and subagents, across
sessions). The document is a set of **atomic entries** — immutable, UUIDed,
timestamped, titled — that are only ever created (`append` with title + body) or
deleted (`delete` by id). Each entry is a write-once file keyed by its uuid under
`~/.myco/memory/{uuid[..2]}/` (same fanout as the session store), carrying an
RFC-822-style header block (`Id` / `Date` / `Date-Local` / `Agent` / `Title`, then a
blank line and the markdown body) — nothing is rewritten in place and no locks are
taken, so concurrent sessions cannot conflict even on weakly consistent network
filesystems; readers order the document by the `Date` header. `list` gives a compact
id/date/title index, `read` returns full entries (document view, or one by id), and
`search` queries per-entry (mode `exact` = Tantivy, `semantic` = MiniLM) with
entry-shaped hits. **Every entry stays indexed and readable until explicitly deleted**
— there is no GC/pruning. Distinct from the per-session `session_meta` scratchpad.

## Product limits (V1)

- No heartbeat: remote liveness is next tool error; local is always in-process.
- No mid-flight cancel over the host pipe yet; Ctrl-C cancels the agent turn locally.
- You cannot invoke slash-commands; tell the user which to run.
- Conversation resume ≠ restored bash sessions or editor state.
- Bash sessions die when the host process exits (CLI exit, host crash, SSH drop). Local in-process
  sessions die with the agent process.
