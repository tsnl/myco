# `myco`

A minimalist coding agent that works across your machines over SSH. One
session runtime, two frontends: the interactive **CLI** (default — now
async: your prompt stays live, input queues while the agent works, output
streams above it) and a **multiplayer web server** (`myco --mode serve`), a
parallel experiment serving the same runtime over HTTP for the Yew GUI and
scripts.

## Why use it?

- **One brain, many machines.** Every tool call names its host: `local` or any
  concrete `Host` alias from `~/.ssh/config`. Remotes attach over SSH on
  demand and need only `myco` on PATH — no config, no keys.
- **Real computer use.** Bash (including multi-turn sessions) and a surgical
  file editor on each host; search and browsing compose from the tools already
  on your machines (`rg`, `curl`, `ck`, …) via bash.
- **Async everywhere.** Input queues while a turn runs — in the CLI and the
  web alike. In serve mode each conversation is a URL and agents run in
  parallel server-side.
- **Sessions you can resume.** Titles, scratchpads, links, and full history
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
  (`/api/sessions/<id>/events`) of text/thinking deltas and tool starts;
  the GUI streams it and reconciles with a slow poll.

## Run

```bash
cargo run -p myco                          # interactive CLI (async)
cargo run -p myco -- -p "explain src/"     # one-shot print mode
cargo run -p myco -- --mode serve          # API server on http://127.0.0.1:7773/api
trunk serve                                # web GUI on :8080 (proxies /api)
```

The web GUI keeps the terminal's visual identity: monospace, dark, USER
rules — minimal chrome by design. Tool calls render as collapsed cards
(name, pretty-printed arguments with long strings elided, result folded in);
click one, or the transcript-wide `verbose` toggle, to see the call exactly
as the model made it. Code blocks are syntax-highlighted.

`trunk serve` needs [trunk](https://trunkrs.dev) and the wasm target
(`rustup target add wasm32-unknown-unknown`). The `Trunk.toml` at the repo
root builds `crates/myco-gui` and reverse-proxies `/api` to the server.

Configure models first in `~/.myco/v2/config.toml` (`[gateways.*]` +
`[models.*]`; `myco --model <key>` / `--config <path>` to override), and
register yourself in `~/.myco/v2/server.toml`:

```toml
[[users]]
id = "ada"            # matched against $MYCO_USER, then $USER
name = "Ada Lovelace" # optional; defaults to the id
token = "…"           # optional; required to reach the HTTP API
                      # generate one with: openssl rand -hex 32
```

Both the CLI and the server refuse to start without a roster that names the
user they are running as — every session entry records its author, and a
name nobody registered has no business in a shared transcript. Override the
path with `--server-config` or `$MYCO_SERVER_CONFIG`. Remotes
just work: the harness spawns `ssh <alias> myco --mode host` lazily, so a
remote only needs your key in `ssh-agent` and `myco` on the PATH used by
non-interactive SSH.

## API

`GET /api/sessions?include_archived=` · `POST /api/sessions` (`{model?,
parent_session?, fork?}`) · `GET /api/sessions/<id>` ·
`PATCH /api/sessions/<id>` (`{title?, archived?}`) ·
`POST /api/sessions/<id>/messages`
(`{text}`) · `GET /api/sessions/<id>/poll?since=N` ·
`GET /api/sessions/<id>/events` (SSE) · `POST /api/sessions/<id>/cancel` ·
`DELETE /api/sessions/<id>/live` · `GET /api/models` · `GET /api/whoami`.
Wire types live in `crates/myco-api`.

Every route requires `Authorization: Bearer <token>` naming a roster user;
the entries a request writes are attributed to that user, not to whoever
started the server. (`GET /sessions/<id>/events` also accepts `?token=`,
since `EventSource` cannot set headers.) `--mode serve` exports
`$MYCO_API_TOKEN` alongside `$MYCO_API` so tools the agent spawns can reach
the API as the local operator. In the web GUI you paste your token once and
it is kept in `localStorage`.

## Workspace

- `myco` — the **root crate**: the session runtime (`server`), the CLI,
  the web server (Rocket, `/api`), the `subagent` tool, and `--mode host`
  (the per-machine worker remotes run); the workspace lives in `crates/`
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
