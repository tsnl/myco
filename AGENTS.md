# AGENTS.md

Guidance for humans and coding agents working on **myco**.

## Premise

**myco** is a multi-host coding agent: one **server** (`myco --mode serve`)
runs sessions (model + harness + conversation) and drives tools on an
always-on **local** host (in-process) and on optional **remote** hosts
(`ssh … myco --mode host` over NDJSON). Everything else is a client of its
REST API: the Yew web GUI, `myco -p`, `clients/myco.py`, and the agent's own
nested tools.

Primary goal: a **personal daily driver** that can replace Claude Code / Codex /
OpenCode-class workflows — long sessions you trust, real computer use, one
conversation across machines, and shared sessions where people and the agent
work the same shells. Multi-host execution and nested-agent orchestration (the
`subagent` tool) are the product wedge; feature work must not outrank session
trust and long-session viability (`TODO.md`).

This is **not** an educational textbook repo (unlike resin/unit). Prefer clarity
and minimalism, but optimize for a tool people run every day, not for teaching
how agents work.

## Ranking

When goals conflict, rank them:

1. **Correctness & session integrity** — well-formed history, cancel behavior,
   no silent corruption on long runs.
2. **Simplicity** — minimum code that solves the real problem.
3. **Operability** — agents and humans can diagnose hosts, config, and failures
   (clear errors that name the config path, host, or session).
4. **Features / cleverness / premature generality** — last.

A plainer design that stays reliable beats a flexible one that hangs, desyncs
hosts, or lies about resume.

## Relentless cutting

- Delete before adding. Every type, dependency, flag, and error variant must
  earn its place against a real user path.
- No speculative abstraction, no config for futures that may never ship, no
  error taxonomies for impossible cases.
- No features beyond what was asked in the task at hand.
- Prefer one honest limitation (document it at the site and in `TODO.md`)
  over a half-working generality.
- When in doubt: cut it, inline it, or simplify it.

## Writing for the reader

- **Docs describe the current codebase, not its history.** No migration essays,
  rename archaeology, or “how we got here.” Git history remembers the rest.
- **Module docs state role and invariants**, not a walkthrough of the file.
  Comments teach *why* and constraints; they never narrate what the next line
  does.
- **The only comments that ship in a PR are comments that should live in the
  codebase.** A comment that narrates the change, addresses a reviewer, or
  restates a design discussion belongs in the PR description or commit
  message, not the code. If it would not make sense to a reader who never saw
  the PR, cut it.
- **Terminology stays stable.** Prefer **host** (execution place for tools:
  `local` in-process or remote worker) over “machine/node/target” in code,
  config, tool schemas, and CLI. User-facing marketing may say “machines”;
  the domain word is still `host`.
- **Tests are claims.** Prefer names that state the invariant
  (`cancel_during_slow_tool_records_cancelled_result`, not `test_cancel_1`).

## Architecture (current)

```
myco --mode serve  (the server: Rocket /api + one agent task per live session)
  └── Server / Live sessions / Room (multiplayer inbox)
        └── Agent (turn loop) → Harness (routing, root-only services)
              ├── HostController "local"  → in-process HostWorker (always on)
              └── HostController "…"      → ssh … myco --mode host (lazy remote)
                    └── standard tools: bash, editor, view_image
clients: myco-gui (Yew) · myco -p · clients/myco.py · agent tools via $MYCO_API
```

Nested agents are the root-only `subagent` tool: one call runs one full turn
of a hidden child session through the same `Server` and returns its answer;
children surface in the GUI's work panel and can be taken over by a person.
Nesting is local-only by doctrine: brains (config, keys, gateway access,
session store) stay on the server's machine; remotes stay hands.

Three crates — `myco` (everything server-side), `myco-api` (wire types, the
`MycoApi` trait; wasm-safe), `myco-gui` (Yew client) — plus
`clients/myco.py`. Inside `myco`:

| Area | Role |
|------|------|
| `src/main.rs` | The binary: `--mode serve` (the server), `--mode host` (worker), `-p` (thin one-shot HTTP client) |
| `src/core/` | Bottom layer, depends on nothing: `Async`/`AsyncStream` aliases, `CancelToken`, image decoding, `myco_home()`/`atomically_write()`, and the external-command registry |
| `src/models/` | Protocol drivers (Anthropic Messages, OpenAI Responses, OpenAI Chat Completions) + `ModelSpec`/`ModelCatalog`; no built-in models |
| `src/config/` | Config file shape (`~/.myco/v2/config.toml`) + startup resolution; roster (`server.toml`) |
| `src/session/` | Session persistence only: documents under `~/.myco/v2/`, the single-writer lock, and the compaction *document* logic |
| `src/prompts/` | System prompt fragments + prelude / project-guidance injection + the session stamp |
| `src/machines/` | The hands: `Harness` (host pool), `HostController`/`HostWorker` + NDJSON protocol (tool calls **and** the shell observer surface), every `ToolService` (bash with pty/screen/locks, editor, view_image) |
| `src/agent/` | The turn loop: `Agent::interact`, `AgentEvent`/`EventSink`, cancellation, the compact worker |
| `src/auth/` | Credentials and access tokens behind the OAuth2 password grant |
| `src/server.rs` | The session runtime: `Live` handles, the `Room` (multiplayer + pre-emption), shells/subagent surfaces, `UserApi` (the `MycoApi` impl) |
| `src/web.rs` | Rocket adapter: REST one-liners over `MycoApi`, SSE events, the shell WebSocket, operator token |
| `src/cli.rs`, `src/client.rs`, `src/admin.rs`, `src/subagent.rs` | The `-p` client; `MycoApi` over HTTP; `myco auth`; the `subagent` tool |
| `tests/` | Integration tests (multiplayer, shells, subagents, cancel, host desync, …) |

**Invariants worth protecting**

- **Local is always in-process** — never require a local `myco --mode host`
  subprocess for the default host.
- **Remotes are lazy** — connect on first tool use; soft-fail non-default hosts.
- **Standard tool catalog is the same on every host**; root-only tools
  (`session_meta`, `subagent`, `prelude`) are installed only on the in-process
  local worker.
- **Tool field `host`** defaults to `local`; bash sessions are **per host**
  (and per agent id).
- **Conversation resume ≠ restored bash/editor state** — document honesty;
  don’t fake rehydration.
- **Builds are offline** beyond the crates.io fetch; no network at compile
  time. Ship platform-matched binaries; do not scp across glibc/arch
  boundaries.
- **Local and remote myco run the same version** — connect fails loud on
  package-version skew, which is what keeps the assumed tool catalog and the
  NDJSON protocol sound.
- **The module graph is acyclic.** Bottom-up: `core` → `models` → `config` →
  `session` → `prompts` → `machines` → `agent` → `server` → `web`/`cli`. A
  module reaching *up* that list is the smell; the fix is usually that the
  shared thing belongs lower down. `#[cfg(test)]` may reach anywhere — test
  setup composes real layers on purpose.
- **One keyboard.** Every interactive surface (shells, subagents) shares the
  lock: writes gated, reads free, every handoff an attributed non-waking
  transcript note, refusals polite on both sides.
- **REST is canonical.** SSE and the shell WebSocket are projections of the
  same `UserApi` guts; a client without them loses latency, not capability.
  New capabilities land on `MycoApi` + REST first.

## Code style

Match neighboring code. Myco is not unit’s banner-heavy textbook style; keep
structure light.

- **Sections:** light dashed banners where files already use them
  (`// ---------------------------------------------------------------------------`),
  not mandatory chapter scaffolding on every module.
- **Imports:** external crates first, blank line, then `crate::…`.
- **APIs:** small focused functions; push protocol/IO detail behind clear types
  (`HostController`, `HostWorker`, `ToolService`).
- **Async:** Tokio; host pipe is concurrent/pipelined — don’t serialize tool
  calls without a reason.
- **Errors:** prefer actionable messages (config path, host name, SSH hint)
  over deep error enums nobody matches.
- **Schema/config breaks:** session files and tool JSON are real contracts.
  Bump/reject deliberately (`SESSION_FILE_VERSION`); don’t silently reinterpret.
- **Comments:** invariants, non-obvious protocol choices, and “why this is safe”
  — never restate the code.

Agent workflow defaults (also in system prompt fragments):

1. **Think before coding** — surface assumptions and tradeoffs; don’t hide
   confusion.
2. **Simplicity first** — minimum code; rewrite if 200 lines could be 50.
3. **Surgical changes** — touch only what the task requires; clean up only
   orphans *you* created.
4. **Goal-driven** — write the failing check or repro, then make it pass.
5. **User authority** — never force-merge / admin-bypass checks or land PRs
   without explicit user approval (see prompt fragment `user-authority`).

## Develop

```bash
cargo build --locked
cargo test --locked                      # whole suite; no network, no providers
cargo clippy --workspace --all-targets   # zero warnings is the bar
cargo clippy -p myco-gui --target wasm32-unknown-unknown
cargo run -p myco -- --mode serve        # then `trunk serve` for the GUI
```

- API credentials: `~/.myco/v2/config.toml` (`[gateways]` auth sources); see
  `README.md`.
- A guided reading of the whole codebase: `TOUR.md`.

## What not to do

- Don’t rename the **host** domain (`host` tool field, `--mode host`,
  `src/machines/host/`) for cosmetic synonyms without an explicit,
  breakage-aware migration plan.
- Don’t scp prebuilt `myco` binaries across mismatched OS/arch/libc; build on
  the target or use a matching asset (`harness-ops`).
- Don’t treat reopening a session as full workspace restore (bash/editor
  state dies with the agent task).
- Don’t edit `~/.myco/v2/*.json` by hand from the agent — use `session_meta`
  or the API.
- Don’t force-merge PRs, bypass branch protection/required checks, or use
  admin privileges to override the user’s review workflow without explicit
  approval in the conversation.

## Backlog pointer

Living priorities and explicit rejects: **`TODO.md`**. Prefer P0 trust items
over shiny multiplayer/GUI unless the user steers otherwise.
