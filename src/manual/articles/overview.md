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
| `~/.myco/config.toml` | Knobs only: `enable_subagent`, `attach_timeout_secs`. Override: `$MYCO_CONFIG` or `myco --config`. |
| `~/.myco/session/{shard}/{id}.json` | Conversation + metadata (title, links, scratchpad). Not shell/file state. Subagent runs use the same store with `kind: subagent` (hidden in default listings) and `id == agent_id`. |
| `~/.myco/session/{shard}/{id}.history` | Readline history for that session. |
| `~/.myco/memory/{uuid[..2]}/{uuid}.md` | Shared cross-agent, cross-session memory (immutable UUID-keyed entry files; see below). |
| `.myco/subagent-logs/{agent_id}.log` | Durable subagent transcripts (cwd-relative). |

Minimal config shape (`~/.myco/config.toml` — hosts are **not** listed here):

```toml
# model = "grok-4.5-build"    # default CLI model (--model overrides)
enable_subagent = true
# Per-remote connect timeout in seconds on first tool use (0 disables).
attach_timeout_secs = 10
```

- Remote hosts come from `~/.ssh/config`: each concrete `Host` alias attaches as
  `ssh -o BatchMode=yes <alias> myco --mode host`. `Include` directives are
  followed. Put user / port / identity / `ProxyJump` in `~/.ssh/config`;
  wildcard (`*`/`?`) and negated (`!`) patterns are ignored. The alias `local`
  is reserved (skipped).
- Remotes need `myco` on the **remote** PATH used by non-interactive SSH
  (`~/.local/bin` and `~/.cargo/bin` are common).
- Missing files → local-only (safe default). There is no `default_host` setting; default is always `local`.

## API credentials & models

Loaded from the process environment; `dotenvy` also loads a `.env` in the cwd at startup.
Default CLI model is **`grok-4.5-build`**; set `model = "<id>"` in config.toml to change
it, or pass `myco --model <id>` (flag wins).

**Anthropic Messages** (Claude models: `claude-haiku-4-5`, `claude-sonnet-4-6`,
`claude-opus-4-8`, `claude-fable-5`, …):

| Variable | Role |
|----------|------|
| `ANTHROPIC_AUTH_TOKEN` or `ANTHROPIC_API_KEY` | Bearer token (required) |
| `ANTHROPIC_BASE_URL` | API base (default `https://api.anthropic.com`) |

**OpenAI Responses** (xAI Grok `grok-4.5-build`, or any Responses-compatible gateway):

| Variable | Role |
|----------|------|
| `XAI_API_KEY` or `OPENAI_API_KEY` | Bearer token (required; see fallback) |
| `XAI_API_BASE_URL` or `OPENAI_BASE_URL` | Base URL (default `https://api.x.ai/v1`) |

Token resolution for OpenAI Responses: `XAI_API_KEY` → `OPENAI_API_KEY` →
`ANTHROPIC_AUTH_TOKEN` → `ANTHROPIC_API_KEY`. Base URL: `XAI_API_BASE_URL` →
`OPENAI_BASE_URL` → `https://api.x.ai/v1`. Requests go to `{base_url}/responses`.

All resolution happens in one startup step (`myco::config::Config`), which also
loads the harness config file (`--config` → `$MYCO_CONFIG` → `~/.myco/config.toml`)
and decides color output: sections are colored when stdout is a TTY, controlled by
`--color auto|always|never` plus `NO_COLOR` / `CLICOLOR_FORCE` / `TERM=dumb`.

Backend is chosen from the model id (Claude → Anthropic Messages; Grok → OpenAI
Responses). Empty credentials fail model creation at startup.

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
