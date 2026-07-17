# `myco`

[![Crates.io](https://img.shields.io/crates/v/myco.svg)](https://crates.io/crates/myco)
[![CI](https://github.com/tsnl/myco/actions/workflows/ci.yml/badge.svg)](https://github.com/tsnl/myco/actions/workflows/ci.yml)

A minimalist coding agent that works across your machines over SSH.

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
  and `AGENTS.md` / `CLAUDE.md` so the agent can find how _you_ work.
- **Coming later:** multiplayer (multiple humans in the same agent workspace).

## Requirements

- API credentials for the model backend you pick (see below)
- **Rust / cargo** (stable)
- Network on first build — `build.rs` uses **`hf-hub`** to fetch MiniLM safetensors into the shared Hugging Face cache
- Extra binaries on `PATH`
  - Required: `ssh`, `lynx` (web-browser), `uv`, `bash`
  - Recommended: `git`, `gh`, `curl`

Optional: `trunk` + `wasm32-unknown-unknown` only if you build **`crates/myco-gui`**.

## Setup

From `crates.io`:

```bash
cargo install myco
```

## Use

> [!IMPORTANT]
>
> Before you get started, you need to configure LLM API keys.

```bash
# Interactive agent CLI (default model: grok-4.5-build)
myco
```

### LLM API credentials

`myco` loads a `.env` from the current directory (via `dotenvy`) and also reads
the process environment. Defaults: model **`grok-4.5-build`** (xAI / OpenAI
Responses API). Pass `--model <id>` for Claude models.

**Anthropic Messages** (Claude: `claude-haiku-4-5`, `claude-sonnet-4-6`,
`claude-opus-4-8`, `claude-fable-5`, …):

| Variable                                      | Role                                           |
| --------------------------------------------- | ---------------------------------------------- |
| `ANTHROPIC_AUTH_TOKEN` or `ANTHROPIC_API_KEY` | Bearer token (required)                        |
| `ANTHROPIC_BASE_URL`                          | API base (default `https://api.anthropic.com`) |

**xAI / OpenAI Responses** (default model `grok-4.5-build`; also any gateway that
speaks the Responses API at `{base}/responses`):

| Variable                                | Role                                     |
| --------------------------------------- | ---------------------------------------- |
| `XAI_API_KEY` or `OPENAI_API_KEY`       | Bearer token (required)                  |
| `XAI_API_BASE_URL` or `OPENAI_BASE_URL` | Base URL (default `https://api.x.ai/v1`) |

If neither xAI/OpenAI key is set, the OpenAI Responses backend also accepts
`ANTHROPIC_AUTH_TOKEN` / `ANTHROPIC_API_KEY` as a fallback token source.

### (Optional) Remote Hosts Config

If you want to use `myco` with one or more remote hosts, you can configure this in
`~/.myco/config.toml`.

```toml
# ~/.myco/config.toml

enable_subagent = true

# Per-remote connect timeout in seconds on first tool use.
attach_timeout_secs = 10

[[remote_hosts]]
name = "tsnl-desktop"
ssh = "tsnl-desktop.yellow-submarine.ts.net"

[[remote_hosts]]
name = "gpu"
ssh = "ubuntu@ec2-12-34-56-78.compute-1.amazonaws.com"
```

## Develop

```bash
cargo test --locked --lib
cargo run --locked --bin myco
```

Optional local pre-commit (same bar as CI: `cargo fmt --check` + `cargo clippy -D warnings`):

```bash
bash scripts/install-pre-commit-hooks.sh
```

Bypass once with `git commit --no-verify`.

NOTE: Semantic search embeds **all-MiniLM-L6-v2** via **Candle** (no ONNX Runtime) at
**compile time**. `build.rs` downloads via **`hf-hub`** into the shared Hub cache
(`~/.cache/huggingface` / `HF_HOME`), stages under `OUT_DIR`, and bakes weights
into the binary — nothing large is in git. Worktrees reuse the same cache.

```bash
# optional offline seed (usually unnecessary after one successful build)
bash scripts/seed-minilm-weights.sh
# or: export MYCO_EMBED_CACHE=/path/to/flat-dir-with-the-three-files
```
