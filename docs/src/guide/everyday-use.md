# Everyday use

Start Myco in the repository or directory you want to work on. State the
outcome, relevant constraints, and how you want the result verified. Tools
execute on `local` by default; name a remote host when the work belongs there.

## Give persistent project guidance

Myco discovers `AGENTS.md` / `CLAUDE.md` from your launch directory through the
repository root at session start. Keep project conventions and verification
commands there. The selected profile also has a workspace and a **prelude**:
durable entries appended to every agent system prompt. Ask the agent to use
the `prelude` tool for persistent personal guidance. Ordinary workspace files
remain notes the agent can read as needed.

## Work in the browser

| Action | Control |
| --- | --- |
| Submit a message | Enter or Send |
| Insert a newline | Shift-Enter or Alt-Enter |
| Queue a follow-up while busy | Enter or Queue |
| Cancel the running turn | Cancel |
| Inspect tool input and output | Expand its tool block |
| Inspect running tools and background shells | Activity |
| Switch configured models | Model selector between turns |
| Compact context | Compact or `/compact` |
| Open another session | New or a session link on the home page |

Set reasoning effort with `--effort` when launching the server. A tab can be
closed or refreshed while work continues. Open the session URL again to observe
its output. Ctrl-C in the launching terminal stops the server.

## Attach an image

Mention a local image as `@./screenshot.png` in your message. Supported
extensions are PNG, JPEG, GIF, and WebP; the file bytes determine its actual
format. Paths with spaces are not supported in mentions. Bad paths and
oversized images fail before the model call.

Image limits apply to the uploaded base64 payload, which is about 4/3 the file
size. The default per-image cap is 5 MiB, and attachments in one message have
a separate 20 MiB budget. Downscale large images before attaching them. The
agent can also call `view_image` on a selected host.

## Automate sessions

For a single task, run `myco -p "prompt"` or `git diff | myco -p "Review this"`.
The answer streams to stdout; diagnostics and the saved session ID go to stderr.
Use `--resume ID` to continue later. `myco --mode cli` provides a scrolling chat
with line editing, tool activity, and `/compact`. The
[command-line manual](../manual/cli.md) describes input, cancellation, and exit codes.

Use the loopback server API to create sessions, submit work, observe output,
compact, and cancel. Connect directly on localhost or through SSH forwarding;
no login or token is needed. A successful submission means accepted; wait for an
idle snapshot and inspect the result. The
[browser manual](../manual/browser.md#server-api-and-automation)
contains the request formats and a Python example.

Include `parent_session` to create a hidden child; add `fork: true` to seed it
with saved parent context. Each child has its own runner and tools while sharing
the server's profile. The [overview](../manual/overview.md#nested-agents-the-recipe)
describes context and ownership rules.
