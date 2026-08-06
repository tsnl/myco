# `myco`

A minimalist coding agent that works across your machines over SSH. One
**server** (`myco --mode serve`) with one door — the REST API — and thin
clients through it: the multiplayer web GUI, `myco -p` for one-shot turns,
and `clients/myco.py` (or plain HTTP) for scripts and agents driving agents.

## Why use it?

- **One brain, many machines.** Every tool call names its host: `local` or any
  concrete `Host` alias from `~/.ssh/config`. Remotes attach over SSH on
  demand and need only `myco` on PATH — no config, no keys.
- **Real computer use.** Bash (including multi-turn sessions) and a surgical
  file editor on each host; search and browsing compose from the tools already
  on your machines (`rg`, `curl`, `ck`, …) via bash. A session can run under
  a real pty (`pty: true` on start — for TUI apps and anything that checks
  isatty), and the `screenshot` action renders any session's terminal screen
  as text, so the agent reads an editor or `top` the way you would.
- **Async everywhere.** Input queues while a turn runs; each conversation is
  a URL and agents run in parallel server-side.
- **Sessions you can resume.** Titles, scratchpads, and full history
  live under `~/.myco/v2/` — reopen any session by URL, or from another
  client. Archive the ones you are done with; they stay readable.
- **Attributed history.** A session is a log of `Entry { author, at, body }`,
  so every message names who wrote it — a person, the agent, or the system.
  Authors come from a roster you write down (`~/.myco/v2/server.toml`); myco
  refuses to start without one rather than invent a name.
- **Nested agents as a tool.** The root-only `subagent` tool runs one full
  turn of a hidden child session per call — no curl, works behind strict
  firewalls, optional context forks. The same surface exists as HTTP
  (`$MYCO_API`) for scripts.
- **Live streaming.** Each session exposes an SSE feed
  (`/api/sessions/<id>/events`) of text/thinking deltas, tool starts and
  finishes, and whole messages; the GUI streams it and reconciles with a
  slow poll.
- **Shared terminals with a keyboard lock.** The GUI's work panel is a
  horizontal split beside the chat: a tab per live bash session on **every
  host** — local and remotes alike, reached over the same NDJSON host
  protocol the tools use — plus chips for whatever tool calls are in
  flight. Tabs come and go as shells do; the active one fills the panel.
  A pty session is a **real terminal emulator**: colors and cursor render
  from the server's vt100 screen, the window resizes to fit the panel
  (SIGWINCH and all), and once you take the keyboard you type straight
  into it — Ctrl+C, Ctrl+D, Esc, arrows, F-keys, Alt chords, paste
  (Ctrl+Shift+C/V stay the browser's copy and paste). The active terminal
  rides a **per-shell WebSocket**: keystrokes go down as ordered frames
  and screens push back the moment they change, so echo is one round trip
  and an idle terminal costs nothing; if the socket drops, the REST
  polling underneath takes over. Piped sessions keep an honest line-based
  input row. You can also **open terminals yourself** (the `+` tab):
  a user-opened terminal is a real bash session owned by the session's
  agent — fully addressable by its tools once you hand the keyboard over —
  starting user-held, announced in the transcript, and renameable (names
  are labels; the id stays the address). The lock decides who may write —
  shells start agent-held, the agent's writes fail politely while you hold
  it, and both sides always read. Every handoff lands in the transcript as
  an attributed, non-waking message naming the host, and everything you
  typed during a hold flushes as one such note when you hand the keyboard
  back — so the agent reads the whole intervention at its next boundary
  instead of discovering a mutated shell.
- **Subagents get the same window.** The `subagent` tool's children are
  full sessions, and each live child is a tab too — a chat rendered by the
  same transcript view as the main pane, streaming while the child works.
  The same lock applies: take a child over and you talk to its agent
  directly, the parent's `subagent` calls to it are refused until you hand
  it back, and the takeover, your messages, and the handoff are all
  recorded in the parent transcript. Or pop it out: every child opens as a
  full session by URL.
- **Shared sessions the agent stays out of.** Several people can hold one
  session. While you are the only person in it, everything you say is for
  the agent — as before. Once someone else posts, the room has rules: the
  agent answers when it is named (`@myco`, `@agent`, `@assistant`, or the
  model key it is running as) and otherwise listens. A message between two
  people is recorded, delivered to everyone watching, and read by the agent
  as context — it just does not cost a turn. Mentions are highlighted.
- **Mid-turn messages pre-empt.** A message that names the agent while it
  is already working does not wait behind the whole turn: it is folded
  into the turn in flight at the next safe boundary (a completed tool
  round) and answered by it. Messages between people posted mid-turn ride
  along the same way, so the history reads in the order the room spoke.

## Run

```bash
cargo run -p myco -- --mode serve          # the server: http://127.0.0.1:7773/api
trunk serve                                # web GUI on :8080 (proxies /api)
cargo run -p myco -- -p "explain src/"     # one-shot turn (spawns the server if needed)
python3 clients/myco.py ask "explain src/" # the same, over the same API
```

There is no terminal REPL: the server is the product, and every client —
GUI, `-p`, scripts — speaks the same REST API. `--mode serve` writes the
local operator's bearer token to `~/.myco/v2/operator.token` (0600), which
is how `myco -p` and local scripts authenticate without a login step.

The web GUI keeps the terminal's visual identity: monospace, dark, USER
rules — minimal chrome by design. Tool calls render as collapsed cards
(name, pretty-printed arguments with long strings elided, result folded in);
click one, or the transcript-wide `verbose` toggle, to see the call exactly
as the model made it. Code blocks are syntax-highlighted. Images render
inline — attachments you post, `view_image` results in their tool cards —
from their stored `data:` URLs; the client never fetches a remote image on
the transcript's say-so.

`trunk serve` needs [trunk](https://trunkrs.dev) and the wasm target
(`rustup target add wasm32-unknown-unknown`). The `Trunk.toml` at the repo
root builds `crates/myco-gui` and reverse-proxies `/api` to the server.

Configure models first in `~/.myco/v2/config.toml` (`[gateways.*]` +
`[models.*]`; `--config <path>` to override), and
register yourself in `~/.myco/v2/server.toml`:

```toml
[[users]]
id = "ada"            # matched against $MYCO_USER, then $USER
name = "Ada Lovelace" # optional; defaults to the id
```

The server refuses to start without a roster that names the
user it is running as — every session entry records its author, and a
name nobody registered has no business in a shared transcript. Override the
path with `--server-config` or `$MYCO_SERVER_CONFIG`.

The roster says who *exists*; passwords are set separately, so a name in the
file is not by itself access to the server:

```bash
myco auth list             # who exists, who can sign in, live sessions
myco auth passwd ada       # prompts; ends ada's existing sessions
myco auth disable ada      # refuse logins, keep the history attributed
myco auth revoke ada       # end sessions without changing the password
```

### Talking in a shared session

Addressing is explicit, never inferred, so the rule is one you can see in the
text you typed:

```
@ada did you see the build?      # to a person — the agent stays quiet
@myco what do you make of it?    # to the agent — it answers
what do you make of it?          # solo session: the agent answers
                                 # shared session: nobody is summoned
```

Remotes just work: the harness spawns `ssh <alias> myco --mode host` lazily, so a
remote only needs your key in `ssh-agent` and `myco` on the PATH used by
non-interactive SSH.

## API

`GET /api/sessions?include_archived=` · `POST /api/sessions` (`{model?,
parent_session?, fork?}`) · `GET /api/sessions/<id>` ·
`PATCH /api/sessions/<id>` (`{title?, archived?, model?, effort?}`) ·
`POST /api/sessions/<id>/messages`
(`{text}`) · `GET /api/sessions/<id>/poll?since=N` ·
`GET /api/sessions/<id>/events` (SSE) · `POST /api/sessions/<id>/cancel` ·
`DELETE /api/sessions/<id>/live` · `GET /api/models` · `GET /api/whoami` ·
`GET /api/sessions/<id>/shells` (all hosts) ·
`GET /api/sessions/<id>/subagents` (live children) ·
`POST /api/sessions/<id>/subagents/<child>/lock` (`{lock}`) ·
`POST /api/sessions/<id>/subagents/<child>/input` (`{text}`, requires the
user hold; echoes into the parent transcript) ·
`GET /api/sessions/<id>/shells/<host>/<shell>?from=N`
(offset-addressed scrollback tail) ·
`GET /api/sessions/<id>/shells/<host>/<shell>/screen` (rendered terminal
screen: plain text plus styled runs with colors and cursor) ·
`POST /api/sessions/<id>/shells/<host>/<shell>/input` (`{data}`, raw bytes,
requires the user keyboard lock) ·
`POST /api/sessions/<id>/shells/<host>/<shell>/resize` (`{cols, rows}`,
requires the lock; pty children get SIGWINCH) ·
`POST /api/sessions/<id>/shells/<host>/<shell>/lock`
(`{lock: "user"|"assistant"}`) ·
`POST /api/sessions/<id>/shells/<host>` (`{shell?, command?, pty?}` — open a
user-held, agent-owned terminal) ·
`POST /api/sessions/<id>/shells/<host>/<shell>/rename` (`{title}`) ·
`GET /api/sessions/<id>/shells/<host>/<shell>/ws` (WebSocket: `input`/`resize`
frames in, `screen` pushes out — the interactive fast path; REST remains the
canonical surface).
Wire types live in `crates/myco-api`.

A session can switch model or reasoning effort mid-conversation:
`PATCH` with `model` (any catalog key) or `effort`
(`low|medium|high|max`; `""` clears the override back to the model's
configured default — set per model with `effort = "…"` in `config.toml`).
The change is validated against the catalog, saved on the session document,
and a live agent rebuilds its model between turns — the turn in flight
finishes on what it started with. The GUI exposes both as topbar pickers
(and a model picker on the new-session page); scripts PATCH them directly
(`clients/myco.py`: `update_session`). Forked children and compaction
successors inherit the override.

`POST /messages` answers with `busy`: whether a reply is coming or already
underway — so a client knows not to wait for a reply that is not coming, and
a note posted mid-turn does not read as the agent going idle. The SSE feed
carries the message itself as `{"type":"message", entry, wakes_agent}`,
emitted the moment it is accepted and ahead of any turn it triggers or joins.
That event is what lets a client place someone else's message in the
transcript as its own record, rather than appending text to whatever was last
on screen, and it arrives even while the agent is mid-turn. The write follows
when the running turn folds the message at its next boundary (or the
session's task drains it), so a client should keep a
delivered-but-unpersisted message on screen across a poll. Tool calls stream
as `tool_started` (with the call's `id`) and complete mid-turn as
`tool_finished`; `turn_finished` arrives exactly once per turn, after the
turn is persisted, so one refetch on it reads the finished transcript.

Sign in with the **OAuth 2.0 password grant** (RFC 6749 §4.3):
`POST /api/auth/token` takes form-encoded
`grant_type=password&username=&password=` and returns
`{access_token, token_type, expires_in, user}`. Every other route requires
`Authorization: Bearer <access_token>`; `POST /api/auth/logout` ends the
session server-side. Tokens last 12 hours, are stored hashed, and live only
in memory — a restart logs everyone out.

Entries a request writes are attributed to the token's owner, not to whoever
started the server. (`GET /sessions/<id>/events` also accepts `?token=`,
since `EventSource` cannot set headers — a query token lands in request
logs, so it is the weakest part of the surface.) `--mode serve` mints a
token for the local operator and exports it as `$MYCO_API_TOKEN` alongside
`$MYCO_API`, so tools the agent spawns reach the API as that user.

## Workspace

- `myco` — **the** crate: session runtime (`server`), agent loop (`agent`),
  multi-host harness + NDJSON host protocol + tool services (`machines`),
  provider backends (`models`), sessions on disk (`session`), config, auth,
  prompts, the Rocket adapter (`web`), the thin `-p` client (`cli`), and
  `--mode host` (the per-machine worker remotes run)
- `myco-types` — the shared vocabulary: conversation records, tool calls,
  terminal screens, the keyboard lock — what the store persists, the wires
  carry, and the browser deserializes
- `myco-api` — the API surface on top of it: the `MycoApi` trait, request/
  response types, stream events (re-exports all of `myco-types`)
- `myco-gui` — minimal Yew web client (one URL per conversation)
- `clients/myco.py` — the API as a dependency-free Python client, doubling
  as executable protocol documentation

## Develop

```bash
cargo test --locked
cargo run --locked -p myco
bash scripts/install-pre-commit-hooks.sh   # optional: CI bar (fmt + clippy) pre-commit
```
