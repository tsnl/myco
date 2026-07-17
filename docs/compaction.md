# Compaction design

Status: **plan only** (this PR). Implementation follows in later PRs.

Related TODO (`TODO.md` → P1 Context lifecycle):

- Manual `/compact` (and/or tool)
- Auto-compact near context limit
- Preserve decisions, paths, todos; drop raw tool noise
- Preference: **Zed-style** — new session that “resumes from previous session”

This document is the contract for how we implement that without breaking session integrity.

---

## Goals

1. **Long sessions stay viable.** Multi-hour coding chats do not die on context overflow or become silently dumb because the model only sees the tail of an unsummarized firehose.
2. **Session integrity.** Compaction never leaves the live agent with a history the provider will reject (orphan `tool_use` without results, bad role order, etc.).
3. **Honesty.** Full transcript remains recoverable. Compaction is a deliberate context rewrite, not silent deletion of the only copy.
4. **Simplicity.** One clear model: *archive full history → open a successor session seeded with a resume summary + recent tail*. Prefer that over in-place mutation of the only session file.

Non-goals for v1:

- Perfect token accounting / cost UI (tracked separately under “Token + cost tracking”).
- Cross-session long-term memory (`MEMORY.md`) — distinct product surface.
- Provider-side “context editing” APIs as the primary mechanism.
- Automatic continue after `TurnEndReason::MaxTokens` (explicitly deferred to `/compact` in TODO).

---

## Current architecture (constraints)

Relevant pieces today:

| Piece | Role |
|-------|------|
| `Agent.history: Vec<Message>` | Live conversation the model sees each `generate` |
| `Session.messages` | Persisted copy under `~/.myco/session/{shard}/{id}.json` |
| `/resume`, `/new` | Switch sessions; history is conversation memory only |
| Backends (`anthropic`, `openai_responses`) | Stateless; full history resent every turn; thinking stripped on wire |
| No usage plumbing | We do **not** yet have reliable input-token counts on `AgentEvent` |

Invariants to protect:

- **Well-formed history** after tool loops (assistant `tool_use` always followed by matching `tool_results`) — already covered by agent tests; compaction must preserve or re-form this.
- **Conversation resume ≠ shell/editor state** — compaction does not claim to restore bash sessions.
- **Session file version** is breaking (`SESSION_FILE_VERSION`); schema additions should be additive + defaulted when possible.

---

## Product model: Zed-style successor session

### User-visible story

1. User (or auto policy) triggers compact.
2. Myco **finalizes the current session** (full messages stay on disk, unchanged).
3. Myco creates a **new session** whose early history is:
   - a synthetic user (or system-visible user) **resume block** summarizing the predecessor, plus
   - a short **verbatim tail** of recent messages so the model still has raw detail for the active task.
4. REPL switches to the new session (like `/new` + seeded history), prints a clear banner:
   - `compacted → new session=<id>  from=<old_id>  kept_tail=<n> messages`
5. Old session remains listable via `/sessions` / `/resume <old>`.

This matches the TODO note preference and keeps auditability: the old JSON is the ground truth.

### Why not in-place rewrite of one session?

In-place replace of `messages` with a summary is simpler to implement but:

- loses easy “what actually happened” without a second store,
- makes `/resume` ambiguous (which generation of history?),
- fights the mental model of sessions as append-mostly transcripts.

If we ever need in-place for GUI constraints, it can be a thin UI over the same archive+successor mechanism.

---

## Message shape after compact

Successor `messages` (conceptual):

```text
[0] UserMessage {
      // synthetic; clearly marked so UI/transcript can style it
      content: [Text: COMPACTION_RESUME_MARKDOWN]
    }
[1..] optional verbatim tail:
      last K *well-formed turns* from the predecessor
      (see “Tail selection”)
```

### Resume markdown (what the summary must capture)

The summarizer prompt should force a fixed outline, roughly:

1. **Goal / active task** — what the user is trying to do now.
2. **Decisions** — choices made and why (keep short).
3. **Key paths** — files, hosts, worktrees, branches, PR links (absolute paths).
4. **Todos / open work** — unfinished checklist.
5. **Constraints** — user rules, “don’t touch X”, test commands that matter.
6. **Recent outcome** — last meaningful result or failure (not raw tool dumps).
7. **Pointer** — `Predecessor session: <id>` so humans/agents can `/resume` it.

Drop by design: multi-KB tool stdout, repeated file reads, thinking blobs, failed exploratory dead-ends unless they constrain the next step.

### Tail selection

Keep the **minimum recent context** that is still useful raw:

- Default: last **2–4 user turns** (each user turn = user message through the following assistant end-turn, including any tool loops in between), **or** last ~N messages once well-formed boundaries are respected.
- Never split a tool loop: if the tail would start mid-`tool_use`/`tool_results`, extend backward to the owning user message (or drop that incomplete loop from the tail and rely on the summary).
- Cap tail by **estimated size** (chars/tokens heuristic) so a single huge tool result cannot dominate; oversized tool result bodies in the tail may be truncated with an explicit `(truncated for compact tail)` marker.

### UI / transcript

- `write_session_history` / Ctrl-L: render the resume block as a distinct paragraph (e.g. under USER with a `Compaction resume` lead-in), not as fake assistant prose.
- Live compact: print summary + “switched session” line; do not dump the entire old history again.

---

## Triggers

### Manual

| Surface | Behavior |
|---------|----------|
| `/compact` | Run compaction now; optional args later (`/compact hard` = smaller tail). |
| Optional tool | **Defer.** Root-only tools exist (`session_meta`), but slash command matches `/new`/`/resume` and avoids the model compacting itself mid-turn by accident. Revisit if headless/CI needs it. |

CLI wiring: same path as other meta-commands in `src/bin/myco.rs` (`parse_meta` / `handle_meta` / help + rustyline completion + `src/manual/articles/cli.md`).

### Automatic

Auto-compact only **between turns** (after a successful `interact` returns, before the next prompt), never mid-tool-loop.

Policy (v1 sketch):

1. Estimate context size for the next request (see “Sizing”).
2. If `estimate >= threshold * model_context_window`, run the same pipeline as `/compact`.
3. Emit a visible notice so the user knows history was rewritten into a successor session.
4. Config (additive, optional section; missing = defaults):

```toml
# ~/.myco/config.toml (proposed)
[compaction]
# 0.0–1.0 of model context window; default ~0.75
auto_threshold = 0.75
# Disable auto; /compact still works
auto = true
# Verbatim tail user-turns to keep (default 3)
tail_user_turns = 3
```

Do **not** block shipping manual `/compact` on config polish — hardcode defaults first, config second.

### Failure / edge cases

| Case | Behavior |
|------|----------|
| Empty / tiny history | No-op with message |
| Compact mid-cancelled turn | Only after history is well-formed (existing cancel rules already append synthetic tool results) |
| Summarizer generate fails | Abort; stay on old session; print error; no partial successor |
| Disk save fails | Abort switch; keep agent on old history |
| Nested subagent | Compaction is **root session only**; subagent logs stay as today |

---

## Pipeline (implementation steps)

Pure library logic should live under `src/session/` (e.g. `src/session/compact.rs`) so CLI is thin.

```text
compact_session(agent, active_session, model, opts) -> Result<CompactOutcome>
```

1. **Snapshot** `history = agent.history().to_vec()`; require non-empty.
2. **Persist predecessor** (`persist_messages`, force) so disk matches memory.
3. **Build summarizer input**
   - Either: call `GenerativeModel::generate` with a *side* message list (not the live agent history): system/user prompt = summarizer instructions + serialized transcript (possibly size-capped / tool-truncated).
   - Prefer a cheap/fast model when available later; v1 may reuse the session model with low effort if easy, else same model.
4. **Parse** model output as resume markdown (trust structured outline; light validation that required headings exist — warn, don’t hard-fail if slightly off).
5. **Select tail** with well-formed boundaries + size caps.
6. **Allocate successor** `Session::new(model)` with:
   - optional title derived from old title (`"{old} (continued)"` or keep same title — prefer **same title** + link metadata),
   - `links` / useful scratchpad **copied** from predecessor (user intent continuity),
   - `messages` = resume user message + tail.
7. **Link sessions** (schema — see below): predecessor records `successor_id`; successor records `predecessor_id`.
8. **Save** both session files.
9. **Install** successor into `ActiveSession` + `agent.set_history(successor.messages)`; reload readline history file for the new id (empty is fine).
10. **Return** ids + stats for the banner.

Summarizer calls must **not** append to the live agent history. Use a one-shot generate helper (extract or mirror `GenerateOutput::from_stream` usage without `Agent::interact`).

---

## Schema changes

Additive fields on `Session` (bump only if necessary; prefer `#[serde(default)]` on version 2):

```rust
/// Session this one was compacted from, if any.
#[serde(default, skip_serializing_if = "Option::is_none")]
pub predecessor_id: Option<String>,

/// Session created by compacting this one, if any.
#[serde(default, skip_serializing_if = "Option::is_none")]
pub successor_id: Option<String>,

/// When this session's live history was replaced by a successor compact.
#[serde(default, skip_serializing_if = "Option::is_none")]
pub compacted_at: Option<DateTime<Utc>>,
```

`/session` and `format_session_detail` should show predecessor/successor when set.

**Do not** delete predecessor `messages`. Optional later: `archived: true` for UI filtering only.

Copy `links` and `scratchpad` into the successor so worktree/PR continuity survives compact.

---

## Sizing (without full token tracking)

Ideal long-term: provider `usage` on each turn → session totals → accurate auto threshold (TODO item “Token + cost tracking”). That can land before or with auto-compact.

v1 heuristic (good enough for manual + crude auto):

- `estimate_tokens(messages) ≈ ceil(total_chars / 4)` over text/tool JSON that backends would send (exclude thinking, matching wire strip behavior), plus a constant for system prompt + tool specs.
- Per-model **context window table** on `Model` (conservative defaults), e.g. 200k for current Claude/Grok wire ids unless we know otherwise. Aliases like `[1m]` can map higher later.
- Reserve headroom for the next completion (`max_tokens` / thinking budget).

When usage plumbing exists, switch the threshold check to `last_input_tokens` (or EMA) and keep the char heuristic as fallback.

---

## Summarizer prompt (v1)

Keep the prompt in `src/prompts/` or next to `compact.rs` as a constant. Requirements:

- Output **only** the resume markdown (no tool calls; tools disabled for this generate).
- Follow the fixed outline above.
- Prefer paths, commands, and decisions over narrative.
- Hard length target (e.g. ≤ ~1500 tokens / ~6k chars) so the resume block itself stays small.
- If the transcript was truncated for the summarizer input, say so in the summary.

Transcript serialization for the summarizer: reuse or adapt `write_session_history` / a dedicated “compact dump” that truncates huge tool results (same helper as tail truncation).

---

## Testing plan

Unit tests (no network):

1. **Tail selection** — never starts mid tool pair; respects user-turn count; truncates giant tool bodies.
2. **Successor assembly** — resume + tail; links/scratchpad copied; predecessor/successor ids set.
3. **No-op** on empty history.
4. **Well-formedness** — property-style checks that successor history converts through existing `convert_messages` expectations (role merge / tool pairing).

Agent/CLI-level (mock model):

5. **Manual compact** with scripted summarizer output installs new history and leaves old session file intact with full messages + `successor_id`.
6. **Summarizer failure** leaves agent history unchanged.
7. **Auto threshold** fires only above estimate; disabled when `auto = false`.

Integration: optional later with live provider behind `#[ignore]` like other live suites.

---

## Phased delivery

### Phase 0 — this PR

- Design doc (`docs/compaction.md`)
- TODO pointer to the design
- No runtime behavior change

### Phase 1 — Manual `/compact` (MVP)

- `session/compact.rs`: tail selection, resume message assembly, session linking
- One-shot summarize via existing model stack (tools off)
- `/compact` meta-command + help + `cli.md`
- Unit tests for tail + assembly + mock compact
- Default fixed threshold constants (no config required)

**Success criteria:** On a long fixture history, `/compact` produces a new session the mock model can “continue,” old JSON still has full messages, history remains well-formed.

### Phase 2 — Auto-compact

- Char/token heuristic + `Model` context window defaults
- Post-turn check in REPL loop
- Optional `[compaction]` config keys
- Banner when auto fires

**Success criteria:** Synthetic oversized history triggers exactly one compact per crossing; no compact mid-tool-loop.

### Phase 3 — Better sizing & UX polish

- Plumb provider usage into events/session (shared with cost TODO)
- `/session` shows last usage + compact links
- Smarter tail (token-aware), optional `/compact hard`
- Consider agent-facing read of predecessor via `session_meta` only if needed

---

## Explicit non-approaches

| Approach | Why not (for myco v1) |
|----------|------------------------|
| Sliding window drop oldest messages | Loses decisions; model gets confused; no summary |
| Keep one session, replace messages in place only | Weak audit trail; worse `/resume` story |
| Summarize every tool result online | Constant latency/cost; complex; not requested |
| Rely on prompt caching only | Already rejected in TODO for product reasons |
| Mid-turn compact | Breaks tool_use pairing and cancel semantics |

---

## Open questions (resolve during Phase 1 if needed)

1. **Same title vs “continued” title** — recommendation: keep title, show predecessor in `/session`.
2. **Summarizer model** — same as session vs fixed small model; start same-model for fewer moving parts.
3. **Whether resume block is `UserMessage` or a dedicated `Message` variant** — prefer `UserMessage` + stable marker string to avoid schema/version churn; dedicated variant only if UI needs it badly.
4. **Copy readline `.history`?** — default no (new session empty readline); fine.

---

## File touch list (Phase 1 estimate)

- `src/session/compact.rs` (new) + `mod.rs` exports
- `src/session/mod.rs` — optional predecessor/successor fields + detail formatting
- `src/bin/myco.rs` — `/compact`, help, completion
- `src/manual/articles/cli.md` — document `/compact`
- `src/prompts/` or compact module — summarizer instructions
- `TODO.md` — checkboxes / link to this doc
- Tests adjacent to compact module + maybe one CLI parse test

Phase 2 adds: `src/harness/config.rs` / `FileConfig`, REPL post-turn hook, `Model` window table.

---

## Success definition (overall)

A user can work for hours: when context gets heavy they run `/compact` (or auto does), land in a fresh session that still knows the task, paths, and todos, can keep calling tools usefully, and can always `/resume` the predecessor for the full forensic transcript.
