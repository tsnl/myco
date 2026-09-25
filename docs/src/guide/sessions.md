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

Automatic compaction is enabled for every model. The per-model `auto_compact_at`
fraction defaults to `1.0`, the full context window; a lower fraction leaves
more room for growth. The server checks the last known prompt size before a new
submission, between tool rounds, or after an answer and asks the agent to continue
after compaction. Switching models preserves a sizing estimate across restarts,
so the next message can compact before a request to a smaller model. The new
message is included in compaction; selecting a model alone does not start work.
Long tool loops can compact repeatedly as context grows. A completed answer can
trigger at most one cycle per submission. Manual compaction and reopening a
saved session wait for input. Failure or ineffective compaction disables automatic
threshold compaction until a manual compaction succeeds or another session opens.

A request-size rejection (HTTP 413 or the configured request byte cap) also
compacts and continues automatically, independently of the token threshold. This
recovery replaces recent images with text references to their saved originals
and preserves completed tool effects. Cancel interrupts it. If summarization
fails or the smaller request is still rejected, the run stops retrying and
removes the rejected submission from active context. Its original input and tool
results remain in saved threads, and the session accepts new input.

The agent can inspect old threads with `session_history`, using `threads`,
`stats`, and `expand` actions. Compaction bounds active model context; it does
not delete the session's older threads from disk.

## Organize saved work

Click **Archive** in the session toolbar or on the home page to hide a session
without deleting it. Archived sessions show an **Archived** badge in the session
header and browser list, alongside their activity status. Choose
**Archived sessions** to find them; **Restore** is available in the list and toolbar.
Restoring from the toolbar keeps your draft and stays in the session. Open tabs
update their badges when you archive, restore, or undo. Opening an archived
session URL does not restore it automatically. Archiving does not stop tools or
archive children.

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
