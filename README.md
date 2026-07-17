# `myco`

Mycelial multi-scale agent runtime.

**Myco** is a coding agent whose structure repeats at every scale: supervisors
delegate to subagents, subagents share the same harness and host pool, and tools
run on **hosts** (local and remote). Brain/hands is one thread; multi-level agent
orchestration is the network — a mycelium of agency.

```
                  ┌─ Agent (supervisor)
                  │    ├── subagent ──► Agent (same harness / host pool)
                  │    │                  └── tools → hosts …
                  │    └── tools ─────────────────────┐
                  ▼                                   ▼
            Harness (routing)                    Host pool
         local | remote | …              bash, editor, search, …
```

- **Local host** is always in-process (no subprocess).
- **Remotes** use `ssh … myco --mode host` over NDJSON.
- The same `myco` binary is the interactive agent **and** the remote host worker.
- **Subagents** share the supervisor’s harness so orchestration and execution stay
  one system at every level.

Formerly developed as [`honk`](https://github.com/tsnl/honk); this repository is the
continued home under the **myco** name.

## Requirements

- **Rust / cargo** (stable)
- **`curl`** — `build.rs` fetches MiniLM safetensors at compile time
- API credentials for your generative backend (see env below)

Optional: `trunk` + `wasm32-unknown-unknown` only if you build **`crates/myco-gui`**.

## Setup

```bash
# From the crate root:
cargo build --locked

# Credentials (example — Anthropic-compatible endpoint):
cat <<-EOF | tee .env
export ANTHROPIC_BASE_URL=...
export ANTHROPIC_AUTH_TOKEN=...
EOF
# or put the same exports in your shell profile
```

Install the CLI (user prefix):

```bash
cargo install --path . --locked --force
myco --version
```

Config (remotes only; local needs no entry): `~/.myco/config.toml` — see
`myco --help overview` / `harness-ops`.

### Migrating from honk

| Honk | Myco |
|------|------|
| binary `honk` | binary `myco` |
| `~/.honk/` | `~/.myco/` |
| `$HONK_CONFIG`, `$HONK_HOME`, … | `$MYCO_CONFIG`, `$MYCO_HOME`, … |
| repo path `.honk/` (worktrees, skills, subagent logs) | `.myco/` |
| config key `honk = "…"` (remote binary) | `myco = "…"` (legacy key `honk` still accepted) |

```bash
mv ~/.honk ~/.myco   # or cp -a
# edit remote binary paths if they still point at `honk`
```

## Use

```bash
# Interactive agent CLI
myco
# or from a checkout without install:
cargo run --locked --bin myco
```

In-session: slash-commands such as `/help`, `/hosts`, `/session` (run by you, not the
agent). Agent-facing runtime docs: `manual` tool or `myco --help <article>` —
`overview`, `cli`, `harness-ops`.

### Multi-host / release

Embedding weights are **fully embedded** in the `myco` binary at compile time. Shipping a
release only needs **platform-matched binaries** — no separate model files at run time.

- **Same OS/arch/libc:** install that binary (weights already inside).
- **Mismatched platforms:** build on the target (or use a matching release asset); do
  not scp binaries across glibc/arch boundaries.
- **Source builds:** weight *files* under `src/text_search/embed_weights/` may be
  **copied** between build machines, or fetched by `build.rs` / curl.

See `myco --help harness-ops`.

## Architecture (crate)

| Piece              | Role                                                            |
| ------------------ | --------------------------------------------------------------- |
| `myco` binary      | Agent CLI + `--mode host` for remote workers                    |
| Host tools         | `bash`, editor, `manual`, text search (Tantivy + Candle MiniLM) |
| Local tools        | `subagent`, `session_meta`                                      |
| `crates/myco-gui`  | Optional Yew UI (separate from the CLI path above)              |

Sessions and metadata live under `~/.myco/` (not edited as raw JSON by the agent —
use `session_meta`).

## Develop

```bash
cargo test --locked --lib
cargo run --locked --bin myco
```

Feature work: prefer a git worktree under `.myco/worktrees/` (see agent system
guidance / project norms).

## Embedding weights (MiniLM / Candle)

Semantic search embeds **all-MiniLM-L6-v2** via **Candle** (no ONNX Runtime) at
**compile time**. Assets are downloaded by `build.rs` into
`src/text_search/embed_weights/` and baked into the binary — nothing large is in git.

```bash
BASE=https://huggingface.co/sentence-transformers/all-MiniLM-L6-v2/resolve/main
mkdir -p src/text_search/embed_weights
curl -fL -o src/text_search/embed_weights/model.safetensors "$BASE/model.safetensors"
curl -fL -o src/text_search/embed_weights/tokenizer.json "$BASE/tokenizer.json"
curl -fL -o src/text_search/embed_weights/config.json "$BASE/config.json"
```
