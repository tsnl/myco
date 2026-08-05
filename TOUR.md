# A close reading of myco

This is a route through the codebase for someone who wants to *read* it —
not to fix one bug, but to understand how the whole thing hangs together
and why it is shaped the way it is. It goes bottom-up through the crate
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

Run it. The binary has three process roles, and holding them apart makes
every module's place obvious:

```bash
cargo test --locked            # the suite runs without any provider or network
cargo run -p myco              # the interactive CLI (async REPL)
cargo run -p myco -- --mode serve   # the API server on :7773
trunk serve                    # the Yew GUI (port per Trunk.toml), proxying /api
# ssh <alias> myco --mode host      # the per-machine tool worker (never run by hand)
```

You will need a model catalog in `~/.myco/v2/config.toml` and a user
roster in `~/.myco/v2/server.toml` (see `README.md`); myco ships no
built-in models and refuses to invent an identity for you. Both refusals
are design positions you will meet again below.

The premise to hold while reading: **one session runtime, two
frontends**. The CLI and the web server drive the same `Server` object
through the same interface; a session is a durable log of attributed
entries; the agent's "hands" (bash, editor) run on whatever host a tool
call names, while the "brain" (config, keys, session store) stays on the
machine you launched from.

## The map

The root crate `myco` is the runtime and both frontends; the pillars
live in `crates/`:

| Crate | One line |
|-------|----------|
| `myco-core` | Bottom of the graph: async aliases, `CancelToken`, `myco_home()`/`atomically_write()`, image sniffing, the external-command registry |
| `myco-api` | The wire vocabulary: `Entry`/`Author`/`Content`, the `MycoApi` trait, `StreamEvent`, `@mention` parsing. Serde-only, wasm-safe |
| `myco-session` | Persistence only: session documents, the single-writer lock, compaction's *document* half, search, the console mirror |
| `myco-models` | Provider drivers (Anthropic Messages, OpenAI Responses, OpenAI Chat Completions) behind one `GenerativeModel` trait; the model catalog types |
| `myco-config` | Startup resolution: config.toml catalog, roster, harness config, all impure inputs injected |
| `myco-auth` | Credentials and access tokens: the store behind the OAuth2 password grant |
| `myco-machines` | The hands: `Harness`, `HostController`/`HostWorker` over NDJSON, and every `ToolService` (bash, editor, …) |
| `myco-prompts` | System-prompt fragments, the soul, the session stamp, the embedded manual |
| `myco-agent` | The turn loop: `Agent::interact`, `AgentEvent`/`EventSink`, cancellation, the compaction *worker* half |
| `myco-gui` | Minimal Yew client; one URL per conversation |
| `myco-test-support` | `ScriptedModel`, fixtures, the `MYCO_HOME` guard |

The module graph is acyclic and you should read it bottom-up, which is
what the stops below do. Each crate depends only on crates above it in
this ladder (runtime dependencies; check the `Cargo.toml`s yourself):

```
myco-core       —                       myco-api        —
myco-prompts    core                    myco-auth       core, api
myco-models     core, api, prompts
myco-config     core, api, prompts, models
myco-session    core, api, prompts, config
myco-machines   core, api, prompts, models, config, session
myco-agent      everything above except auth
myco (root)     everything, plus auth   myco-gui        api only (wasm)
```

Two structural facts to verify early, because everything else leans on
them: `myco-api` depends on nothing in the workspace and `myco-gui`
depends on `myco-api` alone — the browser deserializes the same types
the store persists. And `myco-agent` is the top pillar: nothing in
`crates/` depends on it. Its own module doc claims this; check it
against the `Cargo.toml`s.

---

## Stop 1 — `myco-core`: the primitives

Small enough to read completely. Three things live here because every
layer needs them and nothing here may depend on anything above.

Read: `lib.rs`, then `fs.rs` (private, re-exported), `image.rs`,
`external_command.rs`.

Notice:

- The doc comment on `CancelToken` in `lib.rs` defines *sticky*
  cancellation — a waiter that subscribes after the cancel still wakes.
  The tests below it are written as proofs of that sentence, including a
  100-iteration race loop. This is the contract every cancel path in the
  workspace assumes.
- `fs.rs`: `$MYCO_HOME` is the test seam — nothing anywhere may
  hardcode `~/.myco` — and v2 data under `<home>/v2` shares nothing with
  v1 by design; no migration is attempted.
- `image.rs` sniffs media type from bytes, never the extension, and caps
  on `base64_len(meta.len())` *before* reading the file. The test
  explains why file size is the wrong number: a 4 MiB file is already
  5.3 MiB on the wire.
- `external_command.rs` is the registry of every program myco spawns.
  Its doc claims the test `every_literal_spawn_goes_through_the_registry`
  enforces this — read the test and notice what it actually scans (see
  "Rough edges" below).

Slow down at: the empty-`PATH`-entry case in `find_in` (a POSIX trap:
an empty component must not mean cwd), and the fallback in
`ExternalCommand::resolve` that spawns the bare name so a missing
program fails with the OS's error, not a myco-invented one.

## Stop 2 — `myco-api`: the vocabulary

Read this crate before any consumer, because it is the noun list for
everything else: what a conversation *is*, what the wire carries, and
the one trait both frontends hold.

Read: `lib.rs` top to bottom, then `mention.rs` with its tests.

Notice:

- The section banner above the conversation types: they live here — not
  in the model layer — because `myco-session` stores them, the wire
  carries them, and `myco-models` only *projects* them onto providers.
  `Entry { author, at, body }` with `EntryBody::{User, Agent,
  ToolResults}` is the durable shape; one entry maps one-to-one onto a
  provider message.
- `Author` is intrinsic to the record ("a shared session is unreadable
  without it"), and the display name is denormalized so a transcript
  survives roster changes.
- `Content::Thinking` is stored for resume but stripped when backends
  compose the next request — the api-side half of an invariant the
  drivers enforce (Stop 4).
- `MycoApi` is *the* seam of the v2 architecture: one async trait,
  implemented by the in-process `Server` and by `client::HttpClient`,
  and identity is a property of the handle, not a parameter on each
  call. Keep this in mind for Stops 8–10.
- Display policy (`TOOL_DISPLAY_STRING_MAX`, `truncate_json_strings`,
  `tool_input_json`) lives in the wire crate on purpose: how a tool call
  is summarized is a property of the conversation, and the terminal and
  the web client must agree.
- `mention.rs` is 34 lines of char scanning that define the product's
  whole social contract, deliberately mechanical ("an explicit `@`
  prefix, never a guess about intent"). It lives here because the server
  gates agent turns on it and the client highlights from it — both sides
  need the same answer.

Slow down at: the test
`a_message_event_round_trips_with_its_entry_intact`, a test used as a
spec — its assertion documents that `entry.at` is the identity clients
dedupe on, so it must survive the wire unchanged. You will see the
payoff in the GUI's `merge`/`reconcile` (Stop 11).

## Stop 3 — `myco-session`: the document store

"Persistence only: how a conversation is stored, not how one is
produced." Compaction is split exactly along that line — the document
work is here; the agent run that writes the summary is Stop 7's
`run_compact_worker`.

Read: `lib.rs` (types, save/load, listing), then `lock.rs`,
`compact.rs`, `attach.rs`, `search.rs`, `console_log.rs`.

Notice:

- `SESSION_FILE_VERSION` and the two-stage load: a `VersionProbe` that
  deserializes only `version` before the full parse, so an old file is
  rejected for its version, not for a misleading "missing field" error.
  A rare case where error-message quality dictated code structure.
- `ActiveSession` is `Arc<Mutex<Session>>` — the shared live handle the
  runtime, the CLI, and the `session_meta` tool all mutate. Its
  `persist_entries` writes only on force/entry-count/usage change, and a
  `None` usage keeps the stored value rather than clearing it.
- `lock.rs`'s module doc is the whole design argued in prose: why the
  lock file is a separate inode (atomic rename would strand a lock on
  the old one), why readers are never blocked, why flock's
  process-death semantics mean there is no stale-lock recovery to get
  wrong — and why an *unavailable* lock degrades to unlocked instead of
  refusing to open the session.
- `compact.rs`: `select_tail` never ends mid tool loop — a trailing
  agent entry with unanswered tool calls is dropped, or the successor's
  first request would be malformed. This is one of three sites enforcing
  the same invariant (see "Themes").
- `link_compact_pair` is three lines of code under five lines of doc:
  the successor is saved before the predecessor points at it, because
  one crash order is recoverable and the other is not.
- `attach.rs`: the `@path` extension only selects the token; the media
  type comes from the bytes. The per-message cap is deliberately not
  per-model — it bounds a message, not an image.
- `search.rs` and `console_log.rs` both open with scope refusals worth
  reading ("myco is not in the search-engine business"; the console
  mirror's escape-freedom is *structural*, not filtered).

Slow down at: the golden-fixture test near the bottom of `lib.rs`
(fixture `session_v3_all_variants.json`). Its doc comment is the best
paragraph in the crate: the on-disk schema is spelled with Rust
identifiers, so renaming a variant compiles clean and silently makes
every stored session unreadable. The test pins byte-identical
re-serialization.

## Stop 4 — `myco-models`: the provider boundary

One trait, one method: `GenerativeModel::generate(&self, &[Message]) ->
AsyncStream<Result<MessagePart, GenerateError>>`. Every driver is
stateless; history is the caller's. The crate's mission statement is,
unusually, a comment on an import: read the note in `lib.rs` about why
`myco_api::Content` is imported but *not* re-exported — hiding the
projection seam once let provider concerns drift into a type the
browser deserializes.

Read: `lib.rs` (trait, catalog, `GenerateOutput`, errors), then
`driver_core.rs` and `sse_parser.rs`, then one driver end-to-end
(`anthropic.rs` is the richest), then skim the other two for their
deltas (`openai_common.rs`, `openai_responses.rs`,
`openai_completions.rs`).

Notice:

- `driver_core.rs` is the whole streaming architecture in ~80 lines:
  `spawn_generate` builds the request, measures the *exact serialized
  body* against `MAX_REQUEST_BYTES`, and streams through a backpressured
  channel; `drive_sse_stream` treats every failed send as "consumer
  dropped — stop reading", which is how dropping the stream cancels
  generation and billing. `SlotMap` translates each provider's unified
  index space into myco's separate content/tool-use spaces.
  `validate_finish` is the shared end-of-stream gate.
- `sse_parser.rs` buffers *bytes*, not text: network chunks split
  mid-UTF-8-sequence, and per-chunk conversion would corrupt streamed
  text and tool-input JSON with U+FFFD. The test feeds one byte at a
  time. The cheapest high-value read in the workspace.
- The `Recovery` taxonomy: failures that are a property of the
  *history* (a too-large request) cannot be fixed by retrying, because
  every later turn resends that history — the fix is
  `Recovery::OmitLastMessage`, consumed by the server's turn driver
  (Stop 8) via `Agent::rewind_last_user_turn`.
- Id-less tool calls are rejected loudly in all three drivers, with the
  same reasoning each time: such a call is poison that flows into
  persisted history and 400s on every later request, resume included.
- The empty-assistant-message guard exists in triplicate — search for
  "thinking-only turn" in each driver. Same production failure mode
  ("permanently wedging the session"), three dialects, three slightly
  different tests for "is this turn actually empty".
- Anthropic-specific shaping worth reading with the tests: cache
  breakpoints on the last two messages (the comment explains the
  20-block lookback that makes the arithmetic right), role merging with
  tool results *leading* the user turn they answer, and the auth header
  chosen by token shape.
- Dialect deltas that are all comment: `store: false` on Responses
  (myco is the stateful side), `max_completion_tokens` vs the deprecated
  name, `Effort::Max` clamped for non-Anthropic servers, and
  Completions reading to end-of-stream because usage arrives *after*
  `finish_reason`.

Slow down at: `GenerateOutput::from_stream_with_hook` — the mirror
image of the drivers, parts back into a finished turn, with sparse slots
so out-of-order indices are legal but missing ones are a
`MalformedResponseError`. The `on_part` hook is how the agent streams
deltas to the UI while accumulating the durable entry.

## Stop 5 — `myco-config`: resolve once, pass down

Startup resolution of everything impure: config file, environment,
roster, ssh aliases. The design is visible in one signature —
`resolve_with` takes every impure input (env, loaders, auth reader) as
a parameter, and `Config::resolve` is the only place the real ones are
named. That is why a 500-line test module exercises catalog resolution
and precedence with zero filesystem and zero env mutation.

Read: `lib.rs` (`resolve_with`, `resolve_catalog`,
`resolve_default_model`), `file.rs`, `harness.rs`, `roster.rs`.

Notice:

- Hard vs soft errors: shape problems fail startup; a credential
  *lookup* that fails is recorded per-entry as `auth_error` and
  surfaces only when the model is actually used. Configuring a model
  without its credential is fine until then.
- "Identity before anything else": the roster resolves before the
  catalog, and there is deliberately no fallback — inventing an
  identity from `$USER` would write a name nobody registered into a
  durable, shareable transcript. The roster holds no credentials;
  granting access is Stop 9's business.
- `file.rs`: scalar knobs parse as `Option` so unset stays
  distinguishable from explicitly-set, defaults land once at resolve
  time; the auth entry is a hand-rolled `TryFrom` because untagged
  serde's "did not match any variant" is useless in a config error;
  removed config sections are rejected with a migration pointer, not
  ignored.
- `harness.rs`: SSH details (user, port, identities, ProxyJump) stay in
  ssh_config where OpenSSH reads them natively; myco adds only
  `BatchMode=yes`, because the NDJSON pipe is not a TTY and OpenSSH
  must never prompt there. The ssh_config parser is deliberately tiny —
  wildcard and negated `Host` patterns are matching rules, not
  machines — with two independent termination guards (include depth cap;
  canonicalization as the cycle key).

Slow down at: the journey of `max_image_base64_bytes` — resolved once
in `lib.rs` for the model the process will run, handed to the harness,
and riding the remote spawn *argv* rather than the NDJSON handshake.
The comment in `harness.rs` explains why argv is sound: a worker serves
exactly one controller whose model is fixed at startup, and version
skew cannot strand the flag because connect already fails loud on skew.
Tests guard the seam from both ends.

## Stop 6 — `myco-machines`: the hands

Harness, host pair, and every tool. This is the biggest crate; read it
as three sub-tours.

**Harness** (`harness/mod.rs`, `ssh.rs`, `preflight.rs`): the host
pool. Local is always in-process (a remote literally named `local` is
rejected); remotes start unconnected and connect on first tool call.
Root-only services (`session_meta`, `session_history`, `subagent`) are
installed only on the in-process local worker and never receive the
injected routing `host` field — their schemas may use `host` to mean
something else. Read the test that pins the collision rule: a standard
tool declaring its own `host` would have the model's value silently
stripped, and the test's failure message tells you the remedy ("make it
root-only instead").

**Host pair** (`host/protocol.rs`, `host_worker.rs`,
`host_controller.rs`): read the protocol first — three requests
(`Hello`, `ToolCall`, `AgentFinished`), three responses, and notably
*no cancel message*: the worker mints a fresh `CancelToken` per call,
so a cancelled remote tool runs to completion and only the local waiter
is abandoned (`TODO.md` tracks this honestly). The worker's read loop
never blocks on tool work; the controller pipelines concurrent calls
over one pipe, demuxed by correlation id. Version skew is a *connect*
error, not a latent tool failure hours later — same-version lockstep is
what keeps the assumed tool catalog and the protocol sound.

**Tools** (`tool_services/`): the `ToolService` trait plus
`HostDispatchContext { agent_id, cancel }` — that pair is the entire
ambient context a tool gets. Read `tool_input_schema`'s long doc about
why schemars output is scrubbed (inlined `$defs`, no `default: null`,
`additionalProperties: false`): the schema is prompt engineering for
gateways and weaker models. Then:

- `bash_service/mod.rs`: each call is a *bounded interaction* against a
  live child — write optional stdin, collect until idle gap, timeout,
  byte cap, or exit. Sessions are owned per agent and reaped on agent
  finish. Three separate output caps, each with a why (grace for
  backgrounded grandchildren holding the pipe; head+tail capture because
  build tools print the root cause first and the summary last; a
  resident-memory cap for sessions nobody reads).
- `text_editor_service.rs`: mutations require a read-stamp — a content
  fingerprint (bytes, not mtime: same-granule external writes) recorded
  at view time and re-checked under one lock across check+mutate+record.
- One flat input schema instead of a tagged enum, because a root
  `oneOf` is rejected by the provider. The wire shape is a provider
  constraint, stated where it bites.

Slow down at three spots:

1. The `dead`-flag handshake in `host_controller.rs` (`submit`,
   `run_reader`, `run_writer`): three distinct races, each commented,
   converging on one pairing argument — the reader sets `dead` *before*
   draining pending waiters, and `submit` checks `dead` under the
   pending lock, so a new waiter is either rejected or already
   registered when the drain happens. Nobody awaits a reply that cannot
   come. The test makes a host that answers hello then exits fail fast
   *twice* (the respawn path), never hang.
2. `collect_output`'s generation counter in `bash_service`: the
   idle-gap heuristic is wrong whenever something happens that produces
   no bytes *yet* (a concurrent stdin write, a signal), so writers bump
   an `AtomicU64` and waiters reset their idle clock when it moves.
3. `kill_session_process`: four lines carrying a real safety argument —
   skip `kill(-pgid)` only when the leader exited *and* both pipes hit
   EOF, because signaling a fully-dead group could hit an unrelated
   process that recycled the pid. `eof_streams` exists purely to make
   this decision possible.

## Stop 7 — `myco-prompts`: what the model is told

Read: `lib.rs`, then `manual/mod.rs`; skim the fragments and the three
manual articles (`overview`, `api`, `harness-ops`).

The organizing pressure here is the prompt cache, and it is stated, not
implied:

- `epilogue_with` orders blocks least-to-most volatile, keeping the
  churniest block last so same-model forks share the longest cached
  prefix. There is a unit test asserting the block order — cache
  economics as a testable claim.
- `model_stamp` is identity-free by contract: any per-process value in
  the system prompt would change its bytes per agent and break fork
  cache reuse from the first byte. Its counterpart `session_stamp`
  rides a *message* instead, dated with creation time, not now ("a
  session open for days would otherwise carry a confidently wrong
  'now'; `date` is always right").
- The workspace listing shows ages in days rather than timestamps and
  sorts by path rather than recency — both choices made for cache
  stability, both documented.
- The manual is exported to `<home>/manual/<version>/<commit>/` so
  agents read it with `rg` instead of a dedicated tool; version and
  commit are *path components* so a rebuild lands in a fresh directory
  rather than editing one an older process is still pointing agents at.
  Export writes only when bytes differ, atomically, so a supervisor and
  its nested agents can start concurrently.

Slow down at: the pair of stamp docs read together — what may live in
the system prompt versus what must ride a message — and the test
asserting the session id does *not* appear in the epilogue.

## Stop 8 — `myco-agent`: the turn loop

The top pillar. Its module doc states the invariant everything else
rests on: whatever a turn does — end cleanly, error, get cancelled
mid-tool, or get truncated mid-tool-call by max_tokens — the transcript
left behind must be a prefix the provider will accept on the next
request.

Read: `lib.rs` (events, checkpoint contract, then `interact_entry`
line by line, then the cancel path), then `compact_worker.rs`.

Notice, in `interact_entry`:

- Tool dispatch is keyed on the *presence* of tool uses, not the stop
  reason — max_tokens can truncate a turn mid-call, and a tool_use
  nothing responds to makes the whole history unsendable.
- `join_all` preserves input order so `tool_results[i]` matches
  `tool_uses[i]`; events may interleave freely.
- A `ToolUse` stop with zero tool uses pushes the entry and fails loud:
  retrying unchanged history would loop generate forever, and an empty
  ToolResults message is rejected by the API.
- If cancel fired during tools, the turn stops *without* another
  generate — the transcript already has matching results for every
  call.
- `HistoryCheckpoint` fires only at well-formed boundaries (after the
  user push, after each ToolResults push) — never between an assistant
  tool_use and its results, because that prefix is exactly what a
  context fork must not inherit.
- `PendingInput` is the pre-emption seam, and it drains at exactly the
  same boundaries: messages that arrive mid-turn are folded into
  history right before the next generate, so the model reads them in
  the turn they interrupted. The doc explains why it is pull, not push
  (the agent owns its history exclusively while a turn runs), and why a
  cancelled turn leaves the queue untouched — nothing may be folded
  where nothing would answer it. The server's room inbox (Stop 9) is
  the production supplier.

The cancel path: `CANCEL_TOOL_GRACE` gives a cancelled dispatch two
seconds to clean up before a synthetic `cancelled` result is recorded.
The comment above the `select!` is the densest "why" in the crate: bash
must kill the whole process group (kill_on_drop SIGKILLs only the
leader, orphaning grandchildren), a mid-write session gets its taken
`ChildStdin` back, and for subprocess hosts abandoning only abandons
this waiter because the pipe demuxes by correlation id. Errors emit
`TurnFinished` too — sinks key their state resets off it.

`compact_worker.rs` is the agent half of compaction: a hidden
`SessionKind::Compact` session, a `NullEventSink`, one turn as
`Author::System`, then hand-off to Stop 3's document logic. Slow down
at `read_fresh_summary` and its test: `{id}.summary.md` can survive an
*earlier* compaction, so presence is not proof this run wrote it —
without the pre-read/compare, a stale summary describing a shorter
conversation would be folded in with no error anywhere.

Also read: `max_tokens_mid_tool_call_answers_the_dangling_tool_use` and
`checkpoint_fires_only_at_well_formed_boundaries` — the module-level
invariant made concrete, with the *next* turn's success asserted.

## Stop 9 — `src/server.rs`: the session runtime

The object that does the work. Everything above composes here: one
`Live` handle per resident session — an agent task, an unbounded `Cmd`
queue, a broadcast event feed, a fresh `CancelToken` per turn, the
session's write lock.

Read: `src/server.rs` top to bottom (it earns it), then
`src/subagent.rs`.

Notice:

- `Room` is the multiplayer design in one struct: who has posted
  (`participants`) and what has been accepted but not yet folded into
  history (`inbox`), under one lock. `post_message` decides whether a
  message wakes the agent, broadcasts it, and enqueues it in a single
  critical section — so the room rule reads the room as it is *now*,
  including messages still queued behind a running turn, never a stale
  disk snapshot. The rule itself (`Room::wakes_agent`, `Room::is_shared`)
  is `README.md`'s: explicit address, with one carve-out — a session
  nobody else has posted in is a private line. Agent and system entries
  do not make a room; only other people do. "An agent that guesses wrong
  talks over people; one that waits to be named never does."
- Pre-emption is the inbox's other half: the agent drains it at every
  well-formed boundary (Stop 8's `PendingInput`), so a message that
  names the agent mid-turn is folded into the turn in flight and
  answered by it, and a message between people lands in history where
  the room saw it — costing no turn, no busy flag, no turn events ("a
  client would read those as 'the agent is answering me'"). Whatever a
  turn does not fold, `drain_inbox` runs afterwards: wake messages as
  their own turns, notes as `run_note`.
- `SessionEvent::Message` is broadcast at *post* time, ahead of any
  folding, so watchers see people talking while the agent is mid-turn —
  and `compose` builds the entry once, at acceptance, so the record
  shown live and the record on disk are the same one (timestamps as
  identity; the GUI leans on this in Stop 11).
- `run_turn` owns a turn's lifecycle, and the wire's `TurnFinished` is
  its last act — sent only after `run_user_turn` persisted, and the
  agent's own earlier `TurnFinished` is filtered out of `events()` — so
  a client that refetches on it reads a transcript that already holds
  the answer. The test
  `the_wire_ends_a_turn_once_and_only_after_persistence` pins both
  halves.
- `ensure_live` is the boot sequence: load or accept a fresh session,
  acquire the write lock (Busy is a hard error naming the other
  process; Unavailable degrades to unlocked with a warning), attach a
  per-session harness with the root-only tools, wire the checkpoint,
  spawn the task. The comment about lazy durability — "a session
  becomes durable when it has content, not when it is opened" — is
  pinned by an integration test in Stop 12.
- `run_user_turn` is where `Recovery` lands: one retry for provider
  blips (the user turn is rewound and resubmitted), `OmitLastMessage`
  drops the poisoned message so the session can continue, and
  auto-compact queues when the context passes `AUTO_COMPACT_FRACTION`.
- `run_compact` swaps a live conversation to its successor in place:
  persist, run the worker, relock under the successor's id, swap the
  `ActiveSession` and agent history, re-key the live table. The v1
  `/compact` lifecycle, server-side.
- `UserApi` is `Server` bound to one caller; the `MycoApi` impl lives
  on it, so no route can reach the runtime without naming who is
  asking. The web adapter mints one per authenticated request; the CLI
  mints one for the roster's local user.

`src/subagent.rs`: nesting as a first-class tool — root-only, so it
works behind strict firewalls; one call runs one full turn of a hidden
child through the same `Server`, waits on the child's completed-turn
counter, and the parent's cancel token cancels the child on the way
down.

## Stop 10 — the door: auth, admin, web, client

Read: `crates/myco-auth/src/lib.rs` and `tests.rs`, then
`src/admin.rs`, `src/web.rs`, `src/client.rs`.

Notice:

- `myco-auth`'s module doc explains the grant choice: RFC 6749 §4.3
  because "the failure modes are known and the client side is boring."
  Durability is deliberately asymmetric — users snapshot to disk,
  tokens live only in memory, so a restart logs everyone out, "which is
  the behavior you want from a process that just changed underneath its
  clients."
- Passwords are PBKDF2 with a self-describing hash (work factor raisable
  without stranding old hashes); tokens are a plain SHA-256 digest, and
  the comment says why the asymmetry is right (high-entropy tokens have
  nothing to brute-force). Malformed hashes verify as `false` — "a
  corrupt record must not become an authentication bypass" — with a
  test enumerating eight corrupt shapes.
- `login` burns a real PBKDF2 verification against `dummy_hash()` for
  unknown users so response timing does not enumerate the user list;
  one error variant covers no-such-user, wrong-password, and
  no-password-set. Then find the documented exception (disabled
  accounts) and form your own view — see "Rough edges".
- The roster→auth link is made by the *binary*, not by either crate:
  `Server` construction reconciles roster names into the store, which
  is why `myco-auth` never needs to know `myco-config` exists.
- `src/admin.rs`: no self-service surface on purpose — everything that
  mints or revokes access happens from a shell on the machine, by
  someone who already has the box. Disabling keeps the history
  attributed: "a transcript records what happened, not who still has an
  account."
- `src/web.rs`: every route is a one-liner over `MycoApi`; the `Caller`
  request guard turns a bearer token into a `UserApi`, so there is no
  anonymous path through the module. The one concession — `?token=` on
  the SSE route, because `EventSource` cannot set headers — is called
  out in `README.md` as the weakest part of the surface, and the test
  suite checks it is a real check, not a way around one.
- `src/client.rs` is the other implementation of the trait: ~190 lines,
  `from_env` reading `$MYCO_API`/`$MYCO_API_TOKEN` as exported by
  `--mode serve`, so tools the agent spawns reach the API as the
  operator.

## Stop 11 — the frontends: cli + tui, gui

Read: `src/cli.rs`, then `src/tui/mod.rs`, `src/tui/transcript.rs`,
`src/tui/markdown/` (`mod.rs`, `tables.rs`, `links.rs`), then
`crates/myco-gui/` (`main.rs`, `auth.rs`, `highlight.rs`, `notify.rs`).

The CLI spawns a `Server` in-process, attaches one live session, and
drives it through the same queue and event feed the web server uses.
The prompt stays live (rustyline external printer); input queues while
a turn runs; Ctrl-C cancels the in-flight turn.

The TUI's architectural bet is one enum: everything the CLI shows is a
flat stream of `TuiEvent`s — content bytes, full-state `Style` (never a
delta, so an interrupted stream cannot leak styling), and link spans —
and sinks are dumb encoders. That makes the console mirror's
escape-freedom *structural*: `encode_plain(events)` equals
`strip_sgr(encode_ansi(events))` by construction, and there is a test
asserting exactly that equality. Read the two deliberate asymmetries in
the module doc (submitted input goes to the mirror only; history replay
to the terminal only) and notice that nested-worker events are filtered
structurally — the `EventSink` impl destructures `depth: 0`.

The streaming markdown renderer states two invariants at the top:
disabled styling is byte-identity (no delimiter dropped — piped stdout
and session files stay verbatim), and styled mode consumes delimiters
into presentation but every content byte still reaches output in order.
The machinery to slow down on is `word_marks`: presentation queued at
byte offsets inside the pending word, so styles and OSC 8 link opens
ride the wrap decision and never dangle across a break. Then
`emit_word_raw`'s one-line side effect — recording `max_word_width` —
which is the sole reason table column floors cannot disagree with
actual wrapping: measuring and wrapping share one driver
(`tables.rs`, `render_cell`). Tables are the one construct that needs
the whole block before emitting; the hold-back capture is gated on
styling so plain mode keeps the byte-identity guarantee, and a rejected
capture replays through the line machine newline for newline.

The GUI is deliberately the terminal's identity in a browser. The part
worth close reading is `state.rs`, the conversation's whole state
machine: one `ConvState`, one `apply`, every event a typed action. The
module doc explains the family of bugs this shape exists to make
impossible — a long-lived task holding a Yew `UseStateHandle` reads the
value from the render that created it, so the old SSE loop merged every
arrival into its *first* render's empty transcript and each incoming
message wiped the screen down to itself. Now the loops hold only a
dispatcher and never read state at all. Inside `apply`, the consistency
story survives intact as `merge` (identity is the timestamp — the
server composed the entry once, so the feed's copy and the poll's copy
are the same record), `reconcile` (the server's copy wins, but a
delivered-not-yet-persisted message must survive the poll), and
`Polled` standing down while the stream buffer is non-empty (the agent
checkpoints at every tool round, so a mid-turn poll would show the same
prose twice). Turn ends swap buffer for persisted entries in one step —
the wire's `TurnFinished` arrives after persistence (Stop 9), so one
fetch suffices. A tool call streams as a card keyed by the call id, its
`ToolFinished` result completes that card in place, and the saved entry
renders under the same key: running → complete → persisted is one
element updating, never a shuffle. The state machine is yew-free and
unit-tested on the host; the SSE loop owns reconnection, reloading the
transcript on every (re)connect so a dropped stream heals instead of
silently diverging. Tool cards call `api::tool_input_json` — the same
truncation policy the CLI applies, shared from the wire crate so the
frontends cannot drift.

## Stop 12 — tests as claims

`AGENTS.md` says test names should state invariants. The integration
suite delivers; read these as the codebase's own summary of what it
promises. The enabling trick is `myco-test-support`'s `ScriptedModel`:
it implements `GenerativeModel` by expanding a finished `GenerateOutput`
back into the streaming `MessagePart` sequence a real driver would
produce, and `Server::with_model_factory` accepts it — so the agent
loop, harness, tool dispatch, SSE publication, and persistence all run
for real; only the network hop is replaced. `TempHome` points
`$MYCO_HOME` at a temp dir and serializes mutation across tests.

- `tests/myco_api_roundtrip.rs` — the `MycoApi` contract exercised
  through `dyn MycoApi` only: streaming publishes deltas *before* the
  turn finishes; an abandoned session leaves nothing on disk; archiving
  hides without losing.
- `tests/web_auth.rs` — no route answers without a token; a *prefix* of
  a real token fails; writes are attributed to the token's owner, not
  the process operator; the roster says who exists, not who may sign
  in.
- `tests/web_multiplayer.rs` — the room rules over real HTTP with two
  token holders: the agent does not interject between people; naming it
  wakes it, and it can see what it overheard while quiet; a message
  reaches watchers as its own record whether or not the agent will
  answer. Then the pre-emption half, driven by a gated model that holds
  a turn provably mid-flight: a direct message folds into the turn in
  flight and is answered by it (one turn, not two), and the room rule
  counts messages still queued behind a turn — grace joining mid-turn
  makes ada's next unaddressed line a note, not a prompt.
- `tests/cancel_composed.rs` — the full Ctrl-C path leaves no process-
  group survivors and a well-formed history with a recorded cancelled
  result.
- `tests/host_cancel_desync.rs` — cancel or drop mid host call never
  desyncs the NDJSON pipe; sibling in-flight tools still complete;
  orphan replies are discarded.
- `tests/concurrent_host_tools.rs` — same-turn concurrent host tools
  genuinely overlap (each proves it *saw the sibling's marker*, which
  serial dispatch cannot produce).
- `tests/bash_session_integration.rs` — agent-level bash session claims
  (history shape, result correlation), with an explicit scope note
  pointing shell-behavior claims at `bash_service/tests.rs`.

Also worth a look: `.github/workflows/publish.yml` has a fallback
trigger — committing `.github/publish-request.json` on a `claude/*`
branch — for agent sessions that can push but cannot POST to the
dispatch API. The repo is a daily driver for agents, and its CI shows
it.

---

## Themes to trace

Once you have walked the stops, these threads run through many crates
at once; tracing each end-to-end is the closest reading this codebase
offers.

1. **Well-formed history.** The one invariant, enforced at three
   independent sites: the turn loop answers every tool_use regardless
   of stop reason (Stop 8), checkpoints fire only at provider-acceptable
   prefixes (Stop 8), and compaction's `select_tail` never cuts mid
   tool loop (Stop 3). The drivers add their own guards (no id-less
   tool calls, no empty assistant messages) so poison never enters the
   history in the first place (Stop 4).
2. **Attribution.** Roster (who exists, Stop 5) → auth store (what they
   know, Stop 10) → `Caller`/`UserApi` (who is asking, Stops 9–10) →
   `Entry.author` (who wrote it, Stop 2) → name-prefixing when history
   is projected for the model in shared sessions (Stop 4,
   `entries_to_messages`). At no point can an unregistered name enter a
   stored transcript, and the refusals are all loud.
3. **Cancellation.** Sticky token (Stop 1) → fresh token per turn
   (Stop 9) → grace window and synthetic results (Stop 8) → process-
   group kill and generation bumps (Stop 6) → the honest gap: the host
   protocol carries no cancel, and `TODO.md` says so.
4. **Bounded everything.** Every cap has a number and a paragraph:
   request bytes (Stop 4), image base64 (Stops 1, 5), message
   attachments (Stop 3), bash capture and session streams (Stop 6),
   soul and guidance bytes (Stop 7), event buffer (Stop 9). Read the
   paragraphs; each names the failure the cap prevents.
5. **Prompt-cache economics.** Identity-free system prompt, volatile-
   last block ordering, day-granularity listings, session stamps riding
   messages (Stop 7), cache breakpoints on the last two messages
   (Stop 4). Unusually, several of these are asserted in unit tests.
6. **Fail loud vs degrade, chosen per case.** Unreadable auth store:
   exit (an empty store anyone could be added to is worse). Unavailable
   flock: continue unlocked, with a warning (refusing to open would be
   worse than the risk). Version skew: connect error. Missing roster:
   startup error. Each choice carries its argument in a comment;
   collect them and you have the project's engineering ethic.
7. **One policy, two frontends.** Tool-call summarization and mention
   parsing live in the wire crate; the CLI's transcript layout and the
   GUI's blocks render the same entries; live echo and replay are kept
   byte-identical on the terminal. Whenever the two could drift, the
   shared thing was pushed down a layer.

## Rough edges

A close reading should also notice where the map and the territory
disagree. Known spots, worth reading rather than fixing reflexively:

- `myco-models/src/lib.rs` has no module doc; the crate's mission
  statement lives in the comment on its `myco_api` import.
- `crates/myco-session/src/console_log.rs` has intra-doc links to
  `crate::DEAD_tui::…` and `crate::history_events` that resolve to
  nothing in that crate — the real items live in the binary crate's
  `src/tui/`.
- `myco-auth`'s `PBKDF2_ITERATIONS` doc says it is lowered under
  `cfg(test)`; it is not — the suite gets its speed from
  `with_work_factor` instead. The safety argument still holds; the
  mechanism named is wrong. Relatedly, disabled accounts return a
  distinct error *before* any hashing — a deliberate, documented,
  tested exception to the anti-enumeration stance, and a genuine
  timing oracle.
- `myco-core`'s registry-enforcement test scans only its own crate's
  `src/`, not `myco-machines`, where the actual spawning happens. The
  invariant currently holds by discipline.
- `session_stamp` has exactly one production caller: compaction. The
  prompt fragments tell agents their session id is on the newest
  `# Session` block, which holds only for post-compaction sessions on
  this branch.

## Questions to test your reading

Answerable only from the code; each names its stop.

1. A user posts a message while the agent is three tool calls into
   someone else's turn. Trace exactly when every watcher sees it, when
   it lands on disk, and how the GUI avoids showing it twice or losing
   it on the next poll. (Stops 9, 11)
2. Why does dropping the stream returned by `generate` stop the
   provider from billing you, and which two functions make that true?
   (Stop 4)
3. `max_tokens` cuts off a turn halfway through emitting a tool call.
   What ends up in history, why does the *next* request not 400, and
   which test proves it? (Stop 8)
4. Two panes resume the same session. What exactly prevents the loser's
   turns from vanishing, why is the lock a separate file, and when does
   myco knowingly proceed without it? (Stops 3, 9)
5. Why must a bash exec's stdin be `Stdio::null()` on a remote host,
   and what would a child `read` actually consume if it were not?
   (Stop 6)
6. Compaction crashes between its two saves. Which order of
   `link_compact_pair` writes makes that recoverable, and what would
   the other order leave behind? (Stop 3)
7. What, byte for byte, changes in the system prompt between two agents
   forked from the same parent on the same model — and which tests pin
   the answer? (Stop 7)
8. A gateway streams a tool call with no id. Which three places could
   have papered over it, and why does each one choose to fail the turn
   instead? (Stop 4)
9. How does a signal sent to a quiet bash session produce a snapshot of
   the child's *reaction* rather than an empty buffer? (Stop 6)
10. `myco auth disable ada` runs while ada has a live token and a
    session mid-turn. What stops working, what keeps working, and which
    two mechanisms make the token dead? (Stop 10)
