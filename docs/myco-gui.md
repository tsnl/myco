# myco-gui — web frontend design (planning)

> Status: **planning / RFC**. No server code lands in this PR. This document
> stakes out the shape of `myco --mode server` and the `crates/myco-gui`
> browser frontend so implementation can proceed in reviewable slices.

## Why

The `myco` CLI is a single-session REPL: one conversation, one terminal, tool
output interleaved as text. That is the right default for trust and long
sessions (see `AGENTS.md` ranking + `TODO.md`), but it caps what a human can
*watch* and *steer*:

- You cannot follow multiple sessions at once.
- You cannot watch a long-running `bash` process, a subagent tree, or a
  multi-host tool fan-out expand/collapse live.
- Transcript, host status, session metadata, and search all live behind
  slash-commands and scrollback rather than a navigable surface.

A browser GUI is the natural richer client. It does **not** replace the CLI as
the trust baseline — it is a second front-end over the *same* harness, sessions,
config, and hosts. Anything the GUI can do, the underlying library already does
for the CLI.

## Non-goals (for the GUI, initially)

- Not a new agent runtime. The GUI is a view/control layer over the existing
  `Agent` / `Harness` / `Session` types (`src/lib.rs` already re-exports them).
- Not a replacement for CLI trust work. GUI must not outrank P0/P1 in `TODO.md`.
- Not multi-tenant / auth / accounts in v1 (see "Hosted, later").
- Not a mobile app; responsive web is enough.

## Architecture at a glance

```
                    ┌──────────────────────────────────────────┐
  browser           │  myco --mode server  (single process)     │
 ┌────────┐  HTTP   │                                            │
 │myco-gui│◀──────▶ │  http layer (rocket)                       │
 │ (Yew   │  +WS/   │    ├─ REST: sessions, config, hosts        │
 │  wasm) │  SSE    │    └─ stream: per-session AgentEvent feed   │
 └────────┘         │           ▲                                 │
                    │           │ EventSink                       │
                    │  ┌────────┴─────────┐                       │
                    │  │ session registry │  Agent + history      │
                    │  └────────┬─────────┘                       │
                    │           │                                 │
                    │        Harness ──── HostController ──┐      │
                    │           │ local (in-process)       │      │
                    │           └ remote (ssh myco --mode host)   │
                    └──────────────────────────────────────────┘
                                filesystem: ~/.myco/{config,session}
```

Key point: `--mode server` reuses the **exact same** filesystem, config, and
host-attach paths as `--mode interactive`. It is a third `Mode` alongside
`Interactive` and `Host` in `src/bin/myco.rs`, not a fork of the runtime.

### Backend: `myco --mode server`

- New `Mode::Server` variant + `run_server(args)` in `src/bin/myco.rs`.
- Reuses `load_harness_config`, `default_config_path`, `ensure_remote_ssh_identities`,
  `Harness`, and the session store under `~/.myco/session/`.
- Serves HTTP via `rocket` (already a dependency; currently unused — this is its
  intended consumer). Flags: `--port` (default 8000, matching `Trunk.toml`'s
  `/api` proxy target), `--bind` (default `127.0.0.1`).
- Holds a **session registry**: a map of `session_id -> live Agent` plus a
  broadcast channel of `AgentEvent` per session. The GUI's live view is a thin
  consumer of the same `EventSink`/`AgentEvent` stream the CLI already renders
  (`src/session/agent.rs`). No new event taxonomy is invented for the GUI.

### Frontend: `crates/myco-gui` (Yew, wasm)

- Already scaffolded: `crates/myco-gui/{Cargo.toml,index.html,src/main.rs}`,
  built by `Trunk.toml` (target `crates/myco-gui/index.html`, dev server on
  `:8080` reverse-proxying `/api` → `:8000`).
- Talks to the backend over `/api/**` (REST) and a streaming endpoint
  (SSE or WebSocket) for live `AgentEvent`s.
- Rendering mirrors CLI transcript semantics (RESPONSE / Thinking summary / tool
  invocations) but as collapsible, concurrent, navigable components.

## HTTP surface (draft, v1)

REST (JSON):

| Method | Path                          | Purpose                                    |
| ------ | ----------------------------- | ------------------------------------------ |
| GET    | `/api/health`                 | liveness + version                         |
| GET    | `/api/hosts`                  | host pool + status (mirrors `/hosts`)      |
| GET    | `/api/sessions`               | list saved + live sessions                 |
| POST   | `/api/sessions`               | create a new session (model, effort, cwd)  |
| GET    | `/api/sessions/{id}`          | metadata + full transcript                 |
| POST   | `/api/sessions/{id}/messages` | submit a user turn                         |
| POST   | `/api/sessions/{id}/cancel`   | cancel the in-flight turn (`CancelToken`)  |
| PATCH  | `/api/sessions/{id}`          | title / scratchpad / links (`session_meta`)|

Streaming:

| Path                        | Transport | Payload                                   |
| --------------------------- | --------- | ----------------------------------------- |
| `/api/sessions/{id}/events` | SSE or WS | serialized `AgentEvent` frames (live feed)|

`AgentEvent` already carries `TraceContext { agent_id, depth, parent_tool_use_id }`,
so subagent trees and per-tool nesting are expressible on the wire without new
types. Serialization is additive: derive/implement `serde` for the event +
`ToolUse`/`TurnEndReason` shapes behind the server boundary (do not leak wire
concerns into the core CLI path).

## GUI feature set (v1 target)

The GUI's reason to exist is richer monitoring + concurrency:

1. **Multiple concurrent sessions.** Sidebar of sessions (live + resumable);
   open several; each streams independently.
2. **Live process / subagent tree.** Expand/collapse running tool uses,
   subagents (by `agent_id`/`depth`), and multi-host fan-outs. Watch long
   `bash` output stream in place; collapse when done.
3. **Host dashboard.** Pool status, attach/soft-fail state, which host each
   in-flight tool is running on.
4. **Session metadata surface.** Title, scratchpad, PR/worktree links as
   first-class editable UI (backed by `session_meta`).
5. **Transcript parity.** RESPONSE text, `Thinking:` summary, tool invocations
   rendered with the same semantics the CLI uses — no divergent truth.

Explicitly deferred: rich diff review, inline editors, multiplayer cursors,
notifications. Keep v1 a faithful, richer *window* onto the existing runtime.

## Hosted, later (directional, not v1)

Once local `--mode server` is solid, the same binary can run in a container as a
hosted instance:

- Persist all of a user's sessions (already file-backed under `~/.myco/`; mount a
  volume).
- SSH out to the user's remote hosts (reuses `RemoteHostConfig` +
  `ensure_remote_ssh_identities`).
- Adds the concerns v1 skips: authentication, per-user isolation, secrets
  management, TLS. These are deliberately out of scope until the local GUI earns
  its keep.

## Build & CI notes

- `crates/myco-gui` is **not** part of the root cargo build/clippy/test matrix
  (CI comment in `.github/workflows/ci.yml` already excludes it; it needs
  `trunk` + `wasm32-unknown-unknown`).
- The server backend *is* root-crate code and will be covered by normal
  `cargo clippy --all-targets` + `cargo test`.
- Follow-up: a separate optional CI job to `trunk build` the GUI once it renders
  something real.

## Rollout slices (suggested PR sequence)

1. **(this PR)** Plan + `TODO.md` entry. No runtime change.
2. `Mode::Server` skeleton: `/api/health`, `/api/hosts`, static file serving of
   the built GUI. No live agent yet.
3. Session registry + read-only endpoints (`/api/sessions`, transcript GET).
4. Live turn: `POST messages` + `/events` stream wired to a real `Agent` +
   `EventSink`; serde for `AgentEvent`.
5. Cancel + `session_meta` PATCH; GUI transcript parity.
6. GUI concurrency features (multi-session, subagent/process tree, host board).

Each slice is independently reviewable and leaves CLI trust untouched.

## Open questions

- **SSE vs WebSocket** for the event stream. SSE is simpler (one-way, fits
  `AgentEvent`); user input is a separate `POST`. WS only if we later need
  low-latency bidirectional (e.g. live keystroke interrupts). Lean SSE for v1.
- **Turn concurrency per process.** One tokio runtime already drives many
  hosts; confirm N concurrent live `Agent`s share the `Harness`/host pool
  cleanly (they should — hosts are already shared, tools already fan out).
- **Auth boundary** even for local: bind `127.0.0.1` only + optional token, so a
  browser tab can't be driven by arbitrary local pages (CSRF on `/api`).
