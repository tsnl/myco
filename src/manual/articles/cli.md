# User-facing CLI

`myco --profile NAME` selects a profile; otherwise `MYCO_PROFILE` applies, then
`default`. Config, sessions, workspace, and manual live under
`$MYCO_HOME/profiles/NAME/` (`MYCO_HOME` defaults to `~/.myco`). Local nested
agents inherit the selection. Use the same profile to resume a session.

You cannot press these yourself — tell the user which command to run.

| Command | Meaning |
|---------|---------|
| `/hosts` | Hosts (local in-process + remotes), tools, cmd, live/idle/error |
| `/session` | Current session metadata (title, links, scratchpad, path) |
| `/sessions` | Recent **visible** sessions (titles + link counts; hides subagent/compact) |
| `/sessions archived` | List archived user sessions |
| `/archive [id]` | Archive this session or a saved session by id/prefix |
| `/restore [id]` | Restore this session or a saved session by id/prefix |
| `/resume [id]` | Load conversation memory (no id: session browser, see below) |
| `/new` | Fresh session (saves current) |
| `/title [text]` | Show or set session title |
| `/compact` | Create a successor thread in this session (summary + recent tail) |
| `/verbose` | Toggle full tool details and clear/reprint the active conversation |
| `/model [key]` | List configured models and current selection, or switch at the prompt |
| `/effort [level]` | Show or set reasoning effort (`low\|medium\|high\|max`) |
| `/help` | Full help |
| Alt-Enter / Ctrl-J | Multiline input |
| Enter | Submit |
| Ctrl-C | Cancel line at prompt; cancel in-flight turn while running |
| Ctrl-L | Clear scrollback and reprint the conversation (empty prompt only) |
| Ctrl-D / `/exit` | Save and quit |

Shift-Enter does **not** insert a newline in most terminals: they transmit it as
plain Enter, so it submits the message. If the user reports this, tell them to
use Alt-Enter or Ctrl-J instead. (Shift-Enter works only on the Windows console,
which reports key modifiers.)

USER and ASSISTANT banners put their UTC timestamp on the line immediately below
the header. A live USER prompt shows when it opened; the ASSISTANT timestamp is
the persisted acceptance time. Replay puts that saved acceptance time below both
headers. Older human turns show `unknown`; their creation time is not substituted.
Runtime context (session
identity, compaction summaries, and automatic continuation instructions) is
stored as system parts. The model receives their text, but transcript replay
omits them and they do not count as human submissions.

Archiving hides a session from ordinary listings and bare resume. It preserves
all threads, metadata, search, and links; it does not cancel a run or close tools.
Children and legacy predecessor/successor sessions keep their own archive status.
Use `/sessions archived`, `myco --mode session-browser --archived`, or
`session_meta list archive_filter=archived` to find archived sessions, then
`/restore id` to show one again. Explicit `/resume id` can open an archived
session without changing its status. Another running process owns its session's
writer lock; archive or restore that session in its own CLI or `session_meta`.

Mentioning `@<path>` in a message attaches that file as image input (extensions
png/jpg/jpeg/gif/webp pick out the mention, but the media type is read from the
file's magic number, so a mislabeled file is caught here rather than by the
provider; whitespace-delimited, so paths with spaces are unsupported; `~/`
expands). The text is sent exactly as typed — the `@` mentions stay in it — and
a `[N image(s) attached]` note prints directly under the wrapped input,
identical live and in replay (`/resume`, Ctrl-L); the image bytes are never
printed. A bad path or a file that is not really an image opens an ERROR
section before the model is called; nothing is silently dropped.

Saved images use content-addressed sidecar files in the profile's `images/`
directory. Session format 6 keeps `Content::Image.source` as a
`myco-image:sha256:<hash>:<media-type>` reference; inline data URLs and formats
2–5 remain readable and are externalized on save. History reads and compaction
do not load image bytes. `session_history` displays the sidecar path when an
image is present. The model request resolves only its input images and checks
their SHA-256 hashes; a missing or corrupt active image stops the request with
an error. Copy `images/` together with `session/` when moving a profile. There
is no automatic image deletion, since archived threads may still use a blob.

Size limits are measured on the **base64 payload uploaded**, which is 4/3 of the
file on disk: per image, the running model's `max_image_base64_bytes` (config.toml
`[models.KEY]`, default 5 MiB — so ~3.75 MiB on disk; `view_image` enforces the
same cap on every host, and an image over it fails that tool use); and 20 MiB of
attachments per message, which is myco's own budget and does not vary by model.

`/model key` uses the catalog loaded at startup and keeps history, live shells,
and editor read stamps. It updates retries, context/output policies, the
compaction model, and attachment limits. Invalid keys or unavailable credentials
leave the current selection intact. The switch applies to this process; set
`--model key` or the config default when restarting. Smaller context windows may
need `/compact`. Host image readers retain their startup ceiling; the current
model's lower cap is also enforced on returned images. Restart to raise that
host ceiling. Existing history is retained without resizing its images.
A model configured above 20 MiB still gets its full per-image cap through
`view_image` — only batching many `@path` mentions into one message is held to
the message budget. myco does not re-encode images — an oversized file is
rejected with both sizes named, so downscale it and resubmit.

Images stay in the conversation, so a session accumulates them and can cross the
provider's **whole-request** cap (Anthropic: 32 MB) turns after the attachment
was sent. myco checks each composed request against a 30 MiB ceiling before
uploading it, and maps a provider's own size rejection — a 413, or a 400 whose
body names or describes the size — to the same failure. That failure
is not retryable — every later turn resends the same history — so myco **rewinds
the last user turn out of the active context** and says so in the ERROR
section. This includes Anthropic's many-image dimension limit. Recovery creates
a successor thread; the predecessor keeps the rejected input and all recorded
tool actions, readable with `session_history`. If saving fails, the original
context stays active. The session continues; re-send the message with a smaller image, or
`/compact` (or `/new`) to shed history.

### Session browser

Bare `/resume` opens an fzf picker over visible sessions (fuzzy search on
titles/metadata, transcript preview from the `{id}.console` mirror). Inside
tmux it runs as a `display-popup` executing `myco --mode session-browser`;
outside tmux fzf runs in the current terminal. `tmux` and `fzf` are expected
on PATH (the startup preflight warns when missing); `/resume <id|prefix>`
always works without them.

`myco --mode session-browser` also runs standalone: it prints the picked
session id to stdout (`--out FILE` writes it to a file instead; empty/absent
file means cancelled), e.g. `myco --resume "$(myco --mode session-browser)"`.

Content search: `--search QUERY` ranks sessions by match instead of recency.
The corpus per session is title, first user message, scratchpad, and the
console-transcript tail — plain case-insensitive keyword matching, rebuilt per
call (nothing persists, nothing is indexed). fzf's own typing filters display
labels only. The `session_meta` tool's `list` action takes the same `query`,
so the agent can find past sessions by content.

Startup banner is a small headed block (full-block rule, `MYCO`, model +
session, `/help` and newline hints). Startup preflight problems
print as one WARNING block after it — missing expected executables (`bash`,
`tmux`, `fzf`; `ssh`/`ssh-add`/`ssh-keygen` when remotes are
configured) and ssh-agent issues; hosts via `/hosts`.

Every block the REPL prints is headed. Meta-command output (`/help`,
`/hosts`, `/session`, `/sessions`, `/effort`, `/title`, the `/resume`
acknowledgement) opens a `MYCO` section — thin rule + bold header, the
banner family's mid-screen voice. Command failures (an unknown `/command`,
`/resume` or `/compact` errors) open an ERROR section; an unknown command is
never sent to the model. `/new` clears the screen and opens the fresh
session under the same startup banner.

### Print mode (`myco -p`)

`myco -p "PROMPT"` runs one non-interactive turn and exits: answer text
streams to stdout verbatim (no sections, colors, or wrap — thinking and tool
activity are not rendered); everything else — preflight WARNING,
`session=<id>`, errors — goes to stderr, so stdout is pipe-clean. Bare `-p`
takes the prompt from piped stdin; with both, stdin is prepended as context
(`git diff | myco -p "review this"`). The session persists like an
interactive run — continue with `--resume <id>` interactively, or
`-p … --resume <id>` for one more non-interactive turn. `--parent-session` /
`--fork` compose with `-p` for one-shot nested agents (the session is created
hidden and parented, exactly as in the live-session recipe). `@path` image
mentions in the PROMPT argument attach images exactly as in the REPL
(attachment note on stderr); piped stdin is data and is never parsed for
attachments. No console mirror is written in print mode.
Print mode uses the same automatic-compaction policy as the REPL; a long tool loop
can compact and continue before the process exits.

### Models & config (quick)

- Models come from the `[gateways]` / `[models]` catalog in
  `~/.myco/profiles/default/config.toml` — **none are built in**. `--model <key>` picks a
  catalog key; default is config.toml `model`, or the sole configured entry.
- A gateway holds `protocol` (`anthropic-messages` | `openai-responses` |
  `openai-completions`),
  `base_url`, and `auth` — the token itself as a string, or a source table:
  `{ source = "env", var_name = "…" }` / `{ source = "file", path = "…" }` /
  `{ source = "none" }` (omit for no auth). A model names its gateway plus
  `api_id` (wire id) and a required `context_window`.
- Credentials that fail to look up error at model *use*, naming the source.
- Everything else about the config is checked at startup: an empty catalog is
  an error carrying an example to paste, unknown keys are rejected, and a
  config path you named with `--config` / `$MYCO_CONFIG` must exist. Config
  errors name the file they came from.
- `.env` in cwd is loaded at startup. Full format: `myco --help overview`.
- Section headers / thinking / tool names are colored when stdout is a TTY;
  `--color auto|always|never` overrides (`NO_COLOR` / `CLICOLOR_FORCE` honored).
- Every tool displays its arguments as formatted JSON, with cyan keys and dim
  values. This includes bash `command` and `stdin` fields. Strings retain their
  JSON escapes for newlines, tabs, quotes, and control characters. Long lines,
  paths, and unbroken arguments wrap at the box width; `↪` marks a display
  continuation. With prose wrapping off or piped output, boxes use 72 columns.
  Live output, history replay, and the console mirror use the same layout.
  `/verbose` expands the full inputs and recorded text output.
- Prose (answer text, thinking) is word-wrapped and lightly markdown-styled
  when stdout is a TTY: `**bold**`, `*italic*`, `` `code` `` render with the
  delimiters *removed* (the styling conveys them), `#` headers keep their
  markers, and both `[text](url)` and a bare `http(s)://` URL become a
  clickable OSC 8 hyperlink (over `text`, or over the URL itself), shown
  underlined blue browser-style — terminals render OSC 8 spans unstyled, so
  the decoration is what makes links visible before hover.
  `--wrap auto|off|COLS` sets a width *cap* (auto = 80); the effective width
  is min(cap, terminal width), re-measured every prompt — after a resize the
  dialog is cleared and reprinted at the new width (same as Ctrl-L). Fenced
  code blocks and 4-space-indented lines are never wrapped or styled.
  With styling off (`--color never`, `NO_COLOR`, non-TTY) rendering is exact
  identity — delimiters and link syntax print verbatim — so `myco | tee` and
  the console mirror stay byte-faithful.
- On submit, the typed input echo is replaced with a word-wrapped copy
  (wrap-only, exactly as typed — the edit line is the one region the CLI
  repaints). Replay (`/resume`, Ctrl-L) wraps user turns the same way.
- `TERM=dumb` disables the cursor repaints (input re-echo, resize reflow)
  while plain wrapping stays on. Piped output gets neither: colors can be
  forced into a pipe (`--color always` — escapes are strippable downstream),
  wrap cannot (hard newlines would permanently alter the content).

Thinking/reasoning is always requested (default effort=`high`). The UI shows a `Thinking: …`
summary inside a unified ASSISTANT section; it is stored in session history for resume/Ctrl-L
but stripped from provider requests. Generate failures (e.g. context overflow) open a headed
ERROR section (live only; not stored in session history).

Tool inputs and outcomes share a rounded box with the tool name in its top
border. Concurrent tools use titled separators inside the same box. Arguments
appear immediately with cyan keys and dim values; tool failures and nonzero process
exits are red. Frames use the wrap width, or 72 columns when prose wrapping is
off; `↪` marks continued display lines. The console mirror and replay use the
same layout without adding terminal escapes to the mirror.

By default, each box shows its first five content lines, with `… /verbose` when
more is hidden. Factual status and failure lines remain visible. `/verbose`
toggles full tool inputs and recorded text output, clears the terminal, and
reprints the active thread. Toggle again to return to previews. The choice also
applies to subsequent output, resume, and Ctrl-L until the CLI exits; it starts
off on each launch. Replay does not rerun tools or append duplicate output to the
console mirror. Stored conversation data and model context are unaffected.
Images appear as placeholders, and tool-side output limits still apply to what
was recorded. In pipes or `TERM=dumb`, replay appends without cursor escapes.

Tool failures and process outcomes appear inside the box as short `↳` lines identifying the
tool, host, and command or session. They come from the tool result, independently
of the assistant's answer, and appear again on replay. Text output shares the
box's preview budget; bash process exits show their exit code
or signal. A running shell's status does not imply that each command sent to its
stdin succeeded. Cancellation reports partial results or unknown effects.

Manual and automatic compaction open a **COMPACTING** system section showing
the session, thread, and cancellation hint; elapsed-time updates stay within it.
`/compact` creates a successor thread in the current session. It clears the screen
(scrollback included) and prints a **COMPACTED** banner listing the session, the new thread,
its predecessor, the retained message count, and the summary path. Older threads stay in
the session file and can be read through `session_history` with `thread_id`. Live shells
and editor read stamps continue across compaction. The console mirror keeps the whole
run, and Ctrl-L reprints the active thread's visible retained context. Read the
summary through `session_history` or the summary path shown in the banner.

The compaction worker can only read its assigned thread and write one summary
(at most 8,000 characters). It has no shell, editor, prelude, or host access.
Compaction stops after `compaction_max_requests` model requests (including retries;
default 64). Set this positive integer at the top level of `config.toml` to change
the budget for manual and automatic compaction. There is no compaction duration
limit; the CLI reports elapsed time every 10 seconds. Ctrl-C cancels it. A failed or
cancelled compaction leaves the current thread intact.

Each live USER header is `USER <used>/<max> (<pct>%)` — context tokens used / model window,
compact-formatted (`63.8k/200k`). `used` is 0 until a provider usage report arrives, and `?`
(no percentage) on sessions resumed from before usage tracking. Once a turn has finished, a
`⚙`-prefixed line shows its usage — `⚙ last turn: input 63.8k (58k cached) · output 1.4k` —
where input is the prompt of the turn's final request (≈ the live context) and output is
summed across all of the turn's requests (one per tool round-trip). Below it,
`●`-prefixed lines show this session's local bash sessions, with command, uptime, and
time since the last interaction. A session stays visible until its launched process
exits and both captured output streams close. If descendants keep a stream open after
the process exits, the line says `process exited, output still open`; use bash `read`
for later output or `close` to stop the process group.
The tool result's `exit_code` and `exit_signal` describe the launched process;
`status: exited` means its output streams have also closed. While descendants keep
output open, `read` waits for their next output, idle gap, or timeout as usual.

These lines refresh at each prompt. They are not an inventory of OS processes: remote
hosts are not queried, one-shot `exec` commands are not retained, and descendants that
close or redirect both output streams cannot be tracked after the launched process exits.
Use bash `list` with a `host` to inspect that host's sessions. For managed background
work, use bash `start` and keep the program in the foreground of that session.
Separately, hidden runtime records observe local and connected remote tool resources
at settled execution boundaries. After restart they tell the model which recorded
handles are unavailable. They do not appear in these status lines or transcript replay.

### Console mirror (`{id}.console`)

When stdout is a TTY, the interactive CLI mirrors everything it prints — the
startup banner, preflight WARNING, USER headers + submitted input, the streamed
ASSISTANT section, the `/compact` progress line + COMPACTED banner, live
ERROR / `(cancelled)` notices, and meta-command output (`/hosts`, `/session`,
…) — to a plain-text, ANSI-free file beside the session JSON:
`~/.myco/profiles/default/session/<shard>/<id>.console` (shown as `console:` in `/session`
and `session_meta` get). It is append-only and accumulates across runs of the
same session.

Read it (with your file tools) to see **exactly what the user saw**, in order —
including the live-only WARNING / ERROR sections that never reach the message
history. Useful for questions like "what was that warning at startup?" or "what
did the last error say?". One limit: cursor repaints (input re-echo, resize
reflow, the Ctrl-L / `/resume` transcript reprint) are not mirrored, so the
file is the logical transcript, not a screen snapshot.
