# Browser UI

Start the local server, then open the URL it prints:

```bash
myco --web
myco --web 8766 --profile research
myco --web 0 --config /path/to/config.toml --resume <session-id>
```

`--web` uses port 8765 by default; `--web 0` picks a free port. The server binds
to `127.0.0.1`. Its launch URL signs this browser into this server; keep that
URL private. Assets and Markdown rendering are bundled with myco, with no
frontend build step or CDN. Stop the server with Ctrl-C in the launching terminal.

The browser uses the same model configuration, profile, session store, tools,
and compaction runner as the CLI. `--model`, `--effort`, `--profile`, `--config`,
and `--resume <id>` apply at startup. Bare `--resume` and print/host modes do
not combine with `--web`. Model and effort changes require restarting the server.

## Conversation controls

The floating input bar stays pinned while the conversation scrolls. Enter sends;
Shift-Enter or Alt-Enter inserts a newline. Mention `@path/to/image.png` to attach
an image using the CLI's attachment rules and limits. Image paths with spaces
are not supported as attachments.

Each tool appears as soon as execution starts, with a truncated JSON argument
preview in its collapsed header. Running calls are cyan; completed calls turn
green, and failures red. Expand a block to inspect its complete recorded input,
output, images, and outcome. All tools show their arguments as JSON, with
colored keys. There is no browser verbose mode.

The pinned input area lists active tool calls and local background tasks, such
as bash sessions that continue between turns. Click an active call to open its
block. Background-task summaries refresh every second without consuming tool
output, and disappear when the task ends. As in the CLI, background summaries
cover the local host; active calls include remote tools too.

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

The session picker opens a recent visible session. **New** starts a fresh session;
**Compact** creates a successor thread in the current session. These controls
also accept `/new`, `/compact`, and `/resume <id>` in the input. Other CLI slash
commands are not available in this frontend.

## Running and resuming

One server owns one active session at a time. Multiple tabs observe that same
session, and session controls are disabled during a turn. Another CLI or browser
server cannot write a session while this server holds its writer lock.

Refreshing or closing the browser does not cancel an active turn. Reopening the
launch URL reconnects to the running server and its current output. **Cancel run**
or Ctrl-C in the server terminal cancels the turn. Request retries do not submit
the same action twice. If submission fails, the input draft remains available.

Restarting the server and resuming a saved session restores conversation history,
not old bash processes or editor state. Uncertain tool outcomes from an
interrupted process are recorded without rerunning their effects. Compaction
within a running session preserves live tools; switching sessions gives the
new session its own runtime.
