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

Startup banner prints model, session, config path, hosts, and default host.

### Models & env (quick)

- Default model: `grok-4.5-build` (config.toml `model` or `--model` to change; flag wins).
- Claude models need `ANTHROPIC_AUTH_TOKEN` or `ANTHROPIC_API_KEY` (optional
  `ANTHROPIC_BASE_URL`).
- Grok / OpenAI Responses need `XAI_API_KEY` or `OPENAI_API_KEY` (optional
  `XAI_API_BASE_URL` / `OPENAI_BASE_URL`; default base `https://api.x.ai/v1`).
- OpenRouter models (vendor-prefixed ids like `moonshotai/kimi-k3`,
  `deepseek/deepseek-v4-pro`, `google/gemini-3.5-flash`, `openai/gpt-5.6-sol`,
  `anthropic/claude-opus-4.8`) need `OPENROUTER_API_KEY` (optional
  `OPENROUTER_BASE_URL`). Bare `claude-*` ids stay on the native Anthropic
  backend; the `anthropic/` prefix opts into OpenRouter.
- `.env` in cwd is loaded at startup. Full tables: `myco --help overview`.
- Section headers / thinking / tool names are colored when stdout is a TTY;
  `--color auto|always|never` overrides (`NO_COLOR` / `CLICOLOR_FORCE` honored).

Thinking/reasoning is always requested (default effort=`high`). The UI shows a `Thinking: …`
summary inside a unified ASSISTANT section; it is stored in session history for resume/Ctrl-L
but stripped from provider requests. Generate failures (e.g. context overflow) open a headed
ERROR section (live only; not stored in session history).

Each live USER header is `USER <used>/<max>` (context tokens used / model window). `used` is 0
until a provider usage report arrives.
