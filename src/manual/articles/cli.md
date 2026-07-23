# User-facing CLI

You cannot press these yourself — tell the user which command to run.

| Command | Meaning |
|---------|---------|
| `/hosts` | Hosts (local in-process + remotes), tools, cmd, live/idle/error |
| `/session` | Current session metadata (title, links, scratchpad, path) |
| `/sessions` | Recent **visible** sessions (titles + link counts; hides subagent/compact) |
| `/resume [id]` | Load conversation memory (no id: session browser, see below) |
| `/new` | Fresh session (saves current) |
| `/title [text]` | Show or set session title |
| `/compact` | Compact into successor session (summary + recent tail) |
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
console-transcript tail — keyword (Tantivy) first, MiniLM semantic fallback
when keywords find nothing, over a one-shot in-RAM index (nothing persists;
the semantic pass pays a model load + corpus embed per call). fzf's own typing
filters display labels only. The `session_meta` tool's `list` action takes the
same `query` (plus `semantic` to force semantic ranking), so the agent can find
past sessions by content.

Startup banner is a small headed block (full-block rule, `MYCO`, model +
session, `/help` and newline hints). Startup preflight problems
print as one WARNING block after it — missing expected executables (`bash`,
`lynx`, `tmux`, `fzf`; `ssh`/`ssh-add`/`ssh-keygen` when remotes are
configured) and ssh-agent issues; hosts via `/hosts`.

### Models & config (quick)

- Models come from the `[gateways]` / `[models]` catalog in
  `~/.myco/config.toml` — **none are built in**. `--model <key>` picks a
  catalog key; default is config.toml `model`, or the sole configured entry.
- A gateway holds `protocol` (`anthropic-messages` | `openai-responses`),
  `base_url`, and `auth` — the token itself as a string, or a source table:
  `{ source = "env", var_name = "…" }` / `{ source = "file", path = "…" }` /
  `{ source = "none" }` (omit for no auth). A model names its gateway plus
  `api_id` (wire id) and a required `context_window`.
- Credentials that fail to look up error at model *use*, naming the source.
- `.env` in cwd is loaded at startup. Full format: `myco --help overview`.
- Section headers / thinking / tool names are colored when stdout is a TTY;
  `--color auto|always|never` overrides (`NO_COLOR` / `CLICOLOR_FORCE` honored).
- Prose (answer text, thinking) is word-wrapped and lightly markdown-styled
  when stdout is a TTY: `**bold**`, `*italic*`, `` `code` `` render with the
  delimiters *removed* (the styling conveys them), `#` headers keep their
  markers, and both `[text](url)` and a bare `http(s)://` URL become a
  clickable OSC 8 hyperlink (over `text`, or over the URL itself).
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

Each live USER header is `USER <used>/<max>` (context tokens used / model window). `used` is 0
until a provider usage report arrives. A `⚙`-prefixed line carries the input/output token
counts; below it, one `●`-prefixed line per still-running tool (live bash session on the
in-process local host) shows its command, uptime, and idle time; remote hosts are not queried
for this.

### Console mirror (`{id}.console`)

When stdout is a TTY, the interactive CLI mirrors everything it prints — the
startup banner, preflight WARNING, USER headers + submitted input, the streamed
ASSISTANT section, and live ERROR / `(cancelled)` notices — to a plain-text,
ANSI-free file beside the session JSON: `~/.myco/session/<shard>/<id>.console`
(shown as `console:` in `/session` and `session_meta` get). It is append-only
and accumulates across runs of the same session.

Read it (with your file tools) to see **exactly what the user saw**, in order —
including the live-only WARNING / ERROR sections that never reach the message
history. Useful for questions like "what was that warning at startup?" or "what
did the last error say?". Two limits: cursor repaints (input re-echo, resize
reflow) are not mirrored, so the file is the logical transcript, not a screen
snapshot; and meta-command output (`/hosts`, `/session`, …) is not mirrored.
