# myco v3 — design

This branch is a from-scratch rebuild. v2 lives on `main-v2` and remains the
reference implementation; code crosses over only when it earns its keep (the
ledger near the bottom tracks every piece). This document is the contract
the rebuild is held to. It has survived one adversarial review round (three
independent reviewers: reducibility, concurrency, requirement-fit); the
findings that survived are folded in below and marked where they changed
the design.

## The model in one paragraph

A **workspace** is a pool of **instances** — a chat, a terminal, a browser
page, a cron table, a user's notification inbox — each an actor holding
canonical structured state. Every interaction with an instance is a
**verb**: reads are verbs, writes are verbs, and what would elsewhere be
"projections" are just differently-shaped read verbs (`screen` vs `tail`,
`screenshot` vs `a11y_tree`). **Principals** — humans, agents, system —
drive instances through one bus with one authorization rule, so the agent
is a peer, not a privileged channel. Panes in the client are projections of
instances; the workspace is the model being projected. Sessions as v2 knew
them dissolve: a "session" is a chat instance, and a subagent is a chat
instance parented under another.

## The layers, explained opaquely

Each layer is described only in terms of the contract of the layer below
it. If a sentence here needs to mention a neighbor's internals, the
boundary is wrong.

### L0 — runtime (`crates/runtime`)

**Contract offered:** `Cell<S>` — spawn state with a fallible builder that
receives `Signals` first; `call` a closure over `(&mut S, &Signals)` that
runs strictly after every earlier call and strictly before every later
one, may await, and returns its value to you; read a monotone `watermark`;
`changed(since)` — sleep until the watermark exceeds a value you name;
subscribe to an `Event` stream; `is_crashed` (eventually-true; a `CellGone`
from `call` is the definitive death signal). `Signals` — `bump`, `emit`,
`subscribe` — cloneable, usable from any task.

**How it works, opaquely:** you hand L0 state and closures; it hands back
ordering, wake-ups, and fault isolation. Nothing else. It does not know
what the state is, what the closures do, who is calling, or why. Two
delivery disciplines are exposed because observers need both and they are
mutually exclusive in one channel: the watermark **coalesces** (any burst
collapses to one wake carrying the latest value — lag is impossible by
construction) and events **do not** (history must arrive item by item, so
it is bounded and best-effort instead). A cell whose closure panics
becomes a marked corpse that refuses further commands and wakes its
watchers; it never hangs a caller and never leaks torn state (the state
drops with the task).

### L1 — instances (`crates/instance`)

**Contract offered (consuming only L0's):** register `Kind`s (a static
spec: named verbs with four schema flags, a version, two default-read
hints); `create` instances of them; `call(principal, id, verb, args)`;
`list`/`watermark`/`changed`; a global `(id, Event)` feed. Every instance
answers the `sys.*` verbs — `spec`, `meta`, `log`, `rename`, `take`,
`release`, `remove` — without its kind writing a line.

**How it works, opaquely:** L1 turns L0's "ordered closures over opaque
state" into "authorized verbs over named instances". Each instance is one
cell whose state is the kind's `Instance` object; dispatch wraps a verb
into a closure and authority into two checks — at enqueue (refuse ahead of
the queue) and again at apply (inside the ordering, so authority is bound
to effect: after `sys.take` returns, no verb the old driver had queued but
not yet applying will land). Identity, title, driver, and the verb log live *beside* the
cell, not inside it, so listing, introspection, seat transfer, and removal
work on a wedged or crashed instance — the rescue path never enters a
mailbox. Kind events are forwarded onto the global feed tagged with the
instance id; the forwarder subscribes before the kind's `create` can spawn
anything, so nothing an instance ever emits predates its subscription.
Re-entrant dispatch (a verb handler calling its own instance) is refused
by name, because in L0's contract it can only deadlock.

**What L1 adds that L0 lacks, and nothing more:** names, kinds,
principals, authority, uniform introspection, and one feed. Consistency,
waking, and fault isolation are consumed, not reimplemented.

### L2 — API server (M1)

**Contract offered (consuming only L1's):** authentication (ported from
v2: one-time codes, passkeys, operator), principal resolution
(token → `Principal`), then a generic gateway: `POST
/api/instances/{id}/verbs/{verb}`, `GET`/`POST /api/instances`, one
multiplexed watch/event stream (`/api/instances/{id}/changed` +
`/api/ws`), `GET /api/kinds` for capability discovery. **L2 is the human's adapter
to the bus exactly as the agent loop is the model's adapter** — both
translate an outside intelligence's native I/O into `Pool::call`. The
razor: anything in L2 that is not authentication or translation is in the
wrong layer.

### L3 — clients (M3)

**Contract offered (consuming only L2's):** a tree (projects →
instances), split-tree panes, and a renderer registry keyed by kind — each
renderer consumes its kind's `primary_render` verb payload and re-reads on
watermark advance. Clients hold layout (per project, per user, client-side)
and zero instance state.

**The client is a fold.** One store, one action stream, one reducer:
`State × Action → (State, Effects)`. Three sources feed one dispatch
queue — input surfaces (buttons, keybindings, the palette), the wire's
answers (verb results and refusals), and the watch stream (marks,
events, `gone`, `lagged`) — and the UI is a pure render of `State`.
Effects (HTTP, WS sends, focus, clipboard) run at the edge and re-enter
as actions. Nothing anywhere else holds state or contains logic; an
`onClick` is a dispatch.

The action vocabulary is small because the bus already is the API:

- **Nav** — pure client state: pane focus/split/close, tree selection,
  palette open/query. Handled synchronously.
- **`Call {instance, verb, args}`** — every wire read and mutation is
  this one shape, `sys.*` included. The answer re-enters as
  `Settled {result | VerbError}`, and the server's serde error is the
  payload *verbatim* — the client speaks the wire's refusal vocabulary
  (`not_driver` with `held_by` renders as a seat prompt with a take
  affordance, not a dead toast).
- **Feed** — each watch frame is an action; the feed doctrine's recovery
  (on `lagged`: re-list, re-read) is a reducer rule, not a special path.
- **Session** — login, logout, reconnect.

**The command registry is derived, not authored.** A command is
`{label, doc, enabled(State), action}`. Sources: (a) the hand-written
client commands (nav, layout, session) — the only authored list; (b) the
focused instance's verbs, generated from `GET /api/kinds` — name, doc,
and flags come from the spec, so a new verb on a kind appears in the
palette and on pane chrome with zero client code (the four-places razor,
applied to the client); (c) jump entries from the instance listing;
(d) operator commands when `whoami` says so. **A button is a palette
entry with coordinates**: pane-header buttons, context rows, keybindings,
and palette rows are four projections of one registry entry, and
`enabled(State)` is computed once — surfaces cannot disagree about
whether a verb is available or who holds the seat.

**The palette (`Cmd+P` / `Ctrl+P`)** is one fuzzy list over the whole
registry — jump and command together, grouped, because instances are the
client's files and verbs are its commands. Verbs without args execute on
enter; a verb needing args opens a second stage (a JSON well pre-filled
with `{}` in v1 — which makes the entire bus keyboard-reachable, the
debugging story in one gesture). The palette shows driver-gated entries
disabled with the holder named rather than hiding them: the palette
teaches the seat model. Interception note: browsers allow `Cmd+P` on
keydown-with-preventDefault (print stays reachable from the menu); a
tiny **reserved-chord set** (palette, pane-focus cycling) never reaches
a focused tty — every other key goes raw to the driven terminal, which
must lose only what the room absolutely needs.

The reducer shape is also DP-1 insurance: it is precisely iced's
architecture and runs unchanged under egui, so a native client reuses
the entire core — state, actions, registry, effects — and swaps only
the render layer. And the action log *is* the repro: a client bug
replays as a fold over recorded actions.

### The agent (M2) — beside L2, not inside any layer

The agent loop is a bus client: it holds standing subscriptions
((instance, cursored read, args, budget) refreshed where watermarks
moved), splices results into model context, and dispatches the model's
named tools (`bash`, `subagent`, …) to verbs. The model never sees a
generic `call` — unification is for the substrate, never the prompt.

One consequence of the fence-not-abort mailbox: a model *turn* is not a
mailbox command. It runs as a cancellable side-feed task the chat kind
owns, so a higher-priority verb — a user's interject, an explicit cancel
— can abort it mid-stream while the mailbox stays short and the fence
stays sharp. Cancellation is a kind-level concern; the runtime never
aborts a command.

The catalog already names the next moment: `ModelSpec.auto_compact_at_tokens`
is documented as "queue an auto-compaction when a turn ends" past that
many tokens. Nothing queues — `kind-chat` never reads the field. The
shape, when a store exists, is DP-3; do not implement it here.

## Concepts (the contract, post-review)

**Instance.** Identity (`id`, `kind`, `project`, `title`), canonical
state, a watermark bumped on **every** observable change — kind-state
writes, driver transfer, rename, and removal alike (review finding: meta
changes must wake meta watchers; for kind-state writes this is the kind's
contract to honor — the framework cannot verify a missing bump) — an event stream for significant
moments, and a bounded verb log (who called what; introspection reads are
not recorded, so watching the debugger does not erase the evidence).
Instances outlive any viewer; attachment is never ownership.

**Verb.** A typed command with four schema flags:
- `read_only` — pure; a promise to consumers (the framework cannot verify
  purity), never driver-gated (`register` asserts the combination away);
- `requires_driver` — enforced twice, enqueue and apply;
- `owner_only` — scoped to the instance's immutable creator; the privacy
  axis, orthogonal to the seat (a driver does not gain owned verbs; the
  owner keeps them without the seat). Added by review: the first per-user
  kind (a notification inbox) is unshippable without it;
- `cursored` — takes a cursor, returns the next; consumers read deltas.

Budgets are verb *parameters* (`tail`'s `max_bytes`; `sys.log`'s
`limit`) — the caller owns its budget because the caller knows its
economy. Every kind's `recommended_context` read must carry a canonical
plain-text field in its payload — the acme escape hatch, and the model's
native notation; the payload may carry more (tty's `text` also reports
`running`), but the text must be there.

**Principal.** `Human(id)`, `Agent(chat instance id)`, `System(name)`.

**Driver.** Not a mutex: durable, visible state, changed by
`sys.take`/`sys.release`, enforced by refusal. The policy, complete:
humans may take from agents or from nobody; **nobody takes an occupied
seat** except a human from an agent (not another human's seat, and System
never contends for an occupied one — agents do not wrestle each other
either; an *empty* seat is anyone's, which is how system automation will
drive unheld instances); release returns the seat to the creator, or
empties it when the creator releases. A take **fences**: every
verb the old driver had queued but not yet applied is refused at apply
time. A take does not interrupt the verb already mid-application — a
fence, not an abort.

**Removal.** `sys.remove`: humans and system may remove anything; an
agent only what it created or currently drives. Removal wakes watermark
watchers (their next call answers `unknown_instance`), emits `removed` on
the feed, and forgets the cell — child processes die with the dropped
state (`kill_on_drop`).

**Concurrency** is three mechanisms, none a client-visible lock:
1. *Consistency* — per-instance serialization: every verb, reads
   included, applies in mailbox order. Stated honestly (review finding):
   reads are never driver-gated and never *wait* on authority — the one
   read refusal is owner scoping, immediate at enqueue — but they
   **serialize with writes**; the claim is refusal-not-blocking, not
   parallel read execution. If a kind ever needs truly concurrent reads,
   the side-feed shape (state in an `Arc<Mutex>` beside the cell) already
   permits serving them off-mailbox — a future contract extension, not a
   present promise.
2. *Authority* — driver (mutable, fenced) and owner (immutable), per
   verb.
3. *Observation* — unlimited watchers on a coalescing watermark, plus
   reads.

**Events vs reads, and the feed doctrine.** Read verbs report current
state; events record what happened; a chat transcript stores only the
latter. The global feed is **best-effort by design**: bounded broadcast,
lossy under lag, not causally ordered across instances, with creation, or
with removal (a kind's own event can precede its `created` on a
multi-threaded runtime, and a side-feed's dying words — tty's `exited` —
can trail `removed`; consumers key existence off `list`, never off
`created`, and ignore events for ids they saw removed). The recovery rule for any
consumer that must not miss: treat the feed as a wake-up hint, and on lag
re-`list()` (which carries every watermark) and re-read cursored state.
Corollary, pinned as a convention: **any kind that emits
attention-shaped events must keep the underlying moments re-readable
behind a cursored read** — an event nothing can re-read is an event that
can be silently lost.

**Side-feeds.** A kind may mutate internal shared state from I/O tasks
and publish via `Signals` (commands serialize, streams flow). Two rules,
review-hardened: a side-feed whose input source does not die with the
instance (a pool event feed, unlike a pty) must be cancelled by the
instance's `Drop`; and side-feed tasks may call the pool freely, but verb
handlers must not call verbs on instances that may call back. The
framework's self-call refusal is task-scoped: a task *spawned inside a
handler* escapes it, so a handler must never await verb results from
tasks it spawns — hand such work to the side-feed pattern,
fire-and-forget. Cycles past one hop are likewise the kind-author's
contract to avoid.

**Kinds that consume the bus** (notifiers, future debuggers) hold a
`Pool` clone as a factory field — blessed dependency injection, wired at
startup.

## Irreducibility: what review tried to kill, and what saved it

Every piece below was attacked by name; these are the arguments that
survived. (Eight pieces did not survive and are gone: an internal duplicate
of `Signals`, a redundant constructor, a `default_driver` field that
permanently aliased `creator`, a third meta surface, hand-maintained error
names, a dead panic arm, public re-exports with no consumer, and removal
as a method instead of a verb.)

- **Two channels per cell.** Merging events into the watermark loses
  history (watch keeps only the latest value); merging the watermark into
  events makes a byte-storming pty lag every subscriber (broadcast cannot
  coalesce). One channel must coalesce, the other must not: the
  transport-level image of "reads answer what is, events what happened".
- **The mailbox itself** (vs. state-behind-a-lock). Three properties fall
  without it: fault isolation (a panicking handler poisons a lock but
  only kills a task), await-capable handlers without holding locks across
  await, and a single dispatch point where authority, logging, and
  re-entrancy refusal happen once instead of per-kind.
- **Entry beside the cell** (meta outside the mailbox). Fold it in and:
  `list` enters N mailboxes, so one wedged instance freezes the tree;
  driver refusal happens *after* queueing — refusal becomes waiting,
  which the contract forbids; and the rescue path dies — today a human
  can `sys.meta` a wedged terminal, `sys.take` the seat, and `sys.remove`
  it without ever touching the mailbox. The one duplication this split
  had grown (`default_driver` ≡ `creator`) was found and deleted; what
  remains holds no field on both sides.
- **The two-phase driver check.** Enqueue-only is provably
  non-linearizable (review: a "stopped" agent's queued keystrokes land
  after a human seizes the seat — the emergency stop that doesn't stop);
  apply-only makes refusal a wait. Both, in that order, are the minimum
  that delivers refuse-ahead-of-queue *and* authority-bound-to-effect.
- **The side-feed path.** Route pty bytes through the mailbox and the
  byte stream competes with keystrokes for a 64-slot queue of boxed
  closures — input latency coupled to output throughput. `Signals` is
  the narrowest possible side door: publish-only, no state access.
- **The per-entry verb log.** Derive it from the event feed and it dies
  three ways: the feed is live, not storage (a late debugger sees
  nothing); a global buffer is unbounded or unfair (one chatty instance
  evicts everyone); and refusals are born at the dispatch point, pre-
  mailbox — only the dispatcher can record them, and "the log records
  refusals" is what makes it a debugger.
- **The forwarder task per instance.** Tag events at the source instead
  and L0 must learn instance ids — the layer whose value is knowing
  nothing. Subscribe-per-instance instead and every consumer re-implements
  create-race handling. One task per instance, subscribed before birth
  completes, is the toll the boundary charges; it is paid in exactly one
  place.
- **`requires_driver` as framework schema** (vs. in-kind checks). Every
  kind re-implementing seat policy is the bug class `sys.*` exists to
  prevent, and a kind-side check runs post-queue — blocking again. One
  bool read twice is the whole cost.
- **The KindSpec hints.** Without `recommended_context`, M2's dispatcher
  hardcodes per-kind read choices — the exact kind-knowledge leak L1
  exists to prevent. Two static strings, validated at registration, are
  the cheapest insurance in the codebase.
- **What is knowingly *not* minimal** — schema fields whose consumers
  are one milestone away (`cursored` → M2 subscriptions, `version` → M4
  protocol providers, `read_only` → speculative-read clients). Cutting
  fields that wire consumers re-add next milestone is churn wearing
  minimalism's clothes; each names its consumer in its doc comment, and
  the ledger holds the receipt.

## Worked requirement: push notifications

The test the design was given: add push notifications, prove
decoupledness. Result — **a kind, not a feature**, and the exercise
changed the framework in exactly two ways (the `owner_only` flag; a
lag-survivable forwarder), both defects it exposed rather than mechanisms
it demanded.

The `notifier` kind, one instance per human, created by L2 *as that
human* at first login (find-or-create keyed on `kind + creator`;
provisioning-as-the-user is principal resolution doing its job):

- **State:** pending attention items (absolute sequence numbers, the
  tty-scrollback idiom), mute rules, web-push subscriptions, per-source
  cursors.
- **Verbs:** `pending` (cursored, owner-only — attention is private),
  `text` (owner-only plain-text projection; doubles as the agent's
  window into what its human has not seen), `ack`, `mute`, `register` /
  `unregister` (owned writes; push endpoints are write-only state no
  read returns).
- **Ingest:** a side-feed consuming `Pool::events()` — the first
  side-feed whose input outlives its instance, hence the Drop-cancel
  rule. Routing is the pinned attention envelope
  (`myco_instance::events::Attention` — `for`, `title`, `body`): kinds
  emit facts, the notifier turns facts addressed to its owner into
  items, bumps its watermark (badge watchers re-read `pending`; the
  *watermark* is not the badge — acks bump too; `unacked` in the payload
  is), and hands delivery to a queue.
- **Delivery:** a second side-feed POSTing web-push (VAPID keys live in
  the factory struct, invisible to Value-land), backing off on 5xx,
  dropping dead endpoints.
- **Loss:** the feed is a hint, not the ledger. On `Lagged`, reconcile:
  re-`list()`, compare watermarks to per-source cursors, re-read the
  cursored attention reads of whatever moved. "You were mentioned" must
  not vanish because a chat burst-emitted — and with the reconcile rule
  it cannot, because mentions are re-readable in the transcript.

Why not an L2 feature: pending/ack/mute/subscriptions are state with
reads and writes — the definition of an instance. In L2 it would resurrect
per-op routes and bespoke storage (the `MycoApi` disease the ledger
executed), be invisible to every principal but the browser (breaking the
peer symmetry: the agent could not see its human's unread pile), and
forfeit the free panes/badge/audit that fall out of being an instance.
What stays in L2 regardless: identity, and the find-or-create trigger.

The symmetry worth keeping: **wake-the-human (notifier) and
wake-the-agent (standing subscriptions) are the same shape** — a
per-principal reducer over events and cursored reads. One mechanism,
two consumers, no new concepts.

## sys.* — the uniform verbs every instance answers

`sys.spec` (kind schema, versioned), `sys.meta` (identity, driver,
watermark, crashed), `sys.log` (recent verb calls; `limit`),
`sys.rename`, `sys.take` / `sys.release` (the seat), `sys.remove`.
Framework-answered so kinds cannot get them wrong; entry-level so they
work on wedged and crashed instances. Debugging doctrine: the runtime
eventually exports *itself* as read-only kinds (actor table, bus traffic,
an agent's context assembly as an inspectable instance — "watch what the
model saw"). The debugger is the product.

## Prior art, and what each contributes

- **Erlang/OTP** — the behavior interface, supervision as the lifetime
  answer, the registry. L0 is gen_server minus what we don't need.
- **tmux** — pool ≠ attachment; layouts as serializable split trees;
  control mode proves one machine channel serves human GUIs.
- **Emacs** — buffer/mode/window ≈ instance/kind/pane; comint is
  chat≈tty; its single-threaded core is the failure mode
  actor-per-instance avoids.
- **acme / Plan 9** — uniform verbs breed composability; per-window files
  are reads-as-verbs; the text escape hatch. The trap (text as the *only*
  notation) is fixed by typed payloads.
- **JetBrains MPS** — the workspace is projectional: canonical structure,
  many projections, gestures routed back to the model. Its trap
  (projection lock-in) is fixed by the mandatory plain-text field.
- **Kubernetes** — kinds + uniform verbs + watch-with-versions (our
  watermark); subresources are reads-as-verbs at scale; ownerReferences +
  cascading GC is the template for lifetime once projects arrive.
- **Jupyter** — kernels survive clients; MIME bundles are multi-consumer
  projections (the model is just another consumer); hidden-state is the
  sin our events/reads split exists to avoid.
- **VSCode/LSP, Zellij** — process-isolate plugins; negotiate
  capabilities; version the schemas (now a field, not a vow).

## Deferred problems

Problems examined, decided against solving now, and recorded so the
revisit starts from the analysis instead of from scratch. Each names its
trigger, its path, and the cheap insurance adopted immediately so the
deferral forecloses nothing.

### DP-1 — Browser-client performance ceiling (Zed/Sublime feel)

**Deferred until** the GUI demonstrably feels slow, or a
latency-sensitive pane (map editor) arrives. **Verdict when examined:**
perceptually Zed-class is reachable in a browser for myco's hot surfaces,
because the heavy machinery (vt100 parsing, transcripts, rendering to
styled runs) is server-side native Rust — the client draws small
projections. What decomposes and where the browser taxes it:

- *Keypress→photon*: native ~8–16ms; browser floor keeps ~1 frame of
  event-loop/compositor tax; Chromium's `desynchronized: true` canvas
  claws much back → ~15–25ms achievable. Perception threshold for typing
  is ~40–70ms (sensitive users ~20ms): under it, but instrumented
  shootouts lose to Zed forever.
- *Consistency* (the larger half of "feel"): a Rust/wasm core has no
  GC'd hot path; repaint only on watermark-or-input. No jank sources.
- *Throughput*: solved in public — xterm.js's WebGL glyph-atlas renderer
  (VS Code's terminal) is the reference; Figma the broader proof.
- *Startup*: not matchable (tab boot + wasm fetch); a workspace lives in
  a pinned tab; accepted.

**The path, when triggered:** (1) DOM for chrome (tree, chat —
virtualized; selection/IME/a11y free), GPU canvas for hot panes, and the
framework never owns the hot path (v2's flicker saga, named and fenced);
(2) damage-delta screen payloads — dirty rows keyed to the watermark;
(3) binary framing for hot payloads so the 60Hz path allocates nothing;
(4) predictive local echo for remote typing (the mosh trick; loopback
does not need it); (5) hot renderers written on **wgpu**, which compiles
to WebGPU-in-wasm today and native tomorrow — a native shell later
re-hosts the same pane renderers and swaps DOM chrome for egui: a
re-shelling, not a rewrite. (HWRT is the one thing a tab will never do;
the wgpu clause is what keeps that exit open.)

**Adopted now regardless (cheap):** serve COOP/COEP headers from M1 (two
headers; unlocks wasm threads/SharedArrayBuffer whenever wanted);
renderer crates framework-free (payload in, pixels out — no GUI-framework
types in their signatures); screen payloads shaped so row-level deltas
can be added without breaking consumers (rows are addressed already).

### DP-2 — QUIC / WebTransport instead of HTTP(S)+WebSocket

**Decision:** HTTP(S) + WebSocket is the baseline wire; QUIC enters
later as an upgrade, not a replacement. Facts that decided it:

- Browsers expose QUIC only two ways: HTTP/3 (transparent transport for
  ordinary fetches — a server config detail, no app change) and
  **WebTransport** (streams + unreliable datagrams over HTTP/3;
  Chromium and Firefox ship it, Safari has lagged — verify before
  relying). Raw QUIC sockets do not exist for pages; WebSocket-over-h3
  (RFC 9220) is not dependably implemented. And the page shell always
  loads over HTTP(S) — "instead of" can never be total.
- **The tunnel collision:** the supported remote pattern is an SSH
  tunnel, which forwards TCP only. WebTransport's UDP cannot ride it, so
  WebSocket must remain a first-class fallback under the current
  transport doctrine no matter what. QUIC's actual wins (loss recovery,
  no head-of-line blocking, 0-RTT, migration) are real-network wins;
  loopback has no loss to recover from.
- Certificates: browser QUIC requires TLS even on localhost. Workable —
  WebTransport's `serverCertificateHashes` pins a self-signed cert
  (ECDSA, ≤14-day validity, so the server mints at startup and the page
  fetches the current hash over its HTTP origin) — but it is ceremony
  that buys nothing on loopback.

**Upgrade triggers:** M4 media flows crossing real networks (lossy
frames want datagrams, latest-wins) — WebTransport with WS fallback; or
a native client (the same quinn endpoint serves WebTransport and native
QUIC alike — one server, both worlds, still no WebRTC).

**Adopted now regardless (cheap):** transport-agnostic framing. The
protocol is logical channels — one control (verbs), one event/watch
stream, N media flows — and the carrier maps them: WS multiplexes all of
them today; WebTransport assigns streams and datagram flows later. The
verb envelope never learns what carried it.

### DP-3 — Compaction (successor chat, not in-place)

**Deferred until** chats persist. The catalog already resolves
`auto_compact_at` (default 0.85 of `context_window`) to
`ModelSpec.auto_compact_at_tokens`; the turn-engine comment on that field
is the queue that does not exist. No `select_tail`, no worker, no
successor. The pool is RAM-only — restart drops every transcript — so
an in-place rewrite would only shorten a chat that still dies on exit.

v2 (`main-v2`) mints a successor session. A hidden `SessionKind::Compact`
worker writes markdown; `select_tail` keeps the last **2** well-formed
user turns and never ends mid-tool-loop (tool bodies capped 4_000
chars); the successor is seeded with `# Compaction resume`, the
predecessor id, and the summary path; `link_compact_pair` writes the
successor first so a crash cannot leave the predecessor pointing at a
missing file. Title, parent, and kind copy across; the predecessor stays
readable on disk.

grok-build 1.0.0 compresses *in place*. The full unsummarized transcript
stays a file the model is told to `read_file`/`grep` ("Do NOT modify
these files"); prior summaries are authoritative and must be merged
forward (`<conversation_summary>` / "this session is being continued").
Optional two-pass (`two_pass_compaction`, default false), `/compact
[context]`, `/flush` to memory first, Pre/PostCompact hooks, degenerate-
summary retry, `compaction_checkpoints/` for auto. That mutate is the
wrong shape until a store exists.

**When triggered:** a transcript that outlives the process. Then v2's
successor *instance* (new chat, same parent / project / title; old chat
stays readable) plus grok's "full transcript is a file" once that file
exists. Threshold 0.85 can stay. No new crate; no worker on this
branch.

**Adopted now regardless (cheap):** the catalog field and its comment.
The missing piece is the queue, not the threshold.

## The ledger

Functionality crosses from v2 only with a verdict recorded here.

| v2 piece | verdict | notes |
|---|---|---|
| `pty.rs` (openpty, AsyncFd halves, TIOCSWINSZ) | **ported (M0)** | verbatim into `kind-tty` |
| vt100 screen → styled runs renderer | **ported (M0)** | minus the embedded plain-text copy (now the `text` verb) |
| shell scrollback + `tail(from)` | **re-derived (M0)** | cursored + budgeted (`max_bytes`) |
| shell/subagent locks | **re-derived (M0)** | driver per-verb, apply-time fenced; + `owner_only` axis |
| auth (codes, passkeys, tokens, admin routes, operator) | **ported (M1)** | v2 got this right; landed in L2 nearly verbatim |
| SSE + shell WebSocket | **drop** | one multiplexed event/watch stream (M1) |
| `MycoApi` trait + `HttpClient` + per-op routes | **drop** | generic verb gateway; four parallel op lists become one |
| agent loop, models/providers, session store | **port + reshape (M2)** | the chat kind + the model-side adapter (dispatcher, standing subscriptions) |
| session compaction (successor + compact worker) | **pending** | threshold on `ModelSpec`; queue does not. Successor instance, not in-place, and only after persistence. DP-3. |
| `subagent` tool + child routes | **drop** | chat instances with a parent ref |
| NDJSON host protocol | **re-derive (M4)** | the bus envelope over stdio; hosts/toolds are protocol providers |
| GUI terminal renderer, transcript renderer | **port (M3)** | into the renderer registry |
| GUI browser/draft/conversation pages | **drop** | tree + panes replace page navigation |
| observer notes machinery | **drop** | standing subscriptions |
| notifications (v2 never had them) | **new: notifier kind (M2/M4)** | see the worked requirement |
| command palette (v2 never had one) | **new: `Cmd+P` over the registry (M3)** | entries derived from kind specs; one reducer, buttons emit the same actions — see L3 |
| piped (non-pty) bash mode, signals, screenshot action | **pending** | port into kind-tty when the agent arrives (M2) |
| README/TOUR | **rewrite as they become true** | README tracks the current milestone tip |

## Milestones

- **M0 (done, reviewed)** — `runtime`, `instance`, `kind-tty`; tests pin
  serialization, the apply-time fence, re-entrancy refusal, owner
  scoping, watermark semantics including removal, crash containment, and
  a live pty driven end-to-end through the bus.
- **M1 (done)** — L2: auth port, generic verb gateway, one event stream,
  capability discovery; `myco.py` rewritten thin.
- **M2** — chat kind (agent loop + providers ported), named-tool
  dispatcher, standing subscriptions, notifier kind; user-created chats
  and chat-parenting (subagents) fall out.
- **M3** — GUI: the single-reducer core (one action stream; see L3),
  tree sidebar, split-tree panes, renderer registry (tty + chat
  renderers ported), and the `Cmd+P` palette over the derived command
  registry. The visual contract is **STYLE.md** (approved "amethyst"
  direction: islands on violet-biased paper, presence/seat/ember
  vocabulary, theme-constant terminal material).
- **M4** — protocol providers (toolds/hosts as bus-over-stdio); cron
  kind; browser kind (`a11y_tree` + `screenshot`, computer-use verb
  vocabulary); web-push delivery for the notifier.
- **M5** — self-describing debugging kinds; agent-to-agent messaging
  behind explicit grants with loop budgets.

Parity with v2 is audited against the ledger before `main-v3` becomes the
default.
