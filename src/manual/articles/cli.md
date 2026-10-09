# Command line

Run `myco` and open its printed launch URL. Each browser profile uses its own
`workspace/` for local tools and served files. The server owns sessions and tools;
the browser supplies conversation controls. Use `-p`
for a one-shot prompt or `--mode cli` for scrolling terminal chat. By default both run a
local session runner without starting an HTTP server. Explicit `--server` attaches
one-shot output to an existing server session. The launcher also provides
the internal SSH host worker. `myco-eval` remains a separate evaluation utility.

| Option | Meaning |
| --- | --- |
| `-p [PROMPT]`, `--print [PROMPT]` | Run one prompt and write completed responses to stdout; bare `-p` reads stdin |
| `--server URL` | Attach one-shot output to an existing `/profiles/NAME` service; requires a full `--resume` ID |
| `--detach` | With `--server -p`, return after acceptance and print a reconnect token |
| `--observe INSTANCE:REQUEST` | With `--server`, print retained output from a prior native turn |
| `--mode cli` | Scrolling terminal chat (`--mode interactive` is an alias) |
| `--port PORT` | Loopback HTTP port, default 8765; `0` chooses a free port |
| `--bind ADDR` | Loopback IP or `localhost`, default `127.0.0.1`; `::1` selects IPv6 |
| `--profile NAME` | Select profile, overriding `MYCO_PROFILE` (default `default`); server mode opens it first |
| `--config PATH` | Config path, overriding `MYCO_CONFIG` and the profile default |
| `--model KEY` | Default model from the config catalog |
| `--effort LEVEL` | Reasoning effort: `low`, `medium`, `high`, `max`; default `high` |
| `--resume ID` | Resume a saved session or unique prefix; in server mode, open it from the launch URL |
| `--auto-continue[=true\|false]` | CLI/one-shot only: persist automatic continuation for this session; omit to keep its current setting |
| `--debug-dump-api-requests` | Write provider request bodies to stderr |
| `--help [ARTICLE]` | Launcher help or the embedded manual article |
| `--version` | Package and build identity |
| `--mode host` | Internal SSH worker speaking NDJSON on stdin/stdout |

`--web [PORT]` and `--web-bind ADDR` remain aliases for `--port` and `--bind`.
Non-loopback addresses, including wildcard binds, are rejected. Use an SSH tunnel
for remote access; Myco has no HTTPS listener or certificate options. Tunneling,
request-origin checks, and the read-only `/files/` workspace routes are described in `browser`.
Host workers accept `--name` and `--max-image-base64-bytes`, supplied by the
server when it attaches a remote. The local host is always in-process.

Profiles put config, sessions, images, workspace, and manual under
`$MYCO_HOME/profiles/NAME/`; `MYCO_HOME` defaults to `~/.myco`. Local tool
processes inherit absolute `MYCO_HOME` and the selected `MYCO_PROFILE` even
when they change directories. In server mode, every existing profile has an
independent instance under `/profiles/NAME/`, sharing one loopback port. Launch
overrides apply only to the selected profile; other instances use their own config.
Local tools also receive `MYCO_SERVER_URL` for their instance's API. Child
sessions created at that URL share its profile. Remote workers need no model
credentials. CLI modes continue to run only the selected profile and inherit the
directory where you launched the command.

`.env` in the launch directory is loaded at startup. Configure at least one
model before starting; the overview describes the catalog. Browser controls,
attachments, archived sessions, and automation are documented in `browser`.

Ctrl-C stops the server, cancels active turns, and saves their recorded outcomes.
Closing a browser tab leaves its session running. Reopen session URLs directly
after restarting; no login is needed. Saved session URLs keep the same port;
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
piped text is treated literally. `-p` conflicts with explicit listen flags and
`--mode`.

Stdout contains assistant text, including narration between tool calls. Each
model response is buffered until it completes and validates, so interrupted
drafts cannot leak into scripts when generation retries. Thinking, tool output,
compactor output, and session metadata are excluded.
Diagnostics and `session=ID` go to stderr. Exit codes are 0 for success, 1 for
runtime/provider/output errors, 2 for input/config errors, and 130 for Ctrl-C.
Cancellation settles tool results and persists the session before exiting.

## Attach to an existing service

```bash
myco --server http://localhost:8765/profiles/default --resume FULL_SESSION_ID -p "Continue"
myco --server http://localhost:8765/profiles/default --resume FULL_SESSION_ID -p "Work" --detach
myco --server http://localhost:8765/profiles/default --resume FULL_SESSION_ID --observe INSTANCE:REQUEST
```

Use the full 32-character session ID from the browser URL. The session must be
idle with no queued input to accept a new native turn. This mode uses that
server's existing runner, model, tools, and profile workspace; `workspace=PATH`
is printed to stderr. Local config/profile environment variables are ignored,
and explicit config, profile, model, effort, and auto-continue overrides are
rejected. Explicit prompt image mentions are read from the client's working
directory and uploaded; piped text remains literal. Ordinary `-p` and terminal
chat retain their local working-directory and config behavior.

Client and server must have matching protocol, package version, and build
commit. Unavailable or incompatible services fail without starting a local
runner. URLs must include `/profiles/NAME` on a loopback HTTP origin; use SSH
forwarding to attach remotely. There is no service discovery, private-service
fallback, or interactive service client yet.

`run=INSTANCE:REQUEST` on stderr identifies an accepted or possibly accepted
turn. Transport retries reuse this identity. `--detach` exits after acceptance;
closing a client leaves the turn running. `--observe` reconnects to the same
instance and prints its committed assistant output from the beginning. Output is
published only after the response's history checkpoint succeeds; a later save
failure retains earlier committed output and reports an error. Automatic
reconnect within one invocation resumes after its last printed byte. Ctrl-C
requests cancellation of that turn and waits for its recorded outcome. Browser
follow-ups accepted during the run may join it under the usual queue policy.

Output receipts survive thread compaction but exist only for the current
service process. The 16 most recent native turns are retained per session;
an expired receipt returns an explicit error and is never re-executed. Each
receipt holds at most 4 MiB of assistant text; exceeding this cancels the turn
with an error directing you to saved history. Compact request fingerprints are
retained for up to 4096 native turns per session instance; after that, finish
active work and restart the service before accepting more. Saved conversation
history is independent of these receipt limits.

After service restart, an old token reports an unknown outcome and never
replays a request. Inspect the saved session and any external effects before
submitting new work. Reconnecting with `--observe` is the safe response to a
lost acceptance response; repeating `-p` creates a new request.

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
| `/auto-continue [on\|off]` | Show or change automatic continuation for the current session |
| `/quit`, `/exit` | Exit |

Assistant text streams to stdout; tool activity and diagnostics go to stderr.
A transient generation failure starts a fresh attempt with a retry diagnostic
separating it from the interrupted draft, which remains visible in the terminal.
Tool inputs show each top-level field separately. Long tool output is abbreviated;
complete observations remain in saved session history. Use the browser to browse
old messages, manage sessions, or switch models during a conversation.

Auto-continue is off by default. When enabled, a completed answer or exhausted
output cap receives an internal continuation prompt until the agent calls
`session_meta action=disable_auto_continue`. The prompt tells it to disable when
the task is complete or it needs user input. Enabling the mode does not itself
start work; submit a task normally. It survives compaction and restart, but does
not propagate to child sessions. Restarting never starts a saved task by itself.
Ctrl-C stops the current run, including automatic continuations; the setting
remains enabled for later submissions. Use `/auto-continue off` or
`--auto-continue=false` to disable it. While enabled, generation, persistence,
and automatic compaction failures retry after 1, 2, 4, then 5 seconds, staying at
5 seconds until recovery. Each retry reports its cause. Failed saves retain the
same live state and retry persistence before advancing, so accepted input and
completed tools are not repeated. Cancellation and disabling the mode interrupt
the retry wait. There is no retry-count, spending, or elapsed-time limit in this
mode. With auto-continue off, provider retries retain their configured bounds
and persistence errors return to the caller.

Both terminal modes share the browser's checkpoints, writer locks, attachment
limits, and automatic compaction/continuation. `--resume` uses the saved model
unless `--model` overrides it. A session already open in another process cannot
be written concurrently. Resume restores conversation history, not live shells or
editor state; terminal processes own their tools until exit.
