# `myco`

[![Crates.io](https://img.shields.io/crates/v/myco.svg)](https://crates.io/crates/myco)
[![CI](https://github.com/tsnl/myco/actions/workflows/ci.yml/badge.svg)](https://github.com/tsnl/myco/actions/workflows/ci.yml)

A minimalist coding agent that works across your machines over SSH.

Run `myco` on your laptop: one conversation that edits files, runs shells
(including multi-turn sessions), browses in text mode, and searches code
(keyword + semantic) on the local machine **and** on every concrete `Host`
alias in your `~/.ssh/config` — no host setup beyond SSH itself. Sessions are
resumable (`/resume`), sub-agents keep long work cheap, and skill packs /
`AGENTS.md` stay indexed so the agent can find how _you_ work.

## Install

```bash
cargo install myco
```

Needs stable Rust, network on the first build (`build.rs` bakes MiniLM
embedding weights into the binary via `hf-hub`), and `ssh`, `lynx`, `uv`,
`bash` on `PATH` (`git`, `gh`, `curl` recommended).

## Use

```bash
myco    # default model: grok-4.5-build; pass --model <id> for Claude models
```

Credentials come from the process environment (a `.env` in the cwd is also
loaded):

| Backend                                | Variables                                                                        |
| -------------------------------------- | -------------------------------------------------------------------------------- |
| **Anthropic Messages** (`claude-*`)    | `ANTHROPIC_AUTH_TOKEN` or `ANTHROPIC_API_KEY`; optional `ANTHROPIC_BASE_URL`     |
| **xAI / OpenAI Responses** (`grok-*`)  | `XAI_API_KEY` or `OPENAI_API_KEY`; optional `XAI_API_BASE_URL` / `OPENAI_BASE_URL` |

Remotes just work: myco attaches lazily with `ssh <alias> myco --mode host`,
so a remote only needs your key in `ssh-agent` and `myco` on the PATH used by
non-interactive SSH. Runtime details: `myco --help overview`.

## Develop

```bash
cargo test --locked --lib
cargo run --locked --bin myco
bash scripts/install-pre-commit-hooks.sh   # optional: CI bar (fmt + clippy) pre-commit
```

Semantic search embeds **all-MiniLM-L6-v2** at compile time: `build.rs`
downloads into the shared Hugging Face cache and bakes the weights into the
binary — nothing large is in git. Offline seed:
`bash scripts/seed-minilm-weights.sh` or `MYCO_EMBED_CACHE`.
