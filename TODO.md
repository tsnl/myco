# TODO

Living backlog. Priority tiers target **personal daily replacement** of Claude Code /
Codex / OpenCode / Grok-class agents / Pi. Multi-host brain/hands **and** multi-level
subagent orchestration (mycelial / self-similar agents) are the wedge — do not let
cluster/GUI work outrank CLI trust + long-session viability.

---

## Done / mostly done (do not re-open casually)

- Dual protocol drivers: Anthropic Messages + OpenAI Responses. Models are a config.toml catalog (`[gateways]`/`[models]`; auth = literal token or env/file source) — no built-in model list; any gateway (Anthropic, xAI, OpenRouter, local) via config
- Streaming generate + thinking; `EventSink` / `AgentEvent`; one rendering pipeline (`TuiProducer` drives terminal + console mirror; replay shares its layout helpers)
- Host pool: local + SSH `myco --mode host`, soft-fail non-default, `/hosts`
- Tools: `bash` (exec + sessions), `str_replace_based_edit_tool` (read-stamp)
- Concurrent tool uses per turn (`join_all`), including concurrent host-routed tools (pipelined NDJSON + concurrent host dispatch)
- Session message resume (`~/.myco/session/…`); readline history
- Session metadata v2: title, PR/worktree links, scratchpad; `session_meta` local tool;
  `/title`; list/get any session (breaking vs old v1 files — WIP, no migration)
- Session browser: bare `/resume` → fzf over sessions (console-mirror preview), as a tmux
  `display-popup` running `--mode session-browser` inside tmux, inline otherwise. `tmux` +
  `fzf` are expected on PATH (preflight warns). Deliberately composes with tmux/fzf
  instead of an in-house TUI. Content search: `--search` / `session_meta list query` rank
  sessions by plain keyword matching over title + first message + scratchpad + console
  tail (nothing indexed, nothing persisted).
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
- [ ] **Paste / newline submit** — chord newlines exist (Alt-Enter, Ctrl-J; Shift-Enter
      only on the Windows console — Unix terminals send it as plain Enter); no
      bracketed-paste handling. Terminal paste that injects bare newlines can still
      AcceptLine early (rustyline 15 default). Confirm on real paste; enable bracketed
      paste / filter if so.
- [ ] **Remote host-side cancel** — the local half is fixed: cancel gives the dispatch a
      grace window, so in-process tools run their process-group kill and return partial
      output (`cancel_during_local_exec_leaves_no_process_group_survivors`). Remaining gap
      is the protocol: no Cancel message (`Request`: Hello / ToolCall / AgentFinished), and
      the worker invents a fresh `CancelToken` per ToolCall — a cancelled remote tool runs
      to completion on the host. Need: Cancel over the pipe, wired to the worker-side token.
- [ ] **Host liveness / reconnect** — V1 is attach-time + next tool error. Soft reconnect,
      clearer mid-session DOWN UX (beyond `/hosts` at startup).
- [x] (REJECT) **Cold resume honesty** — sessions restore messages only (no bash sessions, no editor
      stamps). Banner or rehydrate hints so `/resume` does not feel broken.
  - Note: `/help` already documents “conversation memory only”; reject is for extra UX work.

### Tests that encode trust

- [x] History: generate-error-after-tools; resume-after-tools mid-turn (agent unit tests).
- [x] Composed local cancel (Agent → Harness → worker → bash) has an orphan-scan
      integration test. Add **remote host-routed** cancel coverage once the protocol
      Cancel message exists.
- [ ] Tool integration tests (bash sessions, editor read-stamp races) — still thin beyond
      existing bash/editor unit tests.

---

## P1 — Long sessions viable (named gaps + economy)

Without these, multi-hour coding sessions die or get silently dumb / expensive.

### Context lifecycle

- [x] **Compaction (manual)** — `/compact` runs a hidden compact-worker agent over the
      session (`session_history` tool), writes `{id}.summary.md`, and seeds a linked
      successor session with the summary + a well-formed recent tail. Ctrl-C cancels it.
- [ ] **Auto-compact** when approaching the context limit (threshold config).
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

- [x] **`AGENTS.md` support** — `AGENTS.md` (or `CLAUDE.md`) from the launch directory
      is appended to the system prompt at model build time, same lifecycle and cap as
      the soul. Re-read on cwd / project change remains open (prompt stability vs
      freshness trade-off).
- [ ] **Layered config** — `~/.myco` + repo `.myco/` / instruction files:
      model defaults, permissions, hooks paths, ignore globs — not only host pool.
- [ ] **Skills / skill packs**
  - Directory convention (user + project); load as prompt/procedures or slash-skills.
  - Import path for Claude/OpenCode-style skills so switching cost drops.
- [x] **Agent workspace** — free-form `~/.myco/workspace/` maintained with the
      ordinary tools; `workspace/soul/` holds maildir-style write-once soul
      snapshots, and the newest is appended verbatim to every agent system
      prompt at model build time. Replaced the earlier root-only `memory`
      tool (structured UUID-keyed entries + dedicated search) — (REJECT) that
      abstraction: plain files the agent organizes itself cover the same need.

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
- [x] **User multimodal (images)** — `@path` mentions in the REPL attach
      png/jpg/jpeg/gif/webp as `Content::Image` (data URL, ≤5 MiB); OpenAI
      Responses sends `input_image` parts. Non-image files: see **Rich attach**.
- [ ] **Provider file upload for images** — inline data-URL images are re-sent on
      *every* generate call (every tool-loop step), so one attached image adds its
      base64 to the wire for the rest of the session. When that hurts, fix inside
      the protocol drivers, not the abstraction: at request composition, hash the
      image, check an in-memory upload cache (hash → provider file id), upload on
      miss (Anthropic Files API, beta `files-api-2025-04-14`, file source block;
      OpenAI `/v1/files` purpose=vision, `input_image.file_id`), substitute the id
      in the outgoing request, fall back to inline base64 on failure. History and
      session files keep the data URL, so `/resume` across models keeps working.
      Per-gateway opt-in (`files_api = true`) — most OpenAI-compatible gateways
      (OpenRouter, local servers) and Anthropic-protocol proxies lack the
      endpoint. Known costs: uploaded files persist server-side until deleted
      (org storage cap), and upload paths need HTTP-mock tests.
- [ ] **`/model` mid-session** without restart. (`/effort` landed: always-on thinking, default high.)
- [ ] **Rich attach** — files/dirs/URLs as first-class message parts (not only “cat in bash”).

### Agent loop

- [x] (REJECTED) **Todo / task-list tool** — durable checklist for long jobs (Claude TodoWrite-shaped).
  - Adds complexity. Can be achieved with a `TODO.md` file.
- [x] **Subagents: multi-turn + background** — resolved by **dropping the
      subagent toolset**: nested agents are `myco` itself driven over a bash
      session (piped stdin/stdout; wrap/color auto-off; one prompt per line;
      the `USER n/m` header marks each turn boundary). Bash sessions already
      run in the background and support multi-turn `write`/`read`, so both
      halves come free. Nesting is **local-only by doctrine** (brains — config,
      keys, gateway network, session store — stay on the user's machine;
      remotes stay hands); children pass `--parent-session <id>` so their
      sessions are hidden and linked in the shared store.
- [x] **Context forking** — `--parent-session <id> --fork` seeds the child with
      the supervisor's saved conversation under a fresh hidden session id.
      Sessions checkpoint mid-turn at replayable boundaries (after the user
      message and each completed tool round), the current model key is stamped
      into the system prompt (identity-free otherwise) so supervisors launch
      same-model forks, and a same-model fork's first request re-reads the
      supervisor's cached prompt prefix instead of rebuilding context.
- [ ] **Remote nesting / gateway proxy** — running a whole agent *on* a remote
      (vs local brain + remote hands) would need config, keys, and gateway
      network there. If it is ever really needed, the principled fix is
      proxying model traffic through the supervisor's machine (myco as a local
      gateway for children). Not planned; nested agents run locally.
- [ ] **Background jobs** — long tests/builds without blocking the main turn; notify on done.
- [x] (REMOVED) **`lynx_tui_browser`** — the dedicated browser tool is gone; web
      browsing composes from bash (`lynx -dump`, `curl`, …) where installed, with
      per-system guidance in the workspace/soul. No separate web_fetch/web_search tools.
- [x] (REJECTED) **Servo / AccessKit browser backend** — superseded by the same
      doctrine: browsing is not a myco tool; anything heavier than bash-composed
      browsing belongs in a dedicated external program.
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

### Features

- [x] (REMOVED) **Indexed text search** — the in-binary search subsystem (Tantivy exact,
      Candle MiniLM semantic, notify watchers, compile-time weights via build.rs) is cut:
      it dominated dependency count, binary size, and build networking to serve a
      low-ranked need. Agents search with bash + `rg`; project guidance is injected at
      session start instead of indexed. Semantic search, if wanted, becomes a dedicated
      external CLI (long-running index daemon + query interface) — a separate project,
      not a myco subsystem.
