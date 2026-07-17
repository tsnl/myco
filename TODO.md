# TODO

Living backlog. Priority tiers target **personal daily replacement** of Claude Code /
Codex / OpenCode / Grok-class agents / Pi. Multi-host brain/hands **and** multi-level
subagent orchestration (mycelial / self-similar agents) are the wedge — do not let
cluster/GUI work outrank CLI trust + long-session viability.

---

## Done / mostly done (do not re-open casually)

- Dual backends: Anthropic Messages + OpenAI Responses (Grok path)
- Streaming generate + thinking; `EventSink` / `AgentEvent` (CLI consumer is thin)
- Host pool: local + SSH `myco --mode host`, soft-fail non-default, `/hosts`
- Tools: `bash` (exec + sessions), `str_replace_based_edit_tool` (read-stamp), `subagent` (single-shot)
- Concurrent tool uses per turn (`join_all`), including concurrent host-routed tools (pipelined NDJSON + concurrent host dispatch)
- Session message resume (`~/.myco/session/…`); readline history
- Session metadata v2: title, PR/worktree links, scratchpad; `session_meta` local tool;
  `/title`; list/get any session (breaking vs old v1 files — WIP, no migration)
- Anthropic system-block prompt caching (`cache_control` on system text)
- Local turn cancel (Ctrl-C); synthetic cancelled tool results when tools already started
- `dyn GenerativeModel`; harness routing with injected `host` field

---

## P0 — Trust (blockers to relying on it)

Correctness and reliability. Feature parity is worthless if long sessions corrupt or hang.

### Bugs / reliability

- [x] **History integrity — tests + stale docs more than `take()` wipe**
  - Verified 2026-03-26: old `take()` / `last_interaction` wipe is **gone**. Current
    `Agent::interact` pushes user/assistant/tool_results before further work; cancel
    mid-tools records synthetic cancelled results and keeps a well-formed transcript
    (`cancel_during_generate_returns_cancelled`, `cancel_during_slow_tool_records_cancelled_result`).
  - Closed with unit tests:
    - `generate_error_after_tool_results_keeps_well_formed_history`
    - `generate_error_before_assistant_keeps_only_user`
    - `resume_after_tools_mid_turn_continues_cleanly`
  - CLI `/help` documents well-formed history on generate error / cancel (no stale caveat).
- [ ] **Paste / newline submit** — chord newlines exist (Alt/Shift-Enter, Ctrl-J); no
      bracketed-paste handling. Terminal paste that injects bare newlines can still
      AcceptLine early (rustyline 15 default). Confirm on real paste; enable bracketed
      paste / filter if so.
- [ ] **Host-side cancel** — confirmed gap, not just “agent turn only”:
  - `Harness::dispatch_tool_use`: `let _ = cancel; // V1: host calls are not mid-flight cancelled`
  - `host::serve_stdio` invents `CancelToken::new()` per `ToolCall` (never cancelled from agent)
  - Protocol has no Cancel message (`ClientMessage`: Hello / ToolCall / AgentFinished only)
  - In-process bash _can_ kill on cancel (unit test `exec_cancel_kills_runaway`); that path
    is unused for real host-routed tools today.
  - Need: cancel (or kill) over the host pipe + process-group kill for in-flight tools.
- [ ] **Host liveness / reconnect** — V1 is attach-time + next tool error. Soft reconnect,
      clearer mid-session DOWN UX (beyond `/hosts` at startup).
- [x] (REJECT) **Cold resume honesty** — sessions restore messages only (no bash sessions, no editor
      stamps). Banner or rehydrate hints so `/resume` does not feel broken.
  - Note: `/help` already documents “conversation memory only”; reject is for extra UX work.

### Tests that encode trust

- [x] History: generate-error-after-tools; resume-after-tools mid-turn (agent unit tests).
- [ ] Cancel already has agent-level unit coverage; add **host-routed** cancel once protocol exists.
- [ ] Tool integration tests (bash sessions, editor read-stamp races) — still thin beyond
      existing bash/editor unit tests.

---

## P1 — Long sessions viable (named gaps + economy)

Without these, multi-hour coding sessions die or get silently dumb / expensive.

### Context lifecycle

- [ ] **Compaction** — design: [`docs/compaction.md`](docs/compaction.md)
  - Manual `/compact` (and/or tool).
  - Auto-compact when approaching context limit (threshold config).
  - Preserve decisions, paths, todos; drop raw tool noise.
  - > I like Zed's approach: new session, "resume from previous session".
  - Plan: Zed-style successor session (archive full history → summary + tail); phased `/compact` then auto.
- [ ] **Token + cost tracking**
  - Plumb provider `usage` (input/output; Anthropic cache read/write) into `AgentEvent`
    and session totals.
  - Turn footer / `/session` (or similar): tokens this turn, session cumulative, rough cost.
- [x] (REJECT) **Caching strategy beyond system block**
  - History breakpoints / strategic `cache_control` so the growing prefix is not fully uncached.
  - Surface cache hit metrics so prompts can be tuned.
  - Rejection reason: very infrequently used in practice, just let the agent read the session history.
- [x] (REJECT) **Max-tokens continue** — `TurnEndReason::MaxTokens` exists; guided or automatic continue
      instead of a dead end. See `/compact`

### Project brain

- [ ] **`AGENTS.md` support** (also accept `CLAUDE.md` / common aliases as input).
  - Inject at session start; ideally re-read on cwd / project change.
- [ ] **Layered config** — `~/.myco` + repo `.myco/` / instruction files:
      model defaults, permissions, hooks paths, ignore globs — not only host pool.
- [ ] **Skills / skill packs**
  - Directory convention (user + project); load as prompt/procedures or slash-skills.
  - Import path for Claude/OpenCode-style skills so switching cost drops.
- [ ] **Optional cross-session memory** (`MEMORY.md`-style notes) — distinct from chat resume.

---

## P2 — Daily coding comfort

Muscle-memory gaps vs Claude Code / Codex / OpenCode.

### Control plane (default can stay open)

- [x] (REJECTED) **Permission modes** — e.g. ask / allowlist / autopilot; optional network/fs boundaries.
  - Wrong mechanism: better to use OS-level protection or bubblewrap sandboxing.
- [ ] **Dangerous-command gates** — `rm -rf`, `git push --force`, `sudo`, curl|sh, etc.
  - Also wrong mechanism, but scary enough that maybe we have defense in depth.
- [x] (REJECTED) **Plan / ask mode** — reason + propose without edits; or diff-then-apply.
  - Not needed.
- [ ] **Human approval hook** — block selected tools on `ToolStarted` until y/n.
- [ ] **OS-level / bubblewrap sandbox** (preferred over in-app permission modes; see REJECT above).

### Invocation surface

- [ ] **Headless / one-shot** — `myco -p "…"` / stdin / CI-friendly non-interactive mode.
- [ ] **User multimodal** — CLI path for images/files (`Content::Image` exists; CLI text-only;
      OpenAI image path thin).
- [ ] **`/model` mid-session** without restart. (`/effort` landed: always-on thinking, default high.)
- [ ] **Rich attach** — files/dirs/URLs as first-class message parts (not only “cat in bash”).

### Agent loop

- [x] (REJECTED) **Todo / task-list tool** — durable checklist for long jobs (Claude TodoWrite-shaped).
  - Adds complexity. Can be achieved with a `TODO.md` file.
- [ ] **Subagents: multi-turn + background**
  - Kick off N in background; optional multi-turn supervisor interaction
    (today: single-shot only; concurrent parent tools already work).
- [ ] **Background jobs** — long tests/builds without blocking the main turn; notify on done.
- [x] **`lynx_tui_browser`** — host tool via `lynx -dump` (search + simple browsing; link IDs).
  - Point at DDG Lite/HTML or Bing search URLs; follow numbered References.
  - Requires `lynx` on host PATH. No separate web_fetch/web_search tools.
- [ ] **Servo / AccessKit browser backend** (replace or complement Lynx)
  - Embed or sidecar Servo; dump **AccessKit** tree (roles/names/links) as primary
    agent text; optional screenshot (`Content::Image`).
  - Feature-gate / optional host capability (heavy; not every remote).
  - Spike candidates: official `servo` embedder API, or `servo-fetch` (JS + a11y +
    markdown + PNG). Prefer a11y linearization over Lynx `-dump` long-term.
  - Keep Lynx as the light default until Servo packaging is solid on macOS + Linux.
- [x] (REJECTED) **Apply-patch / diff review UX** — unified diff + accept/reject (esp. with plan mode).
  - Adds complexity. No real benefit.
- [x] (REJECTED) **Rewind / branch conversation** — undo last turn or fork session after a bad path.
  - Rarely used in practice. Adds complexity.

---

### Uncategorized bugs

- [x] Fix empty RESPONSE block if just tool use.
  - Always request thinking/reasoning (default effort=high; `/effort`, `--effort`).
    UI always shows summary-only `Thinking: …` inside RESPONSE (not a separate
    section). Thinking is stored in session history for resume and stripped from
    provider requests. OpenAI path ignores raw `reasoning_text` deltas.
- [x] Stray empty sessions on resume list
  - Do not persist sessions with zero messages; `/sessions` and resume pickers
    skip empty stubs. (`/new` also clears the display.)
- [x] Broken pipe error on interruption (Erlang model: cancel/I/O error
      abandons the host connection and kills the process; next call respawns.
      See tests/host_cancel_desync.rs)

---

### Features

- [x] Powerful text search and indexing (v1)
  - Host tools: `index_directory`, `indexed_exact_text_search`,
    `indexed_semantic_text_search` (Candle MiniLM), `drop_directory_index`
  - Exact: Tantivy (in-RAM); semantic: Candle MiniLM + cosine
    (weights: build.rs downloads safetensors → embed_weights/ gitignored + include_bytes; no ORT)
  - Auto-index: `.claude/skills`, `.myco/skills`, `SKILL.md` dirs, `AGENTS.md`/`CLAUDE.md`
  - Persistent watched roots + `notify` incremental updates; parent expand in place
  - [ ] tree-sitter structure index (next)
  - [ ] Persist / build-time skills embedding snapshot
