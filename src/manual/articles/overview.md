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
  **local** host worker (standard tools plus root-only services such as `session_meta` / `subagent`).
- **Remote host process (`myco --mode host`):** standard host tool services (`bash`, editor, `manual`,
  text search) over NDJSON via SSH.
- **Subagents** stay in the agent process and share this harness (same host pool).

## Config & paths

| Path | Role |
|------|------|
| `~/.myco/config.toml` | Remote hosts (`[[remote_hosts]]`). Local is always on. Override: `$MYCO_CONFIG` or `myco --config`. |
| `~/.myco/session/{shard}/{id}.json` | Conversation + metadata (title, links, scratchpad). Not shell/file state. |
| `~/.myco/session/{shard}/{id}.history` | Readline history for that session. |
| `.myco/subagent-logs/{agent_id}.log` | Durable subagent transcripts (cwd-relative). |

Minimal config shape:

```toml
enable_subagent = true

# Local is always enabled in-process — do not list it here.

[[remote_hosts]]
name = "devbox"
ssh = "devbox"                 # Host alias, hostname, or user@host
# myco = "myco"                # remote binary (default)
# user = "alice"               # optional
# port = 22                    # optional
# identity_file = "~/.ssh/id"  # optional
# ssh_options = ["ProxyJump=bastion"]
```

- Remotes use explicit SSH fields; myco always adds `BatchMode=yes`. Prefer Host aliases /
  ProxyJump / User in `~/.ssh/config` when possible.
- Remotes need `myco` on the **remote** PATH (or set `myco = "/abs/path/myco"`).
- Missing config file → local-only (safe default). There is no `default_host` setting; default is always `local`.

## API credentials & models

Loaded from the process environment; `dotenvy` also loads a `.env` in the cwd at startup.
Default CLI model is **`grok-4.5-build`**. Override with `myco --model <id>`.

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

Token resolution for OpenAI Responses: `OPENAI_API_KEY` → `XAI_API_KEY` →
`ANTHROPIC_AUTH_TOKEN` → `ANTHROPIC_API_KEY`. Base URL: `OPENAI_BASE_URL` →
`XAI_API_BASE_URL` → `https://api.x.ai/v1`. Requests go to `{base_url}/responses`.

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
  `drop_directory_index`, query with `indexed_exact_text_search` (Tantivy) /
  `indexed_semantic_text_search` (Candle **MiniLM**, weights baked in at compile
  time). On host start, auto-registers `.claude/skills`, `SKILL.md` directories, and
  `AGENTS.md`/`CLAUDE.md` under a bounded walk of cwd. Prefer `bash` + `rg` for large
  code trees; only register small repeated scopes.

## Product limits (V1)

- No heartbeat: remote liveness is next tool error; local is always in-process.
- No mid-flight cancel over the host pipe yet; Ctrl-C cancels the agent turn locally.
- You cannot invoke slash-commands; tell the user which to run.
- Conversation resume ≠ restored bash sessions or editor state.
- Bash sessions die when the host process exits (CLI exit, host crash, SSH drop). Local in-process
  sessions die with the agent process.
