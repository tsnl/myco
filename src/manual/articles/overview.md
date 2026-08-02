# Myco overview

**myco** is a coding agent CLI: one conversation can drive tools on your laptop and on remote
machines over SSH. Tools run on **hosts** (local or remote); for nested agents, myco drives
itself as an ordinary command (see below).

## Architecture (one sentence)

**Agents orchestrate; hosts run tools on machines.** The **local** host is always enabled
**in-process** (no subprocess). Remotes use `ssh … myco --mode host` over NDJSON. The same
`myco` binary runs the agent (`--mode interactive`) and the remote host runtime (`--mode host`).

```
myco (interactive) / Agent
  └── Harness (routing, config, root-configured services)
        ├── HostController "local"   → in-process HostWorker (always on)
        └── HostController "…"       → ssh … myco --mode host (lazy remote)
              └── bash, str_replace_based_edit_tool, view_image (per host)
```

- **Agent process:** model, conversation history, cancel, event sink, and the in-process
  **local** host worker (standard tools plus root-only services such as `session_meta`).
- **Remote host process (`myco --mode host`):** standard host tool services (`bash`, editor,
  `view_image`) over NDJSON via SSH.
- **Nested agents:** there is no subagent tool — a supervisor starts `myco` itself in a bash
  session **on the local host**, passing `--parent-session <its own session id>` (myco stamps
  that id in a `# Session` block on the first user message of every session, so an agent needs
  no lookup; `session_meta` get still reports it), writes one
  prompt per line, and reads until the next `USER n/m` header (the turn boundary; colors/wrapping
  auto-off when piped). For a single self-contained task, print mode is the one-shot form:
  `myco -p "<task>" --parent-session <id>` runs one turn, streams the answer to stdout, and
  exits (`session=<id>` on stderr). Nesting locally shares config, keys, network, and the session store by
  construction; the child reaches remotes through its own host pool, its session is hidden
  (`kind: subagent`) and parented to the supervisor's. Adding `--fork` seeds the child with the
  supervisor's saved conversation (a context fork): launched with the same `--model` it rides the
  supervisor's prompt cache, and sessions are checkpointed mid-turn (after each user message and
  completed tool round) so forks start from the freshest replayable snapshot — never between a
  tool call and its result. A fork inherits the supervisor's stamped first message and stamps its
  own id on the first message it adds, so the newest `# Session` block is the running session's.
  Remotes stay config/key-free hands.

## Config & paths

| Path | Role |
|------|------|
| `~/.ssh/config` | Remote hosts: every concrete `Host` alias (no `*`/`?`/`!` patterns; `Include`s followed) is a remote host of the same name. Local is always on. |
| `~/.myco/config.toml` | Model catalog (`[gateways]` / `[models]`, default `model`) + knobs (`attach_timeout_secs`, `max_prelude_bytes`). Override: `$MYCO_CONFIG` or `myco --config`. |
| `~/.myco/session/{shard}/{id}.json` | Conversation + metadata (title, links, scratchpad), as **minified single-line JSON** — read it via the `session_history` tool or `jq`, not raw `cat`/`grep`. Not shell/file state. Worker runs (e.g. compact) use the same store with a non-user `kind` (hidden in default listings). |
| `~/.myco/session/{shard}/{id}.history` | Readline history for that session. |
| `~/.myco/manual/{version}/{commit}/` | These articles, copied to disk at startup for the running build (`index.md` plus one file per article). Read and search them like any other files; the agent system prompt names the directory. `myco --help <id>` prints the same text. |
| `~/.myco/workspace/` | Free-form agent workspace: notes, drafts, anything, in any layout. `workspace/prelude/` holds write-once prelude entries (edited via the root-only `prelude` tool); every entry is appended to every agent system prompt, followed by a bounded listing of the other workspace files (see below). |

Minimal config shape (`~/.myco/config.toml` — hosts are **not** listed here;
top-level keys must come before the tables, per TOML):

```toml
model = "grok-4.5-build"      # default model key (--model overrides)
# Per-remote connect timeout in seconds on first tool use (0 disables).
attach_timeout_secs = 10
# Cap on the rendered prelude appended to every agent system prompt (default 262144).
max_prelude_bytes = 262_144

[gateways.xai]
protocol = "openai-responses"
base_url = "https://api.x.ai/v1"
auth = { source = "env", var_name = "XAI_API_KEY" }

[models."grok-4.5-build"]
gateway = "xai"
context_window = 500_000
```

- The config is validated at startup, before any model call. Myco ships no
  models, so an **empty catalog is an error**: it names the config file and
  prints an entry to paste. Unknown top-level keys are rejected (a typo'd
  `model` would otherwise be silently ignored), as are unknown fields inside
  `[gateways.*]` / `[models.*]`, an empty `base_url`, an unknown `gateway`
  reference, and a `thinking` mode the protocol does not support. Every config
  error names the file it came from.
- A config path **you** name (`--config`, `$MYCO_CONFIG`) must exist — a typo
  there is an error, not an empty catalog. The defaulted `~/.myco/config.toml`
  may be absent (that is a first run).
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
protocol = "anthropic-messages"        # {base_url}/v1/messages
base_url = "https://api.anthropic.com"
auth = { source = "env", var_name = "ANTHROPIC_API_KEY" }

[gateways.openrouter]
protocol = "openai-responses"          # requests go to {base_url}/responses
base_url = "https://openrouter.ai/api/v1"
auth = { source = "env", var_name = "OPENROUTER_API_KEY" }

[gateways.ollama]
protocol = "openai-completions"        # requests go to {base_url}/chat/completions
base_url = "http://localhost:11434/v1"

[gateways.anthropic.retry]             # optional; per gateway
max_attempts = 5                       # total tries, including the first
initial_backoff_ms = 500
max_backoff_ms = 60_000
backoff_multiplier = 2.0

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
protocol = "openai-completions"
base_url = "http://localhost:11434/v1"
api_id = "qwen3:8b"
thinking = "none"                      # no reasoning: don't send an effort
context_window = 32768
```

Pick the protocol by what the endpoint serves: `openai-responses` for the
Responses API (`{base_url}/responses` — OpenAI, xAI, OpenRouter),
`openai-completions` for the older Chat Completions dialect
(`{base_url}/chat/completions`) that llama.cpp, Ollama, vLLM, LM Studio,
DeepSeek, Groq and friends speak. Chat Completions has no reasoning-summary
channel, so thinking there comes from the provider's `reasoning_content` /
`reasoning` deltas (nothing shown when a server sends neither), the output cap
goes out as `max_completion_tokens`, and a tool result's images follow in a
user message because a `tool` message may only carry text.

Per-model fields: `api_id` (wire id, defaults to the key), required
`context_window` (drives `USER n/m` + auto-compact), `thinking`
(`anthropic-messages`: `adaptive` (default) | `budget` | `none`;
`openai-responses` / `openai-completions`: `effort` (default) | `none` — use
`none` for models that reject a reasoning effort), `max_output_tokens`
(default 8192), `max_image_base64_bytes` (largest image the model accepts, as
the name says measured on the uploaded base64 payload — 4/3 of the file on
disk; default 5 MiB, matching Anthropic's per-image cap). The image cap is enforced locally by
`view_image` and by REPL `@path` attachments, so an oversized image fails with a
clear message naming both sizes instead of a provider 400. Remote hosts are
spawned with the selected model's value (`myco --mode host --max-image-base64-bytes`),
which keeps every host in a session on the same limit.

`max_truncated_resumes` (default 3, `0` to opt out) caps how many consecutive
`max_tokens` stops one turn resumes through before handing control back.
Truncation is not a dead end: a turn cut off mid-tool-call already ends on the
tool results, so it simply continues, and one cut off mid-sentence is continued
by a user turn asking for the rest (the assistant's own cut-off message cannot be
resent — that is the prefill shape current Anthropic models reject). Both count
against the cap, which exists because a model whose output cap is too low for how
much it writes would otherwise resume all night; any turn that ends for another
reason clears the count. Per model because the right ceiling depends on that
model's `max_output_tokens` versus how much it tends to write.
**Retry** is per gateway — what is being tuned is one endpoint's tolerance for
blips and its rate-limit behaviour — in a `[gateways.NAME.retry]` table:
`max_attempts` (default 3, counting the first; `1` disables), `initial_backoff_ms`
(500), `max_backoff_ms` (30 000), `backoff_multiplier` (2.0). Each unset field
keeps its default, so setting one knob does not reset the others. A model entry
may carry its own `[models.KEY.retry]` — the only way for a gateway-less model to
configure retry — and, like `auth`, it replaces the gateway's table rather than
merging with it. Only failures that happen *before* any of the response has
streamed are retried (connection errors, 408, 429, and 5xx including Anthropic's
529); a 400, a 401 or a 413 fails the same way however often it is sent, so it
surfaces immediately. A failure mid-stream is never retried either, because the
already-emitted parts would be replayed as duplicates. A provider's `Retry-After`
is honoured when it asks for longer than the computed backoff, still bounded by
`max_backoff_ms`.

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

- Host tools (`bash`, `str_replace_based_edit_tool`, `view_image`) accept optional input field **`host`**.
- Omitted `host` → **`local`** (always in-process).
- Bash `session_id`s are **per host** (and per agent id). Do not assume a session on `local`
  exists on `devbox`.
- **Local** is always ready. **Remotes** are lazy: SSH workers spawn on first tool use.
- Connect failures surface as tool errors; `/hosts` shows ok (local/in-process or live remote),
  idle, or DOWN after a failed remote connect.
- **`view_image`** (per host): returns a png/jpeg/gif/webp file as an image the
  model can actually look at — screenshots, diagrams, rendered output. Size is capped at
  the running model's `max_image_base64_bytes` (default 5 MiB, measured on the base64 payload);
  the tool's own description quotes the live limit, and going over fails that tool use.
  The format is read from the file's magic number, so the extension may be wrong or
  missing (user `@path` attachments share the same detection and cap). Text files stay
  with the editor.
- **Text search**: `bash` + `rg`/`grep` on the target host. myco ships no
  search tools of its own; project guidance (`AGENTS.md`/`CLAUDE.md`, skill
  packs) is read with the editor or `rg` like any other file.

## Agent workspace

`~/.myco/workspace/` is the agents' own directory — free-form files maintained with
the ordinary tools (no required format), persistent across sessions and shared by
every agent on the machine. `workspace/prelude/` is the one special place: it holds
the agent's prelude as maildir-style entries — one write-once `*.md` file each, never
edited in place. Every visible entry is rendered, in filename order under a
`[prelude entry <name>]` label, into the `# Prelude` section of every agent system
prompt, read at model build time (session start, model switch, worker spawn).

The root-only `prelude` tool (local in-process worker, like `session_meta`) is the
edit path: `add` a new entry, `replace` an entry (the replacement lands as a new
file before the old id is dropped), `remove` one, or `list` the live state. The
write-once discipline is what makes concurrent agents safe, even on a weakly
consistent network filesystem: adds cannot collide (fresh timestamped names), and
two agents replacing the same entry leave two candidate entries — a duplicate the
next curation pass merges — never a lost one. No locks, no in-place edits.
Distinct from the per-session `session_meta` scratchpad.

The prompt fragment makes the prelude the *default* home for durable information —
agents record findings eagerly and reserve workspace files for cold material
(rarely relevant, or high-volume lookup-only data). Prompt-resident text is
cached, so a big prelude is cheaper than the mid-task exploration it replaces. A
rendered prelude longer than `max_prelude_bytes` (config.toml; default 262144 = 256 KiB)
is cut to that many bytes with a marker in the prompt, and startup opens a WARNING
naming the entry count and both sizes — a prelude silently losing its tail is
otherwise invisible. Merge or remove entries, or raise the knob.

The rest of the workspace is listed, not quoted: a `# Workspace Files` section
gives each visible file's path (relative to `workspace/`), the UTC day it last
changed, and its title (first markdown heading, else first non-empty line). Hidden
names, symlinks, `prelude/` itself, and binary titles are skipped; the walk and the
rendered block are bounded (4 levels, 200 files, 8 KiB) with a marker when files
are left out. The prompt's appended blocks run least to most volatile — project
guidance, then the prelude, then this listing. Guidance leads because it changes
only when the repo's own file does, while agents are asked to record into the
prelude eagerly; ordering it that way keeps a recorded finding from invalidating
the cached guidance block for every agent that follows. The listing likewise uses
days rather than timestamps and path order rather than recency, so ordinary
workspace writes leave the shared prompt prefix intact for same-model forks.

## Product limits (V1)

- No heartbeat: remote liveness is next tool error; local is always in-process.
- No mid-flight cancel over the host pipe yet; Ctrl-C cancels the agent turn locally.
- You cannot invoke slash-commands; tell the user which to run.
- Conversation resume ≠ restored bash sessions or editor state.
- Bash sessions die when the host process exits (CLI exit, host crash, SSH drop). Local in-process
  sessions die with the agent process.
