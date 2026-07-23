# `myco`

[![Crates.io](https://img.shields.io/crates/v/myco.svg)](https://crates.io/crates/myco)
[![CI](https://github.com/tsnl/myco/actions/workflows/ci.yml/badge.svg)](https://github.com/tsnl/myco/actions/workflows/ci.yml)

A minimalist coding agent that works across your machines over SSH.

Run `myco` on your laptop. It edits files, runs shells, and searches code on
the local machine **and** on every concrete `Host` alias in your
`~/.ssh/config` — one session, many hosts, no setup beyond SSH itself.

## Why use it?

- **One agent, many machines.** Point tools at `local` or any `Host` alias from
  your ssh config (`devbox`, GPU box, CI host). Remotes attach over SSH on
  demand; you stay in a single conversation.
- **Real computer use.** Bash (including multi-turn sessions) and a surgical
  file editor on each host; search and browsing compose from the tools already
  on your machines (`rg`, `curl`, `lynx`, `ck` for semantic search, …) via bash.
- **Sessions you can resume.** Titles, scratchpads, PR/worktree links, and full
  conversation history live under `~/.myco/` — pick up later with `/resume`.
- **Nested agents for long work.** myco drives itself: start `myco` in a bash
  session to spin off focused agents so the main thread stays small and cheap.
- **Project guidance is injected.** `AGENTS.md` / `CLAUDE.md` in your launch
  directory is read at session start so the agent knows how _you_ work.
- **Coming later:** multiplayer (multiple humans in the same agent workspace).

## Install

```bash
cargo install myco
```

Needs stable Rust and `ssh`, `uv`, `bash`, `tmux`, `fzf` on `PATH`
(`git`, `gh`, `curl` recommended; `ck` — `cargo install ck-search` — for
semantic code search).

## Use

```bash
myco    # runs the default model from your config.toml; --model <key> to switch
myco -p "explain build.rs"        # print mode: one turn, answer on stdout, exit
git diff | myco -p "review this"  # piped stdin becomes context for the prompt
```

`-p/--print` runs one non-interactive turn: the answer streams to stdout
(raw, pipe-friendly), everything else prints to stderr, and the session is
saved like any other (`session=<id>` on stderr) — continue it with
`--resume <id>`. Bare `-p` takes the prompt from piped stdin.

Configure your models first: myco ships none built in. `~/.myco/config.toml`
holds a small catalog — `[gateways.*]` (protocol + base URL + auth, e.g.
Anthropic, xAI, OpenRouter, or a local server) and `[models.*]` (the keys you
pass to `--model`). The `auth` value is the token itself or a source such as
`{ source = "env", var_name = "XAI_API_KEY" }` (`.env` in the cwd is loaded
at startup) or `{ source = "file", path = "~/.secrets/x.token" }`. The exact variables are documented in the
[overview article](src/manual/articles/overview.md) — also available as
`myco --help overview` once installed. Set a default model with
`model = "<id>"` in `~/.myco/config.toml` (`--model` wins). Transcript
sections are colored when stdout is a TTY (`--color auto|always|never`;
`NO_COLOR` / `CLICOLOR_FORCE` honored), and prose is word-wrapped with light
markdown styling (`--wrap auto|off|COLS` caps the width at min(cap, terminal
width), default 80; resizes reflow the transcript at the next prompt; never
inside code blocks, never when piped).

Remotes just work: myco attaches lazily with `ssh <alias> myco --mode host`,
so a remote only needs your key in `ssh-agent` and `myco` on the PATH used by
non-interactive SSH. Runtime details: `myco --help overview`.

## Develop

```bash
cargo test --locked --lib
cargo run --locked --bin myco
bash scripts/install-pre-commit-hooks.sh   # optional: CI bar (fmt + clippy) pre-commit
```
