# Browser UI

Start the server, then open the URL it prints:

```bash
myco --web
myco --web 8766 --profile research
myco --web 0 --config /path/to/config.toml --resume <session-id>
myco --web --web-bind 0.0.0.0
```

`--web` uses port 8765 by default; `--web 0` picks a free port. The server binds
to `127.0.0.1` unless `--web-bind` names another address. Its launch URL signs
this browser into this server; keep that URL private. Assets and Markdown
rendering are bundled with myco, with no frontend build step or CDN. Stop the
server with Ctrl-C in the launching terminal.

`--web-bind 0.0.0.0` listens on every interface, so the UI answers any machine
that can route here, under whatever name they dial. The launch token is then the
only thing between them and these sessions — and a session is a shell on your
machine. There is no TLS and no user model: treat the URL as the credential,
prefer an SSH tunnel or a trusted network, and pick a hostname over an address
when handing the link out. Restarting the server mints a new token, which
invalidates the previous launch URL.

The home page lists your visible, unarchived sessions, with search, recent update
times, and running status. **New session** creates a session and opens its own
`/sessions/<id>` URL. Session links work with bookmarks, middle-click, and browser
tab groups. Click the myco name to return home. `--resume <id>` opens that session
directly from the launch URL.

Click **Archive** beside a session to hide it from the active list. Choose
**Archived sessions** above the list to find archived sessions and **Restore**
them. Archiving preserves history, the session URL, and running tools; an open
tab can continue its turn. It does not archive child sessions. A session held
by another myco process must be archived from that process or after it closes.
The list refreshes while the home page is visible; changes made outside this
server can take up to ten seconds to appear.

The browser uses the same model configuration, profile, session store, tools,
and compaction runner as the CLI. `--model`, `--effort`, `--profile`, `--config`,
and `--resume <id>` apply at startup. Bare `--resume` and print/host modes do
not combine with `--web`. Effort and configuration-file changes require
restarting the server.

## Conversation controls

The floating input bar stays pinned while the conversation scrolls. Enter sends;
Shift-Enter or Alt-Enter inserts a newline. Mention `@path/to/image.png` to attach
an image using the CLI's attachment rules and limits. Image paths with spaces
are not supported as attachments.

During a turn, **Queue** accepts follow-up messages in submission order. The
composer shows pending messages. They join the next model request after the
current tool batch finishes, alongside its recorded results; if no tools are
running, they are sent when the current response finishes. Up to 20 messages
can wait per session, shared across its tabs and preserved when a tab refreshes
or closes. **Cancel & send queued** stops the current run, records cancelled
tool results, and sends the pending messages with a fresh cancellation token.
Queues live in the running server and are not restored after a server restart.
A rejected submission keeps its draft.

The page heading and browser tab title follow the session title, including the
first-message title and agent renames during a running turn.

Each tool appears as soon as execution starts, with a truncated argument
preview in its collapsed header. Running calls are cyan; completed calls turn
green, and failures red. Expand a block to inspect its complete recorded input,
output, images, and outcome. Input shows each top-level key as a bold label above
its value. Strings retain their newlines without JSON quoting; nested objects and
arrays retain JSON structure. There is no browser verbose mode.

Tool calls show elapsed execution time to tenths of a second in their headers
and in the Activity drawer. Running timers update every 0.1 seconds and survive
page refreshes; completion, failure, or cancellation freezes the final duration.
Durations are observed by the running browser server and retained while viewing
the same thread. Saved history opened after a server restart has no timing data.

**Activity** opens a right-hand drawer with separate sections for active tool
calls and local background tasks, such as bash sessions that continue between
turns. The drawer starts closed; its button shows the current activity count.
Click an active call to close the drawer and open its block. Close the drawer
with its close button, Escape, or a click outside it. Background-task summaries
refresh every second without consuming tool output, and disappear when the task
ends. As in the CLI, background summaries cover the local host; active calls
include remote tools too.

Assistant responses render Markdown headings, lists, tables, task lists,
blockquote text, code blocks, links, and images. Text uses one font size;
headings use weight and underlines. Raw HTML is displayed as text. Markdown
images can reference HTTP(S) URLs, supported image data URLs, or local PNG,
JPEG, GIF, and WebP files. Relative file paths resolve from the server's launch
directory; absolute paths, `~/`, and `file://` URLs also work. Saved conversation
images resolve through the profile's image store.

USER and ASSISTANT headers have UTC timestamps on the next line. Recorded
messages use the turn's saved acceptance time; older turns without one show
`unknown`. The input box has no timestamp.

The model selector in the input bar lists the active configuration's model keys.
Choose a model between turns to use it for subsequent requests and compaction.
Switching preserves conversation history and live tool sessions, and records
the change in the session. An unavailable model leaves the current selection
unchanged and shows an error. The selector does not change the configured
startup default.

**New** opens a fresh session in the current tab; the previous session continues
running on the server. **Compact** creates a successor thread without changing
the session URL. These controls also accept `/new`, `/compact`, and `/resume <id>`
in the input; `/resume` navigates only the current tab. Other CLI slash commands
are not available in this frontend.

Automatic compaction is enabled per model with `auto_compact_at`, a fraction of
its context window (for example, `0.8`). Without this setting it is disabled.
When the threshold is reached, the runner settles pending tools, saves a summary
in a successor thread, and continues the task automatically. Queued follow-ups
join that continued context. Manual **Compact** finishes after creating the
thread and waits for your next message. Failed or ineffective automatic compaction
shows a warning and disables further attempts until manual compaction or a session
change; cancellation preserves the source thread.

## Running and resuming

One server can run several sessions concurrently. Each has its own runner, model
selection, cancellation, tool state, and writer lock. Tabs on different session
URLs work independently; tabs on the same URL observe the same run. Changing a
model or compacting is disabled during that session's turn. Another CLI or browser
server cannot write an opened session while this server holds its writer lock.
An unavailable or locked session shows an error without interrupting other tabs.

Refreshing, navigating away, or closing a tab does not cancel its active turn.
Reopening its session URL reconnects to its output and live tools. **Cancel run**
affects only that session; Ctrl-C in the server terminal cancels all active turns.
Opened sessions and their locks stay alive until the server stops. Request retries
do not submit the same action or create the same session twice. If submission
fails, the input draft remains available. Tabs share a live-output connection
through a browser SharedWorker so a tab group does not exhaust HTTP connections.

Restarting the server and resuming a saved session restores conversation history,
not old bash processes or editor state. Uncertain tool outcomes from an
interrupted process are recorded without rerunning their effects. Compaction
within a running session preserves live tools. After a server restart, open the
new launch URL once to sign in again, then reopen your saved session URLs. Keep
the same port to reuse bookmarks (`--web 0` chooses a new port each time).
