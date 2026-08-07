# myco v3 — design

This branch is a from-scratch rebuild. v2 lives on `main-v2` and remains the
reference implementation; code crosses over only when it earns its keep (the
ledger at the bottom tracks every piece). This document is the contract the
rebuild is held to.

## The model in one paragraph

A **workspace** is a pool of **instances** — a chat, a terminal, a browser
page, a cron table — each an actor holding canonical structured state.
Every interaction with an instance is a **verb**: reads are verbs, writes
are verbs, and what used to be "projections" are just differently-shaped
read verbs (`screen` vs `tail`, `screenshot` vs `a11y_tree`). **Principals**
— humans, agents, system — drive instances through one bus with one
authorization rule, so the agent is a peer, not a privileged channel. Panes
in the client are projections of instances; the workspace is the model
being projected. Sessions as v2 knew them dissolve: a "session" is a chat
instance, and a subagent is a chat instance parented under another.

## Concepts

**Instance.** Identity (`id`, `kind`, `project`, `title`), canonical state,
a monotonically increasing **watermark** bumped on every observable change,
an **event** stream for significant moments (exited, navigated, turn
finished), and a bounded **verb log** (who called what — acme's `event`
file, reborn). Instances outlive any viewer; attachment is never ownership.

**Kind.** The type of an instance: a verb vocabulary with per-verb flags
(`read_only`, `requires_driver`, `cursored`), a `create` recipe, and two
hints — `primary_render` (which read the default pane renderer consumes)
and `recommended_context` (which read an agent's context assembly should
prefer, with default arguments). Kind schemas are versioned, typed, and the
smell test is: a kind wanting one more verb had better argue for it.

**Verb.** A typed command. Reads are pure and always concurrent; cursored
reads take a cursor (`from`, `since`) and return the next one, so
consumers read deltas, not worlds. Budgets are verb *parameters*
(`max_bytes`, `max_entries`) — the caller owns its budget, because the
caller knows its economy (a GUI paginating, an agent guarding context).
Every kind must include at least one plain-text read — the acme escape
hatch that keeps all state greppable and pipeable, and, not by
coincidence, usually the read an LLM wants.

**Principal.** `Human(id)`, `Agent(chat instance id)`, `System(name)`. An
agent is named by the chat it drives. Both adapters translate an outside
intelligence's native I/O into bus verbs: the API server adapts HTTP for
humans exactly as the agent loop adapts model tool-calls — the design's
central symmetry.

**Driver.** Not a lock. Some verbs on some kinds are single-driver
(`input`, `resize`, `click`); the driver is durable, visible state on the
instance, changed by `sys.take` / `sys.release`, enforced by *refusal* —
nobody acquires, waits, or deadlocks. Policy: humans may take from agents
or from nobody; an agent may never take from a human; releasing returns
the instance to its default driver (its creator), or to nobody if the
default driver releases. Reads never require the driver.

**Concurrency** is three orthogonal mechanisms, none a mutex in the
client-visible sense:
1. *Consistency* — each instance is an actor; verbs apply serially.
2. *Authority* — the driver bit, per-verb, as above.
3. *Observation* — unlimited concurrent watchers via watermark + reads.

High-throughput kinds (a pty pumping bytes) may feed internal state from
an I/O task outside the mailbox, publishing through the cell's signals
(bump/emit); the mailbox still serializes all *verbs*. This is the
sanctioned side-feed pattern, not a loophole: commands serialize, streams
flow.

**Observation is agent-side policy, not kind-side machinery.** An agent's
situational awareness is a set of standing subscriptions — (instance, read
verb, args, budget) — refreshed at turn boundaries only where the
watermark moved, then spliced into context. Kinds publish reads; drivers
compose awareness. v2's shell "observer notes" dissolve into this.

**Events vs reads.** Read verbs report *current state*; events record
*what happened*; the chat transcript stores only the latter. (Jupyter's
hidden-state problem is the cautionary tale: never let a transcript imply
state it doesn't have.)

## Layers

```
L0  runtime    crates/runtime   actor cells: mailbox, serialized async verb
                                application, watermark/watch, events, panic
                                supervision. Knows nothing of kinds.
L1  instances  crates/instance  Kind/Instance traits, VerbSpec, Principal,
                                driver policy, sys.* verbs, authorization,
                                Pool (registry, projects, global events).
                                Providers: in-process kinds now; protocol
                                kinds (toolds/remote hosts) later — the wire
                                format is the bus envelope serialized, so
                                in/out-of-process is a transport choice.
L2  api server (later)          auth (ported from v2), principal resolution,
                                one generic verb gateway, one event stream,
                                capability discovery. Anything in L2 that is
                                not authentication or translation is in the
                                wrong layer.
L3  clients    (later)          GUI: tree (projects → instances), split-tree
                                panes (not a free grid), renderer registry
                                keyed by kind; myco.py; -p CLI.
```

The model-facing surface keeps *named tools* (`bash`, `subagent`, …): the
dispatcher maps them to kinds internally. Unification is for the
substrate, never the prompt — meta-tools hurt model ergonomics.

## sys.* — the uniform verbs every instance answers

`sys.spec` (kind schema), `sys.meta` (identity, driver, watermark),
`sys.log` (recent verb calls), `sys.take` / `sys.release` (driver
transfer). Handled by the framework so kinds cannot get them wrong.
Debugging doctrine: the runtime eventually exports *itself* as read-only
kinds (actor table, bus traffic, an agent's context assembly as an
inspectable instance — "watch what the model saw"). The debugger is the
product.

## Prior art, and what each contributes

- **Erlang/OTP** — the five-op behavior interface, supervision as the
  lifetime answer, the registry. L0 is gen_server minus the parts we
  don't need.
- **tmux** — pool ≠ attachment; layouts as serializable split trees;
  control mode proves one machine channel can serve human GUIs.
- **Emacs** — buffer/mode/window ≈ instance/kind/pane, forty years
  proven; comint is chat≈tty; its single-threaded core is the failure
  mode our actor-per-instance avoids.
- **acme / Plan 9** — uniform verbs breed composability; per-window
  files are reads-as-verbs; the text escape hatch. The trap (text as the
  *only* notation) is fixed by typed payloads.
- **JetBrains MPS** — the workspace is projectional: canonical structure,
  many projections, gestures routed back to the model. Its trap
  (projection lock-in) is fixed by the mandatory plain-text read.
- **Kubernetes** — kinds + uniform verbs + `watch` with versions
  (independently our watermark); subresources are reads-as-verbs at
  scale; ownerReferences + cascading GC is the template for instance
  lifetime once projects arrive.
- **Jupyter** — kernels survive clients; MIME bundles are
  multi-consumer projections (the model is just another consumer);
  hidden-state is the sin.
- **VSCode/LSP, Zellij** — process-isolate plugins; negotiate
  capabilities; version the schemas from day one.

## The ledger

Functionality crosses from v2 only with a verdict recorded here.

| v2 piece | verdict | notes |
|---|---|---|
| `pty.rs` (openpty, AsyncFd halves, TIOCSWINSZ) | **ported (M0)** | verbatim into `kind-tty` |
| vt100 screen → styled runs renderer | **ported (M0)** | one renderer served agent/REST/WS in v2; same shape here |
| shell scrollback + `tail(from)` | **re-derived (M0)** | cursored read verb |
| shell/subagent locks | **re-derived (M0)** | driver, per-verb |
| auth (codes, passkeys, tokens, admin routes, operator) | **port (M1)** | v2 got this right; lands in L2 nearly verbatim |
| SSE + shell WebSocket | **drop** | one multiplexed event/watch stream (M1) |
| `MycoApi` trait + `HttpClient` + per-op routes | **drop** | generic verb gateway; four parallel op lists become one |
| agent loop, models/providers, session store | **port + reshape (M2)** | becomes the chat kind + the model-side adapter (dispatcher, subscriptions) |
| `subagent` tool + child routes | **drop** | chat instances with a parent ref |
| NDJSON host protocol | **re-derive (M4)** | becomes the bus envelope over stdio; hosts/toolds are protocol providers |
| GUI terminal renderer, transcript renderer | **port (M3)** | into the renderer registry |
| GUI browser/draft/conversation pages | **drop** | tree + panes replace page navigation |
| observer notes machinery | **drop** | standing subscriptions |
| piped (non-pty) bash mode, signals, screenshot action | **pending** | port into kind-tty when the agent arrives (M2) |
| README/TOUR | **rewrite as they become true** | |

## Milestones

- **M0 (this commit)** — `runtime`, `instance`, `kind-tty`; tests prove
  serialization, watermarks, driver policy, sys verbs, and a live pty
  driven end-to-end through the bus.
- **M1** — L2: auth port, generic verb gateway, one event stream,
  capability discovery; `myco.py` rewritten thin.
- **M2** — chat kind (agent loop + providers ported), named-tool
  dispatcher, standing subscriptions; user-created chats and
  chat-parenting (subagents) fall out.
- **M3** — GUI: tree sidebar, split-tree panes, renderer registry (tty +
  chat renderers ported).
- **M4** — protocol providers (toolds/hosts as bus-over-stdio); cron
  kind; browser kind (`a11y_tree` + `screenshot`, computer-use verb
  vocabulary).
- **M5** — self-describing debugging kinds; agent-to-agent messaging
  behind explicit grants with loop budgets.

Parity with v2 is audited against the ledger before `main-v3` becomes the
default.
