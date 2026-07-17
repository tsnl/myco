# AGENTS.md

Guidance for humans and coding agents working on **myco**.

## Premise

**myco** is a multi-host coding agent: one interactive process (model + harness +
conversation) drives tools on an always-on **local** host (in-process) and on
optional **remote** hosts (`ssh … myco --mode host` over NDJSON).

Primary goal: a **personal daily driver** that can replace Claude Code / Codex /
OpenCode-class workflows — long sessions you trust, real computer use, and one
conversation across machines. Multi-host execution and subagent orchestration are
the product wedge; cluster/GUI work must not outrank CLI trust and long-session
viability (`TODO.md`).

This is **not** an educational textbook repo (unlike resin/unit). Prefer clarity
and minimalism, but optimize for a tool people run every day, not for teaching
how agents work.

## Ranking

When goals conflict, rank them:

1. **Correctness & session integrity** — well-formed history, cancel behavior,
   no silent corruption on long runs.
2. **Simplicity** — minimum code that solves the real problem.
3. **Operability** — agents and humans can diagnose hosts, config, and failures
   (`manual`, `/hosts`, clear errors).
4. **Features / cleverness / premature generality** — last.

A plainer design that stays reliable beats a flexible one that hangs, desyncs
hosts, or lies about resume.

## Relentless cutting

- Delete before adding. Every type, dependency, flag, and error variant must
  earn its place against a real user path.
- No speculative abstraction, no config for futures that may never ship, no
  error taxonomies for impossible cases.
- No features beyond what was asked in the task at hand.
- Prefer one honest limitation (document it in `manual` / `TODO.md`) over a
  half-working generality.
- When in doubt: cut it, inline it, or simplify it.

## Writing for the reader

- **Docs describe the current codebase, not its history.** No migration essays,
  rename archaeology, or “how we got here.” Git history remembers the rest.
- **Module docs state role and invariants**, not a walkthrough of the file.
  Comments teach *why* and constraints; they never narrate what the next line
  does.
- **Terminology stays stable.** Prefer **host** (execution place for tools:
  `local` in-process or remote worker) over “machine/node/target” in code,
  config, tool schemas, and CLI. User-facing marketing may say “machines”;
  the domain word is still `host`.
- **Manual articles** (`src/manual/articles/`, also `myco --help <id>`) are the
  runtime contract for agents. Keep them accurate when behavior changes.
- **Tests are claims.** Prefer names that state the invariant
  (`cancel_during_slow_tool_records_cancelled_result`, not `test_cancel_1`).

## Architecture (current)

```
myco (interactive) / Agent
  └── Harness (routing, config, root-only services)
        ├── HostController "local"  → in-process HostWorker (always on)
        └── HostController "…"      → ssh … myco --mode host (lazy remote)
              └── standard tools: bash, editor, manual, lynx browser, text search
```

| Area | Role |
|------|------|
| `src/bin/myco.rs` | CLI: interactive REPL + `--mode host` worker |
| `src/session/` | Agent loop, events, session files under `~/.myco/session/` |
| `src/harness/` | Host pool, config (`[[remote_hosts]]`), SSH preflight, subagent service |
| `src/host/` | `HostController` + `HostWorker` + NDJSON protocol |
| `src/tool_services/` | Host tool implementations (`ToolService`) |
| `src/generative_model/` | Anthropic Messages + OpenAI Responses (xAI/Grok) backends |
| `src/text_search/` | Tantivy exact + Candle MiniLM semantic search (weights baked in) |
| `src/manual/` | Embedded runtime articles for the `manual` tool / `--help` |
| `src/prompts/` | System prompt fragments (worktrees, computer-use, coding norms) |
| `crates/myco-gui/` | Optional Yew UI — not on the critical CLI path |
| `tests/` | Integration tests (bash sessions, concurrent host tools, …) |

**Invariants worth protecting**

- **Local is always in-process** — never require a local `myco --mode host`
  subprocess for the default host.
- **Remotes are lazy** — connect on first tool use; soft-fail non-default hosts.
- **Standard tool catalog is the same on every host**; root-only tools
  (`session_meta`, `subagent`) are installed only on the in-process local worker.
- **Tool field `host`** defaults to `local`; bash sessions are **per host**
  (and per agent id).
- **Conversation resume ≠ restored bash/editor state** — document honesty;
  don’t fake rehydration.
- **Embedding weights** are compile-time (`build.rs` → `include_bytes!`). Ship
  platform-matched binaries; do not scp across glibc/arch boundaries.

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

### Feature work layout

New non-trivial features: dedicated git worktree + branch under the repo’s
`.myco/worktrees/{branch-slug}/` (see prompt fragment `worktrees`). Register the
worktree on the session with `session_meta` `add_link`. Skip worktrees only for
tiny one-liners or when the user asks to edit the current checkout.

## Develop

```bash
cargo build --locked
cargo test --locked --lib
cargo test --locked --test integration_test   # and other tests/ binaries as needed
cargo run --locked --bin myco
```

- First build may download MiniLM assets via `build.rs` (needs `curl` + network
  unless `MYCO_EMBED_OFFLINE=1` with pre-seeded `src/text_search/embed_weights/`).
- API credentials: see `README.md` / `myco --help overview` (Anthropic +
  xAI/OpenAI Responses env vars; `.env` loaded at startup).
- Runtime docs for agents: `manual` tool or `myco --help overview|cli|harness-ops`.

## What not to do

- Don’t rename the **host** domain (`host` tool field, `--mode host`,
  `/hosts`, `[[remote_hosts]]`, `src/host/`) for cosmetic synonyms without an
  explicit, breakage-aware migration plan.
- Don’t scp prebuilt `myco` binaries across mismatched OS/arch/libc; build on
  the target or use a matching asset (`harness-ops`).
- Don’t treat `/resume` as full workspace restore.
- Don’t expand `crates/myco-gui` scope at the expense of CLI trust work.
- Don’t commit large embed weight blobs; they stay gitignored and baked into
  the binary at compile time.
- Don’t edit `~/.myco/session/*.json` by hand from the agent — use
  `session_meta`.

## Backlog pointer

Living priorities and explicit rejects: **`TODO.md`**. Prefer P0 trust items
over shiny multiplayer/GUI unless the user steers otherwise.
