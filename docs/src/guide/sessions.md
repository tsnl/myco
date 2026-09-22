# Sessions and context

A **session** holds a stable ID, title, links, scratchpad, and ordered
**threads**. A thread is a linear conversation history. Only the latest thread
accepts new messages.

Live tools have a separate lifetime. A bash session is a process on a host;
its state belongs to the current session runtime, not to a saved message.

## Find and resume work

Ask the agent to set the session title with `session_meta`; the page heading
and tab title update with it. The home page lists visible sessions. Search
filters by title, model, and ID. Open links in separate tabs or use `/resume ID`.

```bash
myco --profile work --resume SESSION_ID
```

Use the profile in which the session was created. For searches across stored
message excerpts, scratchpads, and legacy transcript tails, the agent can use
`session_meta` with a `query`.

Resume restores conversation memory. After process exit it cannot restore
running shells, editor read stamps, or the remote filesystem as it was observed.
Ask the agent to check current state before continuing work that depends on it.

## Compact a long conversation

`/compact` creates a successor thread in the **same session**, containing a
summary and bounded recent context. The predecessor keeps its original messages
and tool results. Live shells and editor read stamps continue through
compaction, as do the title, links, and scratchpad.

With a per-model `auto_compact_at` threshold, the server compacts at a settled
boundary between tool rounds or after an answer and asks the agent to continue.
Long tool loops can compact repeatedly as context grows. A completed answer can
trigger at most one cycle per submission. Manual compaction and reopening a
saved session wait for input. Failure or ineffective compaction disables automatic
compaction until a manual compaction succeeds or another session opens.

The agent can inspect old threads with `session_history`, using `threads`,
`stats`, and `expand` actions. Compaction bounds active model context; it does
not delete the session's older threads from disk.

## Organize saved work

Click **Archive** on the home page to hide a session without deleting it. Choose
**Archived sessions** and click **Restore** to make one visible again. Opening
an archived session URL does not restore it automatically. Archiving does not
stop tools or archive children.

Archived sessions and their history, transcript, and summary files live under
`session/archived/` in the selected profile. Restore moves them back to the active
store. Startup moves existing archived sessions into that folder too, skipping
sessions open in another process. Ordinary browsing skips the archive folder,
so old archives do not slow down the active session list.

`/new` saves the current session and starts a fresh one with fresh tool ownership.
A running process owns its session's writer lock; use that process to change
the session while it is open.

## What is saved

Each session has a JSON document under the selected profile's `session/`
directory. Its threads hold messages, recorded tool outcomes, and human turn
timestamps. Historical readline and console sidecars remain readable and move
with archived sessions; the server writes no new terminal logs.

Use the browser and session tools to change metadata. The
[manual](../manual/overview.md#sessions-and-threads) describes the persisted
format and thread semantics in more detail.

## Image storage

Session images live in content-addressed files under the profile's `images/`
directory; histories contain SHA-256 references. Keep that directory with
session backups. Legacy inline images remain readable. Compaction and history
inspection avoid loading archived image payloads; an active request fails
clearly if a referenced file is missing or corrupt. Blobs are not automatically
garbage-collected.
