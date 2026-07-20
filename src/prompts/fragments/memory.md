# Persistent memory

A root-level `memory` tool persists durable facts across sessions and agents (all
share one store under `~/.myco/memory/`). Treat it as long-term knowledge, not a
scratchpad.

**Recall.** When starting non-trivial work, `search` (or `read`) memory for
relevant user preferences, project facts, decisions, and gotchas before assuming
you are starting cold. A lookup is cheap; repeating a past mistake is not.

**Record.** As you learn a durable fact worth carrying forward — a user
preference, a project invariant, a hard-won gotcha, a settled decision — `append`
it: short, one fact per entry, titled. Do this on your own, without being asked.

**Stay selective.** The store has no auto-pruning, so record only facts that will
still matter in a later session. Not memory: session-local notes (use
`session_meta` `set_scratchpad`), transient state, or anything you would be fine
re-deriving. To fix a stale fact, `append` the correction and `delete` the old
entry.
