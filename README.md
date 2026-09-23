# `myco`

[![Crates.io](https://img.shields.io/crates/v/myco.svg)](https://crates.io/crates/myco)
[![CI](https://github.com/tsnl/myco/actions/workflows/ci.yml/badge.svg)](https://github.com/tsnl/myco/actions/workflows/ci.yml)

[User guide & manual](https://tsnl.github.io/myco/) ·
[Build with Myco](https://tsnl.github.io/myco/developers/) ·
[Rust API reference](https://tsnl.github.io/myco/developers/reference.html)

A minimalist coding agent that works across your machines over SSH.

> [!WARNING]
> Myco is pre-1.0 software under active development. Things are changing quickly;
> expect bugs and breaking changes as features, APIs, and configuration evolve.

Run `myco` on your laptop and open its browser UI. It edits files, runs shells, and searches code on
the local machine **and** on every concrete `Host` alias in your
`~/.ssh/config` — one session, many hosts, no setup beyond SSH itself.

![Myco reviewing its own code in a frosted browser UI against a rainy dusk sky](https://raw.githubusercontent.com/tsnl/myco/dddcfef1712641da1823240af9569e192f366730/docs/media/pr251/myco-review.png)

**Myco reviewing Myco.** A scripted review in the real browser UI, with simulated
rain. Captured from the [workspace-files preview](https://github.com/tsnl/myco/pull/251).

<details>
<summary>Watch the rain · 8-second loop</summary>

![Rain falling behind Myco's translucent code-review interface](https://raw.githubusercontent.com/tsnl/myco/dddcfef1712641da1823240af9569e192f366730/docs/media/pr251/myco-review-rain.gif)

</details>

## Why use it?

- **One agent, many machines.** Point tools at `local` or any `Host` alias from
  your ssh config (`devbox`, GPU box, CI host). Remotes attach over SSH on
  demand; you stay in a single conversation.
- **Real computer use.** Bash (including multi-turn sessions) and a surgical
  file editor on each host; search and browsing compose from the tools already
  on your machines (`rg`, `curl`, `lynx`, `ck` for semantic search, …) via bash.
- **Sessions you can resume.** Titles, scratchpads, PR/worktree links, and full
  conversation history live under `~/.myco/` — pick up later from the session browser.
- **Independent sessions.** Work in several browser tabs, or create hidden child
  sessions through the server API.
- **Project guidance is injected.** The nearest `AGENTS.md` / `CLAUDE.md` from
  the session's working directory through the repository root is read at session start.
- **Evaluate your actual tasks.** `myco-eval` turns session cutoffs into private,
  repeatable cases with independent graders. Compare models and prelude variants,
  or run the optional GEPA loop. See [task evals](src/manual/articles/evals.md).
- **Coming later:** multiplayer (multiple humans in the same agent workspace).

## Install

```bash
cargo install myco
```

Needs stable Rust and `ssh`, `uv`, `bash` on `PATH`
(`git`, `gh`, `curl` recommended; `ck` — `cargo install ck-search` — for
semantic code search).

## Use

```bash
myco                      # start HTTP on 127.0.0.1:8765
myco --profile research  # open research first; all profiles share this port
myco --resume SESSION_ID  # open a saved session from the launch URL
myco -p "Review the changes in this repository"
git diff | myco -p "Summarize this diff"
myco --mode cli          # scrolling terminal chat
```

Myco listens only on loopback. Open the printed URL directly. For remote access,
run Myco on the remote host and forward its port through SSH from your computer:

```bash
ssh -N -o ExitOnForwardFailure=yes -L 127.0.0.1:8766:127.0.0.1:8765 user@remote-host
```

Open the remote launch URL with its address changed to `http://127.0.0.1:8766`,
keeping the profile and session path. SSH provides authentication and encryption
between the computers; Myco needs no browser login, token, or cookie.
Myco does not serve HTTPS or accept non-loopback bind addresses.

The browser has collapsible tool blocks, a
floating input bar, Markdown/images, and independent sessions in separate tabs.
Refreshing or closing a tab keeps its current turn running. Ctrl-C in the
launching terminal stops the server and its sessions. See the
[browser manual](src/manual/articles/browser.md), also `myco --help browser`,
for controls, workspace files, and the HTTP API.

Each profile has its own URL, such as `/profiles/default/` or
`/profiles/research/`, with separate config, sessions, images, and tool runtimes.
The profile selector switches between existing profiles; `/profiles/` lists them.
Instances start when opened. Each uses `$MYCO_HOME/profiles/NAME/workspace/`
for local tools and served files (`MYCO_HOME` defaults to `~/.myco`); missing
workspace directories are created automatically. `--profile` chooses the initial
profile, and launch overrides such as `--config` and `--model` apply only to that
profile.

For scripts, `myco -p "prompt"` streams answer text to stdout, with diagnostics
and the saved session ID on stderr. Bare `-p` reads the prompt from stdin;
piped input precedes an explicit prompt as context. Add `--resume SESSION_ID`
to continue a saved conversation. `myco --mode cli` provides terminal chat with
line editing, tool activity, `/compact`, and Ctrl-C cancellation. Both use the
same sessions and automatic compaction as the browser; see `myco --help cli`.

Configure your models first: myco ships none built in. `~/.myco/profiles/default/config.toml`
holds a small catalog — `[gateways.*]` (protocol + base URL + auth, e.g.
Anthropic, xAI, OpenRouter, or a local server) and `[models.*]` (the keys you
pass to `--model`). The `auth` value is the token itself or a source such as
`{ source = "env", var_name = "XAI_API_KEY" }` (`.env` in the cwd is loaded
at startup) or `{ source = "file", path = "~/.secrets/x.token" }`. The exact variables are documented in the
[overview article](src/manual/articles/overview.md) — also available as
`myco --help overview` once installed. Set a default model with
`model = "<id>"` in `~/.myco/profiles/default/config.toml` (`--model` wins). The browser model selector changes the model between turns.

Remotes just work: myco attaches lazily with `ssh <alias> myco --mode host`,
so a remote only needs your key in `ssh-agent` and `myco` on the PATH used by
non-interactive SSH. Runtime details: `myco --help overview`.

## Develop

New to the codebase? Start with the [guided tour](TOUR.md).
The [Myco Book](https://tsnl.github.io/myco/) covers daily use and library reuse;
see [contributing to the docs](docs/src/contributing.md) to build it locally.

```bash
cargo test --locked --lib
cargo run --locked --bin myco
bash scripts/install-pre-commit-hooks.sh   # optional: CI bar (fmt + clippy) pre-commit
```

Browser regression tests run Chromium against an isolated profile, a local
scripted model, and real local tools. They require no API credentials:

```bash
cargo build --locked --bin myco
python3 -m venv /tmp/myco-browser-tests
/tmp/myco-browser-tests/bin/pip install -r scripts/browser-requirements.txt
/tmp/myco-browser-tests/bin/python -m playwright install --with-deps chromium
/tmp/myco-browser-tests/bin/python scripts/browser_tests.py
```

Use `--binary /path/to/myco` or `--browser /path/to/chromium` for existing builds.
Screenshots, server logs, and Playwright traces go to `target/browser-test-results`.

## Workspace

`myco-model` provides backend drivers and message types. `myco-agent` drives
headless execution using supplied tools and event sinks. The `myco` package
assembles sessions, host tools, the browser server, and terminal adapters. Workspace packages share a version and lockfile.
Run `cargo test --locked --workspace` to test all packages.

For scripted sessions and evals, `SessionRunner` supplies submission, durable
checkpoints, automatic compaction, and continuation with an injected model and
compactor. Server sessions use this same runner. Run the offline
[scripted session example](examples/scripted_session.rs):

```bash
cargo run --locked --example scripted_session
```

It uses real local tools in a temporary workspace, grades a file artifact,
compacts during execution, then reloads the saved session and verifies its restart
notice. See [scripted workflows](WORKFLOWS.md) for the API and recovery contract.

## Release

The Publish workflow bumps the shared workspace version and exact internal
dependency pins, verifies all three package archives, then publishes them in
dependency order. Run it with `dry_run: true` to check a release without writing
commits, tags, or registry versions. A real release requires successful CI on
the selected `main` commit and pushes the version commit and tag without
overriding branch protection.

Registry publication is not atomic. If it stops after uploading some packages,
keep the release commit and tag: inspect crates.io and publish only the missing
packages from that tag with `cargo publish -p <package> --locked`, in order
`myco-model`, `myco-agent`, `myco`. Finish the GitHub Release after all three
exist. Never roll back a version that has reached the registry.
