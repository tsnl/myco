# User-facing CLI

You cannot press these yourself — tell the user which command to run.

| Command | Meaning |
|---------|---------|
| `/hosts` | Hosts (local in-process + remotes), tools, cmd, live/idle/error |
| `/session` | Current session metadata (title, links, scratchpad, path) |
| `/sessions` | Recent **visible** sessions (titles + link counts; hides subagent/compact) |
| `/resume [id]` | Load conversation memory |
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

Startup banner is one lean line: model and session. Startup preflight problems
print as one WARNING block after it — missing expected executables (`bash`,
`lynx`; `ssh`/`ssh-add`/`ssh-keygen` when remotes are configured) and ssh-agent
issues; hosts via `/hosts`.

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
  (`**bold**`, `*italic*`, `` `code` ``, `#` headers) when stdout is a TTY.
  `--wrap auto|off|COLS` sets a width *cap* (auto = 80); the effective width
  is min(cap, terminal width), re-measured every prompt — after a resize the
  dialog is cleared and reprinted at the new width (same as Ctrl-L). Styling
  is additive-only: delimiters stay visible, content bytes are never dropped.
  Fenced code blocks and 4-space-indented lines are never wrapped or styled.
  Piped output is never wrapped, so `myco | tee` stays byte-faithful.
- On submit, the typed input echo is replaced with a word-wrapped copy
  (wrap-only, exactly as typed — the edit line is the one region the CLI
  repaints). Replay (`/resume`, Ctrl-L) wraps user turns the same way.

Thinking/reasoning is always requested (default effort=`high`). The UI shows a `Thinking: …`
summary inside a unified ASSISTANT section; it is stored in session history for resume/Ctrl-L
but stripped from provider requests. Generate failures (e.g. context overflow) open a headed
ERROR section (live only; not stored in session history).

Each live USER header is `USER <used>/<max>` (context tokens used / model window). `used` is 0
until a provider usage report arrives.
