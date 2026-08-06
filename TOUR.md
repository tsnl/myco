# A close reading of myco

This is a route through the codebase for someone who wants to *read* it —
not to fix one bug, but to understand how the whole thing hangs together
and why it is shaped the way it is. It goes bottom-up through the module
graph, one stop per layer. Each stop says what the layer is for, what to
read in what order, what to notice while you read, and where the code
rewards slowing down.

Companions: `README.md` says what myco does, `AGENTS.md` says how to work
on it (and states the invariants this tour keeps pointing at), `TODO.md`
says what is deliberately unfinished. Read all three before the code.

References below are `path` plus an identifier — a type, function, or
test name you can grep for. Line numbers rot; names mostly don't. When a
stop says "read the comment on X", the comment is the point: this
codebase spends its comment budget on *why* and on invariants, and the
best design writing in the repo lives above the functions it justifies.

## Before the code

Run it. There are three process roles, and holding them apart makes
every module's place obvious:

```bash
cargo test --locked                  # the suite runs without any provider or network
cargo run -p myco -- --mode serve    # THE server: http://127.0.0.1:7773/api
trunk serve                          # the Yew GUI on :8080, proxying /api
cargo run -p myco -- -p "explain src/"   # one-shot turn over the API
python3 clients/myco.py ask "hi"     # the same, from Python
# ssh <alias> myco --mode host       # the per-machine tool worker (never run by hand)
```

You will need a model catalog in `~/.myco/v2/config.toml` and a user
roster in `~/.myco/v2/server.toml` (see `README.md`); myco ships no
built-in models and refuses to invent an identity for you. Both refusals
are design positions you will meet again below.

The premise to hold while reading: **one server, one door**. Everything
is a client of the REST API — the web GUI, `myco -p`, `clients/myco.py`,
the agent's own nested tools. A session is a durable log of attributed
entries; the agent's "hands" (bash, editor) run on whatever host a tool
call names, while the "brain" (config, keys, session store) stays on the
machine the server runs on. And every interactive surface — a shell, a
subagent — shares one social contract: a keyboard lock that says who may
write, with every handoff recorded in the transcript.

## The map

Four crates. The wasm boundary and the vocabulary/surface split are the
two divisions that earn their keep:

| Crate | One line |
|-------|----------|
| `myco` | Everything server-side: the binary, the runtime, the tools, the drivers |
| `myco-types` | The shared vocabulary: `Entry`/`Author`/`Content`, tool calls, `ShellScreen`/`ShellLock`. Serde-only, wasm-safe, bottom of everything |
| `myco-api` | The API surface on top: the `MycoApi` trait, request/response types, `StreamEvent`, `@mention` parsing. Re-exports all of `myco-types` |
| `myco-gui` | Minimal Yew client; one URL per conversation |

Plus `clients/myco.py`: the REST surface as a dependency-free Python
client, doubling as executable protocol documentation.

Inside `myco`, the module graph is acyclic; read it bottom-up, which is
what the stops below do:

```
core → models → config → session → prompts → machines → agent
                                  → auth
server (composes everything) → web / cli / admin / subagent
```

Two structural facts to verify early: `myco-types` depends on nothing in
the workspace, and `myco-gui` depends on `myco-api` alone (which re-
exports the vocabulary) — the browser deserializes the same types the
store persists. Server internals (`models`, `session`, `machines`,
`agent`) import `myco_types` directly and never the API crate: the wire
surface sits at the edge, not under the guts. `src/test_support/` is a
feature-gated module (`test-support`), enabled through the crate's dev-
dependency on itself, so one lib build serves unit and integration tests
alike.

---

## Stop 1 — `src/core/`: the primitives

Small enough to read completely. These live here because every layer
needs them and nothing here may depend on anything above.

Read: `mod.rs`, then `fs.rs`, `image.rs`, `external_command.rs`.

Notice:

- The doc comment on `CancelToken` defines *sticky* cancellation — a
  waiter that subscribes after the cancel still wakes. The tests below it
  are written as proofs of that sentence, including a 100-iteration race
  loop. This is the contract every cancel path in the workspace assumes.
- `fs.rs`: `$MYCO_HOME` is the test seam — nothing anywhere may hardcode
  `~/.myco` — and v2 data under `<home>/v2` shares nothing with v1 by
  design; no migration is attempted.
- `image.rs` sniffs media type from bytes, never the extension, and caps
  on `base64_len(meta.len())` *before* reading the file. The test
  explains why file size is the wrong number: a 4 MiB file is already
  5.3 MiB on the wire.
- `external_command.rs` is the registry of every program myco spawns.
  Since the crate consolidation its enforcement test
  (`every_literal_spawn_goes_through_the_registry`) scans the *whole*
  `src/` tree — including `machines/`, where the spawning actually
  happens — so the claim in its doc is now mechanically checked.

Slow down at: the empty-`PATH`-entry case in `find_in` (a POSIX trap: an
empty component must not mean cwd), and the fallback in
`ExternalCommand::resolve` that spawns the bare name so a missing
program fails with the OS's error, not a myco-invented one.

## Stop 2 — `myco-types` and `myco-api`: the vocabulary and the surface

Read these before any consumer: the noun list for everything else —
what a conversation *is*, what the wires carry, and the one trait every
client holds.

Read: `crates/myco-types/src/lib.rs` top to bottom, then
`crates/myco-api/src/lib.rs`, then `mention.rs` with its tests.

Notice:

- The conversation types live in `myco-types` — not in the model layer —
  because `session` stores them, the host protocol and the HTTP wire both
  carry them, and `models` only *projects* them onto providers. `Entry {
  author, at, body }` with `EntryBody::{User, Agent, ToolResults}` is
  the durable shape; one entry maps one-to-one onto a provider message.
- `Author` is intrinsic to the record ("a shared session is unreadable
  without it"), and the display name is denormalized so a transcript
  survives roster changes.
- `MycoApi` is *the* seam of the architecture: one async trait,
  implemented by the in-process `Server` (as `UserApi`) and by
  `client::HttpClient`, and identity is a property of the handle, not a
  parameter on each call. Keep this in mind for Stops 8–9.
- The interactive-surface vocabulary reads as one design: `ShellLock` is
  *who holds a keyboard* — a shell's or a subagent child's, one enum, one
  meaning, riding both the host protocol and the HTTP wire (which re-
  exports it under its historical name `ShellLockMode`). `Shell` carries `title` (a person's label) beside `id`
  (the address); `ShellScreen` carries plain `text` beside styled `runs`
  (colors pre-resolved to `#rrggbb` server-side, the cursor its own run);
  `ShellWsInput`/`ShellWsOutput` are the socket frames, and their doc
  states the transport doctrine — the socket carries nothing the REST
  surface doesn't; a client without it loses latency, not capability.
- Display policy (`truncate_json_strings`, `tool_input_json`) lives in
  the wire crate on purpose: how a tool call is summarized is a property
  of the conversation, and every frontend must agree.
- `mention.rs` is a page of char scanning that defines the product's
  whole social contract, deliberately mechanical ("an explicit `@`
  prefix, never a guess about intent"). It lives here because the server
  gates agent turns on it and the client highlights from it — both sides
  need the same answer.

Slow down at: `a_message_event_round_trips_with_its_entry_intact`, a
test used as a spec — its assertion documents that `entry.at` is the
identity clients dedupe on, so it must survive the wire unchanged. The
payoff is the GUI's `merge`/`reconcile` (Stop 10).

## Stop 3 — `src/session/`: the document store

"Persistence only: how a conversation is stored, not how one is
produced." Compaction is split exactly along that line — the document
work is here; the agent run that writes the summary is Stop 7's
`run_compact_worker`.

Read: `mod.rs` (types, save/load, listing), then `lock.rs`,
`compact.rs`, `attach.rs`.

Notice:

- `SESSION_FILE_VERSION` and the two-stage load: a `VersionProbe` that
  deserializes only `version` before the full parse, so an old file is
  rejected for its version, not for a misleading "missing field" error.
- `ActiveSession` is `Arc<Mutex<Session>>` — the shared live handle the
  runtime and the `session_meta` tool both mutate. `persist_entries`
  writes only on force/entry-count/usage change, and a `None` usage
  keeps the stored value rather than clearing it.
- `SessionKind` is the one visibility predicate: only `User` sessions
  list by default; `Subagent` and `Compact` children stay hidden but
  remain real sessions — which is exactly what lets the GUI open a
  subagent child as an ordinary URL (Stop 10).
- `lock.rs`'s module doc is the whole design argued in prose: why the
  lock file is a separate inode, why readers are never blocked, why
  flock's process-death semantics mean there is no stale-lock recovery
  to get wrong — and why an *unavailable* lock degrades to unlocked
  instead of refusing to open the session.
- `compact.rs`: `select_tail` never ends mid tool loop — one of three
  sites enforcing the well-formed-history invariant (see "Themes").
  `link_compact_pair` is three lines of code under five lines of doc:
  the successor is saved before the predecessor points at it, because
  one crash order is recoverable and the other is not.
- `attach.rs`: the `@path` extension only selects the token; the media
  type comes from the bytes.

Slow down at: the golden-fixture test near the bottom of `mod.rs`
(fixture `session_v2_all_variants.json`). Its doc comment is the best
paragraph in the module: the on-disk schema is spelled with Rust
identifiers, so renaming a variant compiles clean and silently makes
every stored session unreadable. The test pins byte-identical
re-serialization.

## Stop 4 — `src/models/`: the provider boundary

One trait, one method: `GenerativeModel::generate(&self, &[Message]) ->
AsyncStream<Result<MessagePart, GenerateError>>`. Every driver is
stateless; history is the caller's.

Read: `mod.rs` (trait, catalog, `GenerateOutput`, errors), then
`driver_core.rs` and `sse_parser.rs`, then one driver end-to-end
(`anthropic.rs` is the richest), then skim the other two for their
deltas (`openai_common.rs`, `openai_responses.rs`,
`openai_completions.rs`).

Notice:

- `driver_core.rs` is the whole streaming architecture in ~80 lines:
  `spawn_generate` measures the *exact serialized body* against
  `MAX_REQUEST_BYTES` and streams through a backpressured channel;
  `drive_sse_stream` treats every failed send as "consumer dropped —
  stop reading", which is how dropping the stream cancels generation and
  billing.
- `sse_parser.rs` buffers *bytes*, not text: network chunks split
  mid-UTF-8-sequence, and per-chunk conversion would corrupt streamed
  text with U+FFFD. The test feeds one byte at a time. The cheapest
  high-value read in the workspace.
- The `Recovery` taxonomy: failures that are a property of the *history*
  (a too-large request) cannot be fixed by retrying — the fix is
  `Recovery::OmitLastMessage`, consumed by the server's turn driver
  (Stop 8).
- Id-less tool calls are rejected loudly in all three drivers: such a
  call is poison that flows into persisted history and 400s on every
  later request, resume included. The empty-assistant-message guard
  exists in triplicate for the same reason — search "thinking-only
  turn" in each driver.
- Anthropic-specific shaping worth reading with the tests: cache
  breakpoints on the last two messages, role merging with tool results
  *leading* the user turn they answer, the auth header chosen by token
  shape.

Slow down at: `GenerateOutput::from_stream_with_hook` — parts back into
a finished turn, with sparse slots so out-of-order indices are legal but
missing ones are a `MalformedResponseError`. The `on_part` hook is how
the agent streams deltas to the UI while accumulating the durable entry.

## Stop 5 — `src/config/`: resolve once, pass down

Startup resolution of everything impure: config file, environment,
roster, ssh aliases. The design is visible in one signature —
`resolve_with` takes every impure input as a parameter, and
`Config::resolve` is the only place the real ones are named. That is why
a 500-line test module exercises catalog resolution and precedence with
zero filesystem and zero env mutation.

Read: `mod.rs`, `file.rs`, `harness.rs`, `roster.rs`.

Notice:

- Hard vs soft errors: shape problems fail startup; a credential
  *lookup* that fails is recorded per-entry as `auth_error` and surfaces
  only when the model is actually used.
- "Identity before anything else": the roster resolves before the
  catalog, and there is deliberately no fallback — inventing an identity
  from `$USER` would write a name nobody registered into a durable,
  shareable transcript. The roster holds no credentials; granting access
  is Stop 9's business.
- `harness.rs`: SSH details stay in ssh_config where OpenSSH reads them
  natively; myco adds only `BatchMode=yes`, because the NDJSON pipe is
  not a TTY and OpenSSH must never prompt there.

Slow down at: the journey of `max_image_base64_bytes` — resolved once
for the model the process will run, handed to the harness, and riding
the remote spawn *argv* rather than the NDJSON handshake. The comment in
`harness.rs` explains why argv is sound; tests guard the seam from both
ends.

## Stop 6 — `src/machines/`: the hands

Harness, host pair, and every tool. The biggest module; read it as
three sub-tours.

**Harness** (`harness/mod.rs`, `preflight.rs`): the host pool. Local is
always in-process (a remote literally named `local` is rejected);
remotes start unconnected and connect on first tool call. Root-only
services (`session_meta`, `subagent`, `prelude`) are installed only on
the in-process local worker and never receive the injected routing
`host` field.

**Host pair** (`host/protocol.rs`, `host_worker.rs`,
`host_controller.rs`): read the protocol first — and notice it carries
`myco_types` structs verbatim (`ShellScreen`, `ShellLock`): one
definition of the terminal serves the NDJSON wire and the HTTP wire, so
the server's screen route is a passthrough, not a mapping. Beyond `Hello`/
`ToolCall`/`AgentFinished`, the `Shell*` requests are the **observer
surface** — a person watching or driving a live bash session — and the
protocol doc states the rule: they carry no `agent_id` (watching is not
ownership) *except* `ShellStart`, where the id is the point: a terminal
the user opens is owned by the session's agent so its bash tool can
drive it. There is still no cancel message: the worker mints a fresh
`CancelToken` per call, so a cancelled remote tool runs to completion
and only the local waiter is abandoned (`TODO.md` tracks this
honestly). Version skew is a *connect* error, not a latent tool failure
hours later.

**Tools** (`tool_services/`): the `ToolService` trait plus
`HostDispatchContext { agent_id, cancel }` — that pair is the entire
ambient context a tool gets. Read `tool_input_schema`'s long doc about
why schemars output is scrubbed: the schema is prompt engineering. Then:

- `bash_service/` — `mod.rs` (agent surface + session internals),
  `observer.rs` (the person's half), `screen.rs` (vt100 → styled runs),
  `pty.rs`: each agent call is a *bounded interaction*
  against a live child — write optional stdin, collect until idle gap,
  timeout, byte cap, or exit. Sessions are owned per agent and reaped on
  agent finish. Below the agent surface sits the observer surface — the
  web terminal's half: non-consuming `Scrollback` tails, the vt100
  `screen` model rendered to styled `ScreenRun`s (`render_screen`, with
  the xterm-256 palette resolved server-side in `color_hex`), the
  keyboard `ShellLock`, `shell_start` (user-opened, user-held terminals
  in the same session table), `shell_rename` (titles are labels; ids
  are addresses), `shell_resize` (TIOCSWINSZ → SIGWINCH, gated on the
  user lock), and `shell_wait_change` — the change-driven wait the
  WebSocket pusher blocks on, with the `Notified::enable` dance that
  prevents a lost wakeup. The pty plumbing (`pty.rs`) is deliberately
  libc-only: openpty, controlling-tty setup, nonblocking master I/O,
  `resize`.
- `text_editor_service.rs`: mutations require a read-stamp — a content
  fingerprint recorded at view time and re-checked under one lock across
  check+mutate+record.

Slow down at three spots:

1. The `dead`-flag handshake in `host_controller.rs` (`submit`,
   `run_reader`, `run_writer`): three distinct races, each commented,
   converging on one pairing argument — nobody awaits a reply that
   cannot come.
2. `collect_output`'s generation counter: the idle-gap heuristic is
   wrong whenever something happens that produces no bytes *yet*, so
   writers bump an `AtomicU64` and waiters reset their idle clock when
   it moves.
3. `kill_session_process`: four lines carrying a real safety argument —
   skip `kill(-pgid)` only when the leader exited *and* every pipe hit
   EOF, because signaling a fully-dead group could hit an unrelated
   process that recycled the pid.

## Stop 7 — `src/prompts/` and `src/agent/`: what the model is told, and the turn loop

**Prompts** (`prompts/mod.rs`, `prelude.rs`, `fragments/`): the
organizing pressure is the prompt cache, and it is stated, not implied.
`epilogue_with` orders blocks least-to-most volatile (a unit test
asserts the order — cache economics as a testable claim); `model_stamp`
is identity-free by contract; `session_stamp` rides a *message*, dated
with creation time, not now. The workspace listing shows ages in days
and sorts by path — both choices made for cache stability, both
documented.

**Agent** (`agent/mod.rs`, `compact_worker.rs`): the turn loop's module
doc states the invariant everything else rests on: whatever a turn does
— end cleanly, error, get cancelled mid-tool, or get truncated mid-call
by max_tokens — the transcript left behind must be a prefix the provider
will accept on the next request.

Notice, in `interact_entry`:

- Tool dispatch is keyed on the *presence* of tool uses, not the stop
  reason — max_tokens can truncate a turn mid-call, and a tool_use
  nothing responds to makes the whole history unsendable.
- `HistoryCheckpoint` fires only at well-formed boundaries — never
  between an assistant tool_use and its results, because that prefix is
  exactly what a context fork must not inherit.
- `PendingInput` is the pre-emption seam, draining at the same
  boundaries: messages that arrive mid-turn are folded into history
  right before the next generate, so the model reads them in the turn
  they interrupted. The server's room inbox (Stop 8) is the production
  supplier.

The cancel path: `CANCEL_TOOL_GRACE` gives a cancelled dispatch two
seconds to clean up before a synthetic `cancelled` result is recorded.
The comment above the `select!` is the densest "why" in the module.

Also read: `max_tokens_mid_tool_call_answers_the_dangling_tool_use` and
`checkpoint_fires_only_at_well_formed_boundaries` — the invariant made
concrete, with the *next* turn's success asserted.

## Stop 8 — `src/server.rs`: the session runtime

The object that does the work. Everything above composes here: one
`Live` handle per resident session — an agent task, an unbounded `Cmd`
queue, a broadcast event feed, a fresh `CancelToken` per turn, the
session's write lock, the subagent hold, the per-hold typed-keys
buffer.

Read: `src/server/mod.rs` (the runtime: `Live`, `ensure_live`, the
agent task), then `room.rs`, `observer.rs`, `user_api.rs`, then
`src/subagent.rs`.

Notice:

- `Room` is the multiplayer design in one struct: who has posted
  (`participants`) and what has been accepted but not yet folded into
  history (`inbox`), under one lock. `post_message` decides whether a
  message wakes the agent, broadcasts it, and enqueues it in a single
  critical section. The rule (`Room::wakes_agent`) is `README.md`'s:
  explicit address, with one carve-out — a session nobody else has
  posted in is a private line. "An agent that guesses wrong talks over
  people; one that waits to be named never does."
- `accept_room_note` is the other half of the social contract: every
  keyboard handoff, opened terminal, rename, and subagent takeover goes
  through it as an attributed, *non-waking* message — watchers see it at
  once, the agent reads it at its next boundary. Keystrokes are the one
  deliberate batch: they stream one at a time now (the WebSocket), so
  they accumulate in `Live::typed` per hold and flush as a single
  caret-notation note when the keyboard returns (`readable_keys`).
- The subagent surface mirrors the shell surface on purpose:
  `live_children` lists live child sessions (metadata reads only — the
  comment explains why never a snapshot), `subagent_lock` flips the
  child's `user_hold` and announces transitions in the *parent*
  transcript, `subagent_input` requires the hold and echoes what was
  said. `src/subagent.rs` closes the loop: the tool refuses politely
  while a person holds the child.
- `run_turn` owns a turn's lifecycle, and the wire's `TurnFinished` is
  its last act — sent only after persistence, so a client that
  refetches on it reads a transcript that already holds the answer. The
  test `the_wire_ends_a_turn_once_and_only_after_persistence` pins both
  halves.
- `ensure_live` is the boot sequence; the comment about lazy durability
  — "a session becomes durable when it has content, not when it is
  opened" — is pinned by an integration test.
- `UserApi` is `Server` bound to one caller; the `MycoApi` impl lives on
  it, so no route can reach the runtime without naming who is asking.

## Stop 9 — the door: auth, admin, web, client, myco.py

Read: `src/auth/mod.rs` and its tests, then `src/admin.rs`,
`src/web.rs`, `src/client.rs`, `src/cli.rs`, `clients/myco.py`.

Notice:

- `auth`'s module doc explains the grant choice: RFC 6749 §4.3 because
  "the failure modes are known and the client side is boring."
  Durability is deliberately tiered — users snapshot to `auth.json`,
  tokens live only in memory (a restart logs everyone out), one-time
  codes live and die with the process (they are redeemed against *this*
  server, so `myco auth code` mints over HTTP as the operator rather
  than touching files), and passkeys persist in `passkeys.json` — the
  server's own file, so the admin CLI's `auth.json` rewrites cannot
  clobber it. Passwords are PBKDF2 with a self-describing hash; tokens
  and codes are plain SHA-256 digests of high-entropy values. `login`
  burns a real verification against `dummy_hash()` for unknown users so
  response timing does not enumerate the user list; the passkey
  login/start route answers identically for "no such user" and "no
  passkeys enrolled" for the same reason.
- The WebAuthn ceremonies (`web.rs`, `passkey_*` routes) lean on a wire
  coincidence that is actually a standard: webauthn-rs serializes its
  challenge types to exactly the JSON the browser's own
  `PublicKeyCredential.parse*OptionsFromJSON` / `credential.toJSON()`
  speak, so the GUI's bridge is two ten-line JS functions and the JSON
  crosses this codebase untouched. The relying party comes from
  `[passkeys]` in server.toml; the localhost default allows any port,
  which is what makes passkeys work through an SSH tunnel and trunk's
  dev proxy without configuration.
- The roster→auth link is made by the *binary*: `Server` construction
  reconciles roster names into the store.
- `src/web.rs`: every REST route is a one-liner over `MycoApi`; the
  `Caller` request guard turns a bearer token into a `UserApi`, so
  there is no anonymous path through the module. Two routes are more
  than one-liners, deliberately: `serve` writes the operator token to
  `$MYCO_HOME/v2/operator.token` (0600) so local clients authenticate
  without a login step, and `shell_ws_run` is the shell WebSocket's
  event loop — one task, two sources (client frames; the shell's change
  signal), screens pushed only when they differ, coalesced. Its doc
  states the transport doctrine: same guts as the REST routes, so a
  client without the socket loses latency, not capability. The `?token=`
  concession exists twice (SSE, WS) for the same reason: browsers cannot
  set headers on either.
- `src/client.rs` is the other implementation of the trait — `from_env`
  reading `$MYCO_API`/`$MYCO_API_TOKEN` as exported by `--mode serve`,
  so tools the agent spawns reach the API as the operator. `src/cli.rs`
  is the thin `-p` client over it (spawning a detached server when none
  answers); `clients/myco.py` is the same surface again in Python,
  urllib-only, on purpose.
- `src/admin.rs`: no self-service surface — everything that mints or
  revokes access happens from a shell on the machine, by someone who
  already has the box.

## Stop 10 — `crates/myco-gui`: the frontend

Read: `state.rs` first, then `transcript.rs` and `work.rs`, then
`main.rs` (the pages and the `Conversation` wiring), then `auth.rs`,
`highlight.rs`.

**The state machine** (`state.rs`) is the part worth close reading: one
`ConvState`, one `apply`, every event a typed action, yew-free and
unit-tested on the host. The module doc explains the family of bugs
this shape exists to make impossible (a long-lived task holding a Yew
`UseStateHandle` reads the value from the render that created it).
Inside `apply`: `merge` (identity is the timestamp), `reconcile` (the
server's copy wins, but a delivered-not-yet-persisted message survives
the poll), `Polled` standing down while the stream buffer is non-empty,
and `arrivals` — messages delivered mid-turn render *after* the stream
buffer, because that is where the room actually saw them, and fold into
the transcript at the next boundary deduped against what the server
absorbed.

**The work panel** (`work.rs` for the machinery, wired in `main.rs`) is
the multiplayer cockpit: a horizontal
split beside the chat, one tab per live shell and live subagent (tabs
come and go with the lists; `+` opens a terminal of your own), the
active tab filling the panel through one shared `WorkView` — chrome,
lock button, rename affordance, input row — whatever the body is. A
subagent tab's body is `render_transcript`, the *same* function the main
pane uses: the chat window costs no second chat implementation. A pty
tab's body is `render_screen` over the server's styled runs, and in raw
mode the body itself holds keyboard focus: `key_bytes` translates
keydowns the way xterm would (Ctrl chords, Esc as ESC, arrows honoring
application-cursor mode, Alt as the ESC prefix, Ctrl+Shift+C/V left to
the browser), a per-shell `TypeQueue` keeps REST fallback bytes in typed
order, and the active pty tab rides a WebSocket — frames up, screen
pushes down, the poll loop standing down while the socket lives.

Notice the shape of the side-channel state: everything the poll task
writes lives in `use_mut_ref` cells (`PanelBuf`, the active tab, the
socket link), with one shared `bump` closure as the render trigger — a
monotonic counter, so two writers can never set the same value and lose
a render.

## Stop 11 — tests as claims

`AGENTS.md` says test names should state invariants. The integration
suite delivers; read these as the codebase's own summary of what it
promises. The enabling trick is `test_support`'s `ScriptedModel`: it
expands finished `GenerateOutput`s back into the streaming parts a real
driver would produce (and can be fed mid-test via `push`, for turns that
must name ids that did not exist at build time), so the agent loop,
harness, tool dispatch, SSE publication, and persistence all run for
real; only the network hop is replaced.

- `tests/myco_api_roundtrip.rs` — the `MycoApi` contract through `dyn
  MycoApi` only: streaming publishes deltas before the turn finishes; an
  abandoned session leaves nothing on disk.
- `tests/web_auth.rs` — no route answers without a token; a *prefix* of
  a real token fails; writes are attributed to the token's owner. The
  code and passkey flows run end to end — the latter against a software
  authenticator (`webauthn-authenticator-rs`'s SoftToken), including the
  claim that gives passkeys their point: a fresh store from the same
  files signs the user back in after the restart that voided every
  token.
- `tests/web_multiplayer.rs` — the room rules over real HTTP with two
  token holders, then the pre-emption half, driven by a gated model that
  holds a turn provably mid-flight.
- `tests/web_shells.rs` — the shell surface end to end, local and
  remote: watch, take, type, screen; keystroke notes flushing on
  handoff; a user-opened terminal usable at once, announced, renamed.
- `tests/web_subagents.rs` — the subagent surface: delegate, list, take
  the child, talk to it directly, watch the parent get refused
  mid-hold, hand back, watch it succeed.
- `tests/cancel_composed.rs` / `tests/host_cancel_desync.rs` /
  `tests/concurrent_host_tools.rs` — cancel leaves no process-group
  survivors and a well-formed history; cancel or drop mid host call
  never desyncs the NDJSON pipe; same-turn host tools genuinely overlap.
- `bash_service/tests.rs` — shell behavior claims down to the pty:
  styled runs with palette-resolved colors, resize visible to the
  child's own `stty size`, `shell_wait_change` waking on echo.

---

## Themes to trace

1. **Well-formed history.** The one invariant, enforced at three
   independent sites: the turn loop answers every tool_use regardless of
   stop reason (Stop 7), checkpoints fire only at provider-acceptable
   prefixes (Stop 7), and compaction's `select_tail` never cuts mid tool
   loop (Stop 3). The drivers add their own guards so poison never
   enters the history at all (Stop 4).
2. **Attribution.** Roster (who exists, Stop 5) → auth store (what they
   know, Stop 9) → `Caller`/`UserApi` (who is asking, Stops 8–9) →
   `Entry.author` (who wrote it, Stop 2). At no point can an
   unregistered name enter a stored transcript.
3. **One keyboard.** The lock is the same idea everywhere it appears:
   shells and subagents, GUI and API, local and remote. Writes are
   gated, reads never are, every transition is an attributed non-waking
   transcript fact, and both sides fail politely rather than interleave
   — the agent's bash write on a user-held shell, the parent's
   `subagent` call on a user-held child, the user's input on an
   agent-held one.
4. **One door, layered transports.** REST is canonical; SSE streams the
   conversation; the shell WebSocket accelerates the terminal. Each
   addition is a projection of the same guts (`shell_ws_run` calls the
   same `UserApi` methods the routes do), so no transport can drift
   into its own semantics.
5. **Cancellation.** Sticky token (Stop 1) → fresh token per turn
   (Stop 8) → grace window and synthetic results (Stop 7) →
   process-group kill and generation bumps (Stop 6) → the honest gap:
   the host protocol carries no cancel, and `TODO.md` says so.
6. **Bounded everything.** Every cap has a number and a paragraph:
   request bytes, image base64, bash capture and scrollback, event
   buffer, session cap, terminal size. Each names the failure it
   prevents.
7. **Prompt-cache economics.** Identity-free system prompt,
   volatile-last block ordering, day-granularity listings, session
   stamps riding messages, cache breakpoints on the last two messages.
   Several are asserted in unit tests.
8. **Fail loud vs degrade, chosen per case.** Unreadable auth store:
   exit. Unavailable flock: continue unlocked, with a warning. Version
   skew: connect error. Missing roster: startup error. Remote
   `shell_wait_change`: degrade to a server-side pause, documented at
   the site. Collect the arguments and you have the project's
   engineering ethic.

## Rough edges

A close reading should also notice where the map and the territory
disagree. Known spots, worth reading rather than fixing reflexively:

- `src/models/mod.rs` still has no module doc; the crate's mission
  statement lives in scattered comments.
- `auth`'s `PBKDF2_ITERATIONS` doc says it is lowered under
  `cfg(test)`; it is not — the suite gets its speed from
  `with_work_factor` instead. Relatedly, disabled accounts return a
  distinct error *before* any hashing — a deliberate, documented
  exception to the anti-enumeration stance, and a genuine timing
  oracle.
- The shell WebSocket does not reconnect on its own: if it dies while
  the tab stays active, REST polling covers the gap until the tab is
  switched away and back. Cheap to live with; documented nowhere else.
- Remote hosts have no push primitive on the NDJSON protocol, so a
  remote terminal's server side polls at 150ms behind the socket
  (`HostController::shell_wait_change` says so). An unsolicited-message
  channel on the host protocol is the honest fix if remote typing ever
  feels laggy.
- `session_stamp` has exactly one production caller: compaction.

## Questions to test your reading

Answerable only from the code; each names its stop.

1. A user posts a message while the agent is three tool calls into
   someone else's turn. Trace exactly when every watcher sees it, when
   it lands on disk, where it renders relative to the streaming turn,
   and how the GUI avoids showing it twice. (Stops 8, 10)
2. Why does dropping the stream returned by `generate` stop the
   provider from billing you, and which two functions make that true?
   (Stop 4)
3. `max_tokens` cuts off a turn halfway through emitting a tool call.
   What ends up in history, why does the *next* request not 400, and
   which test proves it? (Stop 7)
4. You type `vim`, `iEsc`, arrows, `Ctrl+C` into a pty tab. Trace each
   keystroke: what bytes leave the browser, on which transport, what
   orders them, what the child receives, how the echo comes back, and
   what lands in the transcript — and when. (Stops 6, 8, 10)
5. The user opens a terminal called `scratch`, renames it `build
   watch`, and walks away. What can the agent do with it at each point,
   how does it even know it exists, and what would renaming have broken
   if titles were identities? (Stops 6, 8)
6. A person takes over a subagent child mid-delegation. What exactly is
   refused, what still works, what does the parent transcript record,
   and which test walks the whole loop? (Stop 8)
7. Compaction crashes between its two saves. Which order of
   `link_compact_pair` writes makes that recoverable, and what would
   the other order leave behind? (Stop 3)
8. Why must the WebSocket's change-wait call `Notified::enable` before
   checking the watermark, and what is the worst case if it did not?
   (Stop 6)
9. How does a signal sent to a quiet bash session produce a snapshot of
   the child's *reaction* rather than an empty buffer? (Stop 6)
10. `myco auth disable ada` runs while ada has a live token and a
    session mid-turn. What stops working, what keeps working, and which
    two mechanisms make the token dead? (Stop 9)
