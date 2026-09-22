# Command line

Run `myco` in your project directory and open its printed launch URL. The server
owns sessions and tools; the browser supplies conversation controls. Use `-p`
for a one-shot prompt or `--mode cli` for scrolling terminal chat. Both run a
local session runner without starting an HTTP server. The launcher also provides
the internal SSH host worker. `myco-eval` remains a separate evaluation utility.

| Option | Meaning |
| --- | --- |
| `-p [PROMPT]`, `--print [PROMPT]` | Run one prompt and stream answer text to stdout; bare `-p` reads stdin |
| `--mode cli` | Scrolling terminal chat (`--mode interactive` is an alias) |
| `--port PORT` | Loopback HTTP port, default 8765; `0` chooses a free port |
| `--bind ADDR` | Loopback IP or `localhost`, default `127.0.0.1`; `::1` selects IPv6 |
| `--profile NAME` | Select profile, overriding `MYCO_PROFILE` (default `default`) |
| `--config PATH` | Config path, overriding `MYCO_CONFIG` and the profile default |
| `--model KEY` | Default model from the config catalog |
| `--effort LEVEL` | Reasoning effort: `low`, `medium`, `high`, `max`; default `high` |
| `--resume ID` | Resume a saved session or unique prefix; in server mode, open it from the launch URL |
| `--debug-dump-api-requests` | Write provider request bodies to stderr |
| `--help [ARTICLE]` | Launcher help or the embedded manual article |
| `--version` | Package and build identity |
| `--mode host` | Internal SSH worker speaking NDJSON on stdin/stdout |

`--web [PORT]` and `--web-bind ADDR` remain aliases for `--port` and `--bind`.
Non-loopback addresses, including wildcard binds, are rejected. Use an SSH tunnel
for remote access; Myco has no HTTPS listener or certificate options. Tunneling,
authentication, and the read-only `/files/` workspace routes are described in `browser`.
Host workers accept `--name` and `--max-image-base64-bytes`, supplied by the
server when it attaches a remote. The local host is always in-process.

Profiles put config, sessions, images, workspace, and manual under
`$MYCO_HOME/profiles/NAME/`; `MYCO_HOME` defaults to `~/.myco`. Local tool
processes inherit absolute `MYCO_HOME` and the selected `MYCO_PROFILE` even
when they change directories. Child sessions created by the server API share
that server's profile. Remote workers need no model credentials.

`.env` in the launch directory is loaded at startup. Configure at least one
model before starting; the overview describes the catalog. Browser controls,
attachments, archived sessions, and automation are documented in `browser`.

Ctrl-C stops the server, cancels active turns, and saves their recorded outcomes.
Closing a browser tab leaves its session running. Restarting requires opening
the new launch URL to authenticate again. Saved session URLs keep the same port;
use a fixed port for bookmarks.

## One-shot prompts

```bash
myco -p "Review this repository"
git diff | myco -p "Review this diff"
cat task.txt | myco -p
myco -p "Continue the review" --resume SESSION_ID
```

With an explicit prompt, piped stdin is prepended as context. Empty input fails
with exit code 2. Only explicit prompt text expands `@./image.png` attachments;
piped text is treated literally. `-p` conflicts with explicit server flags and
`--mode`.

Stdout contains streamed assistant text, including narration between tool calls.
Thinking, tool output, compactor output, and session metadata are excluded.
Diagnostics and `session=ID` go to stderr. Exit codes are 0 for success, 1 for
runtime/provider/output errors, 2 for input/config errors, and 130 for Ctrl-C.
Cancellation settles tool results and persists the session before exiting.

## Terminal chat

```bash
myco --mode cli
myco --mode cli --resume SESSION_ID
myco --mode cli --model MODEL_KEY
```

Enter submits; Alt-Enter inserts a newline. The line editor supports navigation
and in-memory input history. Ctrl-C clears the input or cancels the running turn;
Ctrl-D exits at an empty prompt. Piped input submits one line per turn. A failed
or cancelled turn returns to the prompt.

| Command | Action |
| --- | --- |
| `/help` | Show terminal controls |
| `/session` | Show the session ID and model |
| `/compact` | Compact into a new thread in this session; return to the prompt |
| `/quit`, `/exit` | Exit |

Assistant text goes to stdout; tool activity and diagnostics go to stderr.
Tool inputs show each top-level field separately. Long tool output is abbreviated;
complete observations remain in saved session history. Use the browser to browse
old messages, manage sessions, or switch models during a conversation.

Both terminal modes share the browser's checkpoints, writer locks, attachment
limits, and automatic compaction/continuation. `--resume` uses the saved model
unless `--model` overrides it. A session already open in another process cannot
be written concurrently. Resume restores conversation history, not live shells or
editor state; terminal processes own their tools until exit.
