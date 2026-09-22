# Myco overview

**myco** is a coding agent server: one conversation can drive tools on your laptop and on remote
machines over SSH. Tools run on **hosts** (local or remote); nested sessions use the authenticated server API (see below).

## Architecture (one sentence)

**Agents orchestrate; hosts run tools on machines.** The **local** host is always enabled
**in-process** (no subprocess). Remotes use `ssh … myco --mode host` over NDJSON. The same
`myco` binary runs the server (the default mode) and the remote host runtime (`--mode host`).

```
myco server / chat adapter
  ├── Agent (model context and run loop)
  └── SessionRuntime (session binding + tool ownership)
      └── Harness (routing, config, root-configured services)
          ├── HostController "local"   → in-process HostWorker (always on)
          └── HostController "…"       → ssh … myco --mode host (lazy remote)
                └── bash, str_replace_based_edit_tool, view_image (per host)
```

- **Server process:** model, conversation history, cancel, event sink, and the in-process
  **local** host worker (standard tools plus root-only services such as `session_meta`).
- **Remote host process (`myco --mode host`):** standard host tool services (`bash`, editor,
  `view_image`) over NDJSON via SSH.
- **Nested agents:** authenticated clients create a hidden child through
  `POST /api/sessions` with `parent_session` and optional `fork: true`. The
  child shares the server's profile and model catalog, with its own runner and
  tools. Forks inherit saved context; unresolved parent tool calls receive unknown
  outcomes, never replay. The child's first submission stamps its own identity,
  including when it was created before a server restart. Remotes stay tool workers.

## Sessions and threads

A **session** has a stable id, metadata, and an ordered set of **threads**. Each thread
is a linear message history. Only the latest thread accepts new messages; an agent
works on one thread at a time, and session turns and compaction share a writer gate.

`/compact` creates a successor thread in the same session. Its first message contains
the summary, followed by bounded recent context. The predecessor retains its original
messages and tool output. Title, links, and scratchpad remain attached to the same session.

Live bash shells and editor read stamps belong to the **session runtime**, shared across
threads and any replacement agent using that runtime. Compaction does not reset them.
A recorded tool result remains an observation from its original thread: a shell or file
may have changed since then. `/new` or switching to another session uses fresh tool
ownership. Resuming saved history after process exit does not restore tools.

Hidden `runtime` system parts record the runtime owner, observation time, model key,
API model/protocol and effort when known, and owned tool resources. Inventory covers
retained bash sessions (including exited processes with captured output) and editor read
fingerprints. Local state is observed directly; connected remote hosts are queried with
a bounded wait. Inventory never connects a lazy remote. Failed queries retain explicitly
last-known data rather than claiming the host is empty. This is an inventory of tool
handles, not every OS process or file created by a command.

A new runtime records which previously observed handles are unavailable here. External
side effects may survive: inspect them before retrying work, and re-read files before
editing. Model and effort changes produce a new notice. Compaction and rejected-input
recovery carry the latest runtime facts forward; earlier threads retain the original
observations. The session's top-level `model` is its initial catalog key; runtime records
identify the model used afterward. These parts reach the model but are omitted from
transcript replay, titles, and human acceptance timestamps.

State checkpoints fail closed: a save error stops further model/tool work. An interrupted
tool batch is recovered with explicit unknown outcomes and a hidden runtime notice, since
the calls may have taken effect before their results were saved. Inspect external state
before retrying those actions. Stored histories remain readable for inspection, but
malformed call/result pairs cannot be used as executable context.

Use `session_history` to read saved threads without loading all of them into context:

- `{"session_id":"…","action":"threads"}` lists threads, newest first.
- `{"session_id":"…","thread_id":"…","action":"stats"}` reports a thread and its predecessor.
- `{"session_id":"…","thread_id":"…","action":"expand","index":12}` reads an original message.

Omitting `thread_id` selects the active thread. Older threads are read-only.
Session files use schema version 5, including archive status, per-user-turn acceptance
times, and structured system content. System parts carry model-visible runtime context
without appearing in transcript replay. Formats 2 through 4 are accepted and upgraded
on read; loading alone does not rewrite their files. Older turns keep unknown timestamps.
Older binaries reject version 5. Existing predecessor/successor session links
remain metadata; separate saved sessions are not automatically combined.

The browser’s Archive and Restore controls change a session's browsing visibility while retaining
every thread and live tool. Archive status belongs to the named session only;
children and legacy compaction-linked sessions are independent. See `browser` for
archive filters and restoration.

## Config & paths

Select a profile with `myco --profile NAME`, or set `MYCO_PROFILE`; the default
name is `default`. Each profile has its own config, sessions, workspace/prelude,
and exported manual under `~/.myco/profiles/NAME/`. `MYCO_HOME` changes the parent
installation directory, so test runs can use `MYCO_HOME=/tmp/myco-test`.
Profile names contain letters, digits, hyphens, or underscores.

All sessions hosted by a server share its profile. Local tool processes inherit
`MYCO_PROFILE` and absolute `MYCO_HOME` across working-directory changes. Remote
hosts remain tool workers; config and credentials stay with the server.

For an existing installation, stop myco and move its `config.toml`, `session/`,
and `workspace/` into `~/.myco/profiles/default/` before restarting. Files are
never moved automatically; missing profile config is reported at its new path.
The manual is regenerated on startup. The paths below show the default profile.

| Path | Role |
|------|------|
| `~/.ssh/config` | Remote hosts: every concrete `Host` alias (no `*`/`?`/`!` patterns; `Include`s followed) is a remote host of the same name. Local is always on. |
| `~/.myco/profiles/default/config.toml` | Model catalog (`[gateways]` / `[models]`, default `model`) + knobs (`attach_timeout_secs`, `max_prelude_bytes`). Override: `$MYCO_CONFIG` or `myco --config`. |
| `~/.myco/profiles/default/session/{shard}/{id}.json` | Ordered threads + shared metadata (title, links, scratchpad), as **minified single-line JSON** — read it via the `session_history` tool or `jq`, not raw `cat`/`grep`. Not shell/file state. Worker runs (e.g. compact) use the same store with a non-user `kind` (hidden in default listings). |
| `~/.myco/profiles/default/images/{shard}/{sha256}` | Immutable raw image sidecars, shared by all threads and sessions in this profile. Back up this directory together with `session/`. |
| `~/.myco/profiles/default/session/{shard}/{id}.history` | Legacy readline history, preserved when present. |
| `~/.myco/profiles/default/session/archived/{shard}/` | Archived session JSON, thread summaries, and any legacy readline/console files. Restore moves these files back; writer locks stay at `session/{shard}/{id}.lock`. Startup moves archived sessions out of the active store when their writer lock is available. |
| `~/.myco/profiles/default/manual/{version}/{commit}/` | These articles, copied to disk at startup for the running build (`index.md` plus one file per article). Read and search them like any other files; the agent system prompt names the directory. `myco --help <id>` prints the same text. |
| `~/.myco/profiles/default/workspace/` | Free-form agent workspace: notes, drafts, anything, in any layout. `workspace/prelude/` holds write-once prelude entries (edited via the root-only `prelude` tool); every entry is appended to every agent system prompt, followed by a bounded listing of the other workspace files (see below). |

Minimal config shape (`~/.myco/profiles/default/config.toml` — hosts are **not** listed here;
top-level keys must come before the tables, per TOML):

```toml
model = "grok-4.5-build"      # default model key (--model overrides)
# Per-remote connect timeout in seconds on first tool use (0 disables).
attach_timeout_secs = 10
# Hard cap on the rendered prelude in every agent system prompt (default 262144):
# oversized edits are refused, and startup exits against a prelude over it.
max_prelude_bytes = 262_144
# Model requests per compaction, including retries (positive; no duration limit).
compaction_max_requests = 64

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
  there is an error, not an empty catalog. The defaulted `~/.myco/profiles/default/config.toml`
  may be absent (that is a first run).
- Remote hosts come from `~/.ssh/config`: each concrete `Host` alias attaches as
  `ssh -o BatchMode=yes <alias> myco --mode host`. `Include` directives are
  followed. Put user / port / identity / `ProxyJump` in `~/.ssh/config`;
  wildcard (`*`/`?`) and negated (`!`) patterns are ignored. The alias `local`
  is reserved (skipped).
- Remotes need `myco` on the **remote** PATH used by non-interactive SSH
  (`~/.local/bin` and `~/.cargo/bin` are common). Verify with
  `ssh -o BatchMode=yes <alias> 'command -v myco; myco --version'`;
  an interactive login can resolve a different binary than the host worker.
- Missing files → local-only (safe default). There is no `default_host` setting; default is always `local`.

Bash `exec` and `start` inherit the host process's working directory. Use
`cd /path && command` to run elsewhere; quote paths as shell arguments.
An `exec` directory change lasts only for that call. To keep shell state across
calls, `start` a shell (for example, `cd /path && bash --noprofile --norc`) and
send commands through `write`. The bash tool has no separate working-directory
argument; unsupported fields are rejected before execution.

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
auto_compact_at = 0.8                  # compact at 80% of the window

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
`view_image` and by browser `@path` attachments, so an oversized image fails with a
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
**Auto-compaction** runs through the server’s session runner. `auto_compact_at = 0.8` triggers when reported prompt size reaches 80% of
`context_window`, at a settled boundary between tool rounds or after a normal answer.
The system prompt tells the agent this
threshold. Unset (the default) disables automatic compaction; the fraction must
be greater than 0 and less than 1.

It runs the same compaction as `/compact`, creating a successor thread in the
same session with live tools intact. After success, a `# Resumption` message
asks the agent to continue the pending task from the summary and retained
context, or stop if the task is complete or needs user input. This message is
stored in the conversation without a human acceptance timestamp. It is a continuation, not startup: completed actions should not be
repeated. Opening a saved session with `--resume` or `/resume` still waits for
user input and does not restore live tools from a previous process.

Long tool loops can compact repeatedly when the context shrinks then grows again.
A completed answer triggers at most one compact-and-continue cycle per submission.
If the next usage report remains above the threshold, or summarization fails,
automatic compaction is disabled until manual compaction succeeds or another session
is opened. Failed generation, cancellation, refusal, and an exhausted truncation cap
do not start automatic continuation. Manual `/compact` waits for the next user input.
Compaction workers do not run auto-compaction. Each committed successor retains the
same live tool owner and the run's usage and truncation accounting.

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
`max_backoff_ms`. The agent starts a fresh generation attempt for each retry;
provider drivers perform one attempt and report failures. The browser
shows a notice describing the failure and whether it will retry. Cancel stops
the request, including retry waits. Notices are not added to model history.

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
`~/.myco/profiles/default/config.toml`).

## Host routing

- Host tools (`bash`, `str_replace_based_edit_tool`, `view_image`) accept optional input field **`host`**.
- Omitted `host` → **`local`** (always in-process).
- Bash `session_id`s are **per host** and owned by a session runtime. Do not assume a session on `local`
  exists on `devbox`.
- **Local** is always ready. **Remotes** are lazy: SSH workers spawn on first tool use.
- Connect failures surface in tool output. Check the remote with the non-interactive SSH
  commands in `harness-ops` before retrying.
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
- **Editor views**: whole-file reads, `view_range` slices, and directory listings
  reject output over 256 KiB. A single long line also counts toward the cap.
  Read a smaller range, or use bash with bounded output for long lines and large
  directories. A rejected view does not authorize subsequent edits.

## Nested agents (the recipe)

Use the authenticated server API on the **local host**. Read `browser.md` for
cookie authentication, request envelopes, and polling or streaming results.
An operator must supply the server launch credential to the client; never put
it in model messages or commit it to the repository.

1. Create a session with a fresh `request_id` and `parent_session` set to your
   session ID, available in the newest `# Session` block or `session_meta` get.
2. For shared context, add `fork: true`. It seeds the child with the parent's
   saved conversation. Use the same model key to preserve prompt-cache reuse;
   send `select_model` before the first submission if the server default differs.
3. Submit the bounded task through the child's action endpoint. Poll its snapshot
   or consume `/api/events`; `busy: false` marks the end of accepted work.
4. Cancel through the child's cancel endpoint when necessary. Closing a client
   connection does not stop the worker. Submit further turns to the same ID.
5. Read the child's output and verify the requested result. It remains hidden
   from ordinary listings; open its URL or use `session_meta` with
   `include_hidden: true` to inspect it later.

Give each child a bounded task, constraints, expected result, and whether it may
delegate further. Ask for completion evidence or a specific blocker. Keep bulk
output in files and return their paths with a concise summary. A completed turn
alone does not prove that the task is complete.

Forks copy checkpointed observations, including pending operations. Unfinished
tool calls get unknown outcomes before the child generates; they are never
replayed. Forks have their own tool ownership and cannot inherit live parent shells.
All child sessions share the server's profile and reach remotes through SSH;
remote workers need no model keys or session store.

## Agent workspace

`workspace/` under the selected profile root is the agents' own directory — free-form
files maintained with the ordinary tools (no required format), persistent across sessions and shared by
every agent using that profile. `workspace/prelude/` is the one special place: it holds
the agent's prelude as maildir-style entries — one write-once `*.md` file each, never
edited in place. Every visible entry is rendered, in filename order under a
`[prelude entry <name>]` label, into the `# Prelude` section of every agent system
prompt, read at model build time (session start, model switch, worker spawn).

Running agents scan the selected profile's prelude before each model step, including
between tool rounds in a long turn. When visible entry contents change, myco appends
a small `[myco: Prelude changes]` note listing added, modified, and removed filenames
to the latest user input or tool result and checkpoints it before the next request.
The agent can read changed files on the local host or use `prelude` action=list;
current entries supersede the prompt snapshot, and removed entries no longer apply.
This covers edits from other sessions as well as the agent's own prelude tool calls.
The system prompt stays fixed so its cached prefix remains reusable.

Scanning happens at model-step boundaries: it does not interrupt an in-flight request
or tool call, and an idle session picks up changes when it next runs. Hidden temporary
files, non-Markdown files, and empty entries are ignored. Failed scans keep the last
known snapshot and are retried at the next step. If rewind or compaction drops a
notice, or the agent moves to another thread after receiving updates, the next step
asks it to reload the full live prelude.

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
cached, so a big prelude is cheaper than the mid-task exploration it replaces.

`max_prelude_bytes` (config.toml; default 262144 = 256 KiB) bounds the rendered
prelude, and it is enforced at both ends rather than applied to the prompt: the
`prelude` tool refuses an `add`/`replace` that would cross it, and startup
**exits** against a directory already over it, naming the sizes and the two
fixes (prune entries by hand, or raise the knob). A prompt therefore always
carries the prelude whole — a shortened one is indistinguishable, from inside
the prompt, from knowledge that was never recorded, which is exactly the
failure the cap exists to prevent.

The rest of the workspace is listed, not quoted: a `# Workspace Files` section
gives each visible file's path (relative to `workspace/`), the UTC day it last
changed, and its title (first markdown heading, else first non-empty line). Hidden
names, symlinks, `prelude/` itself, and binary titles are skipped; the walk and the
rendered block are bounded (4 levels, 200 files, 8 KiB). A marker reports known omissions,
but the listing is not exhaustive even without one: search the profile's `workspace/`
when an expected note is missing. The prompt's appended blocks run least to most volatile —
project guidance, then the prelude, then this listing. Guidance leads because it changes
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
- Bash sessions die when the host process exits (server exit, host crash, SSH drop). Local in-process
  sessions also end when their owning session runtime is released (for example, `/new`).
