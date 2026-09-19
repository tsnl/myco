# Agent/session contract refactor

Worktree: `.myco/worktrees/feat-agent-state`, branch `feat/agent-state`,
based on `f1e99d8` (includes automatic-compaction continuation).

The objective is a scriptable agent with deterministic state transitions,
frontend-independent long-running session orchestration, and durable lifecycle
observations that reach the model without appearing as human transcript text.

Completion requires:

- A pure context/agent state machine drives the actual model/tool interpreter.
  Invalid or stale completions and malformed call/result boundaries are rejected;
  cancellation settles outstanding calls before another generation.
- One session runner owns submission, checkpoints, recovery, manual compaction,
  and automatic compaction/continuation. Both shell and headless/eval workflows
  use it, including compaction during long tool loops.
- Structured hidden system parts record initial context, resume, model changes,
  compaction, resource inventory, and resources unavailable after restart. They
  remain model-visible, survive persistence, and do not become human turns in
  transcript replay, search titles, or input recovery.
- Durable state records pending work before effects. Persistence failures stop
  further effects. Restart never silently replays possibly executed tools and
  never claims to restore live services. Format changes have explicit upgrades
  or rejection; predecessor threads retain their original observations.
- Deterministic eval examples/tests exercise the public runner with supplied
  models and tools, automatic compaction, restart, cancellation, and persistence
  failure. Runtime manual and architecture documentation match the shipped API.
- Workspace formatting, lint, and offline tests pass, followed by a requirement
  audit against the implemented behavior.

This file tracks unfinished work and is removed or replaced by current-code
documentation before the refactor is ready for review.

## Current implementation and remaining work

Implemented in the working tree:

- `myco-agent::AgentState` drives the actual async interpreter. Tests cover pure
  replay, stale/duplicate completions, busy context mutations, tool-result count,
  cancellation settlement, and bounded truncation. `run_with_outcome` reports
  the stop reason and measured usage.
- `Content::System { kind, text, data }` is model-visible in all three providers
  and absent from transcript replay. Session stamps, compaction summaries, and
  truncation/automatic-compaction continuations use it. Human-turn selection for
  rewind, timestamps, and compact tails excludes system-only messages.
- Session format 5 explicitly upgrades known runtime-text shapes from formats
  2–4 without rewriting source files on load. CLI subprocess tests exercise
  replay after restart and after automatic compaction.
- Fallible checkpoints persist pending generation/tool intent before dispatch,
  and results before further work. `Thread::pending_operation` is durable;
  runtime binding recovers unknown tool outcomes without replaying them. Session
  state is committed in memory only after save succeeds, and rebinding the same
  thread preserves newer observations in a live agent. Equal-length context
  changes and invalidated usage estimates are persisted. Compaction workers use
  the same fallible checkpoint path.

Still required (the goal is not complete):

1. Extract the session workflow from `ReplSession`: compactor injection,
   submission/continuation, error handling and manual/automatic compaction must
   serve CLI and scripted/headless callers. Add generation-boundary stepping so
   long tool loops can compact before reaching provider limits. Preserve usage,
   cancellation, and truncation accounting across a compaction.
2. Complete live recovery/stepping integration: a checkpoint failure during
   an outstanding operation currently leaves the agent deliberately stopped
   (`Busy`); restart safely reconciles it, but the session runner should expose
   an explicit way to resume from preserved live state after storage repair.
   Preserve pending-state and new-observation guarantees when moving the loop.
3. Record structured runtime lifecycle events: resume, model/effort changes,
   running service inventory, and services unavailable on a new runtime. Existing
   `running_tool_summaries` only observes in-process services and is display text;
   inspect the tool/host APIs before defining the durable resource record.
4. Add a public scripted session/eval example and tests covering the entire
   workflow (not just `Agent`), persistence failure, restart, and repeated auto
   compaction. Update architecture and runtime documentation for the final API.

Verification: `cargo test --locked --workspace` passed (561 tests, 5 live-provider
checks ignored) with local loopback allowed; the sandbox disallows HTTP stub
binding. Formatting and all-target Clippy passed. Later edits only clarified docs
and relaxed an exact callback-count assertion to verify checkpoint ordering.
No live model calls, pushes, or PRs. The next task step is item 1 above, in this
same worktree; do not start over or mark the goal complete.
