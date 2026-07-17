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

- [ ] **Compaction**
  - Manual `/compact` (and/or tool).
  - Auto-compact when approaching context limit (threshold config).
  - Preserve decisions, paths, todos; drop raw tool noise.
  - > I like Zed's approach: new session, "resume from previous session".
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

- [x] Fix empty ASSISTANT block if just tool use.
  - Always request thinking/reasoning (default effort=high; `/effort`, `--effort`).
    UI always shows summary-only `Thinking: …` inside ASSISTANT (not a separate
    section). Thinking is stored in session history for resume and stripped from
    provider requests. OpenAI path ignores raw `reasoning_text` deltas.
- [x] Stray empty sessions on resume list
  - Do not persist sessions with zero messages; `/sessions` and resume pickers
    skip empty stubs. (`/new` also clears the display.)
- [x] Broken pipe error on interruption (Erlang model: cancel/I/O error
      abandons the host connection and kills the process; next call respawns.
      See tests/host_cancel_desync.rs)

---

## myco-gui — web frontend + shared core (PR #13 and its stack)

Goal: a web UI that looks like **a slightly more polished CLI**, sharing the
runtime with the CLI so the two never diverge. **The CLI stays first-class**: it
must remain usable over a dumb pipe (`tee`, a COM port, a monochrome printer) —
print speed notwithstanding. The GUI is a second *presentation*, never a second
*runtime*.

### Architecture principle — one core, thin presentations

The seam is already in the codebase: **`EventSink`** separates *what happened*
(runtime events) from *how it is shown* (CLI stdout vs. SSE broadcast). Extend
that seam to the whole app.

- **`src/application` (Phase 0, do FIRST):** a transport-agnostic `Application`
  owning `Harness`, model/effort/debug config, the session registry
  (create/open/list live+saved), `build_model`, and the **single** system-prompt
  source. Exposes `run_turn(session, input, sink, cancel)` (sink injected) and
  command handlers that return **data structs**, not printed strings.
- **CLI (`src/bin/myco.rs`):** thin adapter = readline loop + `CliEventSink` +
  char-rules rendering of the returned structs.
- **Server (`src/server/*`):** thin adapter = rocket/SSE + `BroadcastSink` +
  JSON of the same structs.
- **Litmus test for every fn:** does it decide *what the app does* or *how it is
  displayed*? Core gets the *what*; CLI/GUI keep the *how*. Never unify
  presentation — the CLI's char rules and the GUI's `<hr>`/color divs are meant
  to differ. Unify **logic + data structs only**.

Known drift to kill in Phase 0 (already two copies):
- `SYSTEM_PROMPT_PROLOGUE` (defined in both `server/state.rs` and `bin/myco.rs`).
- `build_model` (backend config, effort, tool specs, prompt assembly).
- Session lifecycle (new/resume/model-parse/history-load).
- Session/host presentation DTOs (`server::messages_view` vs CLI
  `format_session_detail` / `print_host_status`): core returns `SessionDetail` /
  `HostStatus`; CLI formats to text, GUI serializes to JSON.

Phase 0 success criteria:
- `SYSTEM_PROMPT_PROLOGUE` and `build_model` each exist **once**.
- CLI behavior byte-identical (existing `transcript.rs` tests still pass).
- Server behavior unchanged.
- Lands as its **own PR under #13**; the GUI rewrite sits on top and consumes the
  clean `Application`.

### Deferred (described, NOT a Phase-0 assumption): "CLI as HTTP client"

Tempting end state (LSP / Docker / Jupyter-kernel style): the server is the only
runtime; both CLI and GUI are clients. **Do not require this for the CLI.** It
regresses the explicit robustness goal (a pure-HTTP CLI depends on a socket + SSE
+ a healthy daemon; in-process depends on none of that) and front-loads daemon
lifecycle complexity (stale servers, port conflicts, version skew, zombies) we
do not need yet.

The correct shape — enabled by Phase 0, opt-in later, **in-process default**
(like `git`: local ops in-process, *also* speaks a wire protocol to remotes):

```
myco                 → in-process Application   (default: fast, robust, no daemon)
myco --connect URL   → HTTP client to a remote/hosted server   (future)
myco --mode server   → serves Application over HTTP+SSE
```

This unlocks the hosted/container/SSH-remote north star without making the local
CLI pay a daemon tax. Same Phase-0 work either way; the client driver is a later
phase, not a foundation.

### Shipping phases (GUI)

- [x] **Phase 0 — `src/application` extraction** (own PR under #13). See above.
- [x] **Phase 1 — CLI-parity web UI (SHIP THIS FIRST).** Scrap the current GUI.
      Match the CLI experience almost exactly, with two deltas:
  - Horizontal rules are real `<hr/>`-style elements spanning the **full dialog
    width**, not printed box-drawing chars fixed at 72 cols.
  - Use **color** to delineate outputs (USER / ASSISTANT / ERROR / thinking /
    tools).
  - Sections/headers mirror the CLI: `USER` (double rule), `ASSISTANT` (thin
    rule), `ERROR`; `Thinking: …` paragraphs; `name(<pretty json>)` tool paras;
    `USER used/max` context tokens.
  - Scope: **no** session management beyond `/session`; **no** autocomplete.
    Just render the transcript as a slightly-more-polished CLI. Then ship.
- [ ] **Phase 2 — Markdown in output.** Input stays raw text; after Enter, past
      messages render as Markdown. Do **not** vary header font sizes — keep `#`
      prefixes as the header indicator, but always render headers **bold**.
      Respect `*`/`_` italics and hyperlinks.
- [ ] **Phase 3 — Interactivity.** Click-to-expand tool use + subagent
      invocations; per-stream bash blocks (exit code / stdout / stderr); subagent
      links (`myco-session://<id>` recognized + rendered as a hyperlink to the
      subagent's session). Tab autocomplete + popups.
- [ ] **Phase 4 — Session management + multitasking.** Multi-session sidebar,
      concurrent sessions, session **search** (index all session JSON) replacing
      the top-left title, **archive** sessions (+ button). Split user "notes"
      (collapsible right-side panel, half-width) from the agent scratchpad.
      (Search + archive may each be their own PR below #13; decide ordering.)

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
