# `myco`

A minimalist coding agent that works across your machines over SSH — now a
**server**: `myco` runs an API on localhost, drives multiple concurrent agent
sessions, and serves a minimal web GUI. (v2 rewrite; the v1 CLI lives in the
`main` branch's history and on crates.io ≤ 0.3.x.)

## Why use it?

- **One brain, many machines.** Every tool call names its host: `local` or any
  concrete `Host` alias from `~/.ssh/config`. Remotes attach over SSH on
  demand and need only `myco` on PATH — no config, no keys.
- **Real computer use.** Bash (including multi-turn sessions) and a surgical
  file editor on each host; search and browsing compose from the tools already
  on your machines (`rg`, `curl`, `ck`, …) via bash.
- **Concurrent sessions.** Each conversation is a URL; agents run in parallel
  server-side, and input queues while a turn is running.
- **Sessions you can resume.** Titles, scratchpads, links, and full history
  live under `~/.myco/` — reopen any session by URL, or from another client.
- **Nested agents as a tool.** The root-only `subagent` tool runs one full
  turn of a hidden child session per call — no curl, works behind strict
  firewalls, optional context forks. The same surface exists as HTTP
  (`$MYCO_API`) for scripts.
- **Live streaming.** Each session exposes an SSE feed
  (`/api/sessions/<id>/events`) of text/thinking deltas and tool starts;
  the GUI streams it and reconciles with a slow poll.

## Run

```bash
cargo run -p myco            # API server on http://127.0.0.1:7773/api
trunk serve                  # web GUI on http://127.0.0.1:8080 (proxies /api)
```

`trunk serve` needs [trunk](https://trunkrs.dev) and the wasm target
(`rustup target add wasm32-unknown-unknown`). The `Trunk.toml` at the repo
root builds `crates/myco-gui` and reverse-proxies `/api` to the server.

Configure models first in `~/.myco/config.toml` (`[gateways.*]` +
`[models.*]`; `myco --model <key>` / `--config <path>` to override). Remotes
just work: the harness spawns `ssh <alias> myco --mode host` lazily, so a
remote only needs your key in `ssh-agent` and `myco` on the PATH used by
non-interactive SSH.

## API

`GET /api/sessions` · `POST /api/sessions` (`{model?, parent_session?,
fork?}`) · `GET /api/sessions/<id>` · `POST /api/sessions/<id>/messages`
(`{text}`) · `GET /api/sessions/<id>/poll?since=N` ·
`GET /api/sessions/<id>/events` (SSE) · `POST /api/sessions/<id>/cancel` ·
`DELETE /api/sessions/<id>/live` · `GET /api/models`. Wire types live in
`crates/myco-api`.

## Workspace

- `myco` — the **root crate**: server binary (Rocket, `/api`), meta lib,
  the `subagent` tool, and `--mode host` (the per-machine worker remotes
  run); the workspace lives in `crates/`
- `myco-gui` — minimal Yew web client (one URL per conversation)
- `myco-api` — wire types shared by server and clients
- `myco-agent`, `myco-session`, `myco-machines`, `myco-models`,
  `myco-config`, `myco-prompts`, `myco-core` — the server's pillars
- `myco-test-support` — shared test fixtures

## Develop

```bash
cargo test --locked
cargo run --locked -p myco
bash scripts/install-pre-commit-hooks.sh   # optional: CI bar (fmt + clippy) pre-commit
```
