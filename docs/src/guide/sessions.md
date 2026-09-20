# Sessions and context

A **session** holds a stable ID, title, links, scratchpad, and ordered
**threads**. A thread is a linear conversation history. Only the latest thread
accepts new messages.

Live tools have a separate lifetime. A bash session is a process on a host;
its state belongs to the current session runtime, not to a saved message.

## Find and resume work

Use `/title Fix parser errors` to label the current session and `/session`
to inspect its paths and metadata. `/sessions` lists recent visible sessions.
`/resume` opens the picker, and `/resume ID` accepts an ID or unique prefix.
From the shell, `myco --resume` opens the most recent eligible session.

```bash
myco --profile work --resume SESSION_ID
myco --profile work --mode session-browser --search "parser"
```

Use the profile in which the session was created. Search matches titles,
first messages, scratchpads, and transcript tails. The picker requires `fzf`;
inside tmux it opens in a popup. Explicit IDs work without it.

Resume restores conversation memory. After process exit it cannot restore
running shells, editor read stamps, or the remote filesystem as it was observed.
Ask the agent to check current state before continuing work that depends on it.

## Compact a long conversation

`/compact` creates a successor thread in the **same session**, containing a
summary and bounded recent context. The predecessor keeps its original messages
and tool results. Live shells and editor read stamps continue through
compaction, as do the title, links, and scratchpad.

With `auto_compact_at` configured, the interactive CLI can compact after a
successful turn reaches the threshold, then ask the agent to continue the
pending task. Each user submission can trigger one such cycle. Manual
`/compact` waits for your next input; reopening a saved session also waits.
Automatic compaction is disabled after a failure until another session opens.

The agent can inspect old threads with `session_history`, using `threads`,
`stats`, and `expand` actions. Compaction bounds active model context; it does
not delete the session's older threads from disk.

## Organize saved work

`/archive [ID]` hides a session from ordinary browsing without deleting it.
`/sessions archived` lists archived sessions and `/restore ID` makes one visible
again. Explicitly resuming an archived ID does not automatically restore it.
Archiving does not stop tools or archive child sessions.

`/new` saves the current session and starts a fresh one with fresh tool ownership.
A running process owns its session's writer lock; use that process to change
the session while it is open.

## What is saved

Under the selected profile's `session/` directory, each session has a JSON
document and readline history. Interactive TTY runs also append an ANSI-free
`.console` transcript. This mirror includes notices that are absent from model
history, but omits cursor repaints. It is a display log, not an execution trace.

Use the CLI and session tools to change metadata. The
[manual](../manual/overview.md#sessions-and-threads) describes the persisted
format and thread semantics in more detail.
