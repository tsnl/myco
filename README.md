# `myco`

A coding agent that works across your machines over SSH.

Run `myco` on your laptop. It edits files, runs shells, and searches code on the
local machine **and** on remotes you configure — one session, many hosts.

## Why use it?

- **One agent, many machines.** Point tools at `local` or a named remote (`devbox`,
  GPU box, CI host). Remotes attach over SSH on demand; you stay in a single
  conversation.
- **Real computer use.** Bash (including multi-turn sessions), a surgical file
  editor, text-mode browsing, and indexed search (keyword + semantic) on each host.
- **Sessions you can resume.** Titles, scratchpads, PR/worktree links, and full
  conversation history live under `~/.myco/` — pick up later with `/resume`.
- **Sub-agents for long work.** Spin off focused agents so the main thread stays
  small and cheap.
- **Skills and project guidance stay searchable.** Hosts auto-index skill packs
  and `AGENTS.md` / `CLAUDE.md` so the agent can find how *you* work.
- **Coming later:** multiplayer (multiple humans in the same agent workspace).

## Requirements

- **Rust / cargo** (stable)
- **`curl`** — `build.rs` fetches MiniLM safetensors at compile time
- API credentials for the model backend you pick (see below)

Optional: `trunk` + `wasm32-unknown-unknown` only if you build **`crates/myco-gui`**.

## Setup

```bash
# From the crate root:
cargo build --locked
```

### API credentials

`myco` loads a `.env` from the current directory (via `dotenvy`) and also reads
the process environment. Defaults: model **`grok-4.5-build`** (xAI / OpenAI
Responses API). Pass `--model <id>` for Claude models.

**Anthropic Messages** (Claude: `claude-haiku-4-5`, `claude-sonnet-4-6`,
`claude-opus-4-8`, `claude-fable-5`, …):

| Variable | Role |
| -------- | ---- |
| `ANTHROPIC_AUTH_TOKEN` or `ANTHROPIC_API_KEY` | Bearer token (required) |
| `ANTHROPIC_BASE_URL` | API base (default `https://api.anthropic.com`) |

```bash
cat <<-EOF | tee .env
export ANTHROPIC_AUTH_TOKEN=sk-ant-...
# export ANTHROPIC_BASE_URL=https://api.anthropic.com   # optional override
EOF
```

**xAI / OpenAI Responses** (default model `grok-4.5-build`; also any gateway that
speaks the Responses API at `{base}/responses`):

| Variable | Role |
| -------- | ---- |
| `XAI_API_KEY` or `OPENAI_API_KEY` | Bearer token (required) |
| `XAI_API_BASE_URL` or `OPENAI_BASE_URL` | Base URL (default `https://api.x.ai/v1`) |

If neither xAI/OpenAI key is set, the OpenAI Responses backend also accepts
`ANTHROPIC_AUTH_TOKEN` / `ANTHROPIC_API_KEY` as a fallback token source.

```bash
cat <<-EOF | tee .env
export XAI_API_KEY=xai-...
# export XAI_API_BASE_URL=https://api.x.ai/v1   # optional
# Or OpenAI-compatible:
# export OPENAI_API_KEY=sk-...
# export OPENAI_BASE_URL=https://api.openai.com/v1
EOF
```

Install the CLI (user prefix):

```bash
cargo install --path . --locked --force
myco --version
```

Config (remotes only; local needs no entry): `~/.myco/config.toml` — see
`myco --help overview` / `harness-ops`.

## Use

```bash
# Interactive agent CLI (default model: grok-4.5-build)
myco

# Claude via Anthropic env vars
myco --model claude-sonnet-4-6

# From a checkout without install:
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
- **Source builds:** weight _files_ under `src/text_search/embed_weights/` may be
  **copied** between build machines, or fetched by `build.rs` / curl.

See `myco --help harness-ops`.

## Architecture (crate)

| Piece             | Role                                                            |
| ----------------- | --------------------------------------------------------------- |
| `myco` binary     | Agent CLI + `--mode host` for remote workers                    |
| Host tools        | `bash`, editor, `manual`, text search (Tantivy + Candle MiniLM) |
| Local tools       | `subagent`, `session_meta`                                      |
| `crates/myco-gui` | Optional Yew UI (separate from the CLI path above)              |

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
