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

## Work interactively

| Action | Control |
| --- | --- |
| Submit a message | Enter |
| Insert a newline | Alt-Enter or Ctrl-J |
| Cancel the line or running turn | Ctrl-C |
| Reprint the conversation | Ctrl-L at an empty prompt |
| Inspect tools and hosts | `/hosts` |
| List or switch configured models | `/model`, `/model KEY` |
| Change reasoning effort | `/effort low`, `medium`, `high`, or `max` |
| Save and quit | `/exit` or Ctrl-D |

Most terminals send Shift-Enter as ordinary Enter. Use the multiline controls
above when composing longer instructions. Output wraps to the terminal width
with an 80-column cap by default; `--wrap 100` changes the cap and `--wrap off`
disables wrapping. `--color auto|always|never` controls color.

## Attach an image

Mention a local image as `@./screenshot.png` in your message. Supported
extensions are PNG, JPEG, GIF, and WebP; the file bytes determine its actual
format. Paths with spaces are not supported in mentions. Bad paths and
oversized images fail before the model call.

Image limits apply to the uploaded base64 payload, which is about 4/3 the file
size. The default per-image cap is 5 MiB, and attachments in one message have
a separate 20 MiB budget. Downscale large images before attaching them. The
agent can also call `view_image` on a selected host.

## Use Myco in a pipeline

```bash
myco -p "Explain the repository layout"
git diff | myco -p "Review this diff for correctness"
printf '%s\n' "Summarize the public API" | myco -p
```

Print mode runs one turn and exits. Answer text streams to stdout; warnings,
errors, and `session=<id>` go to stderr. With an explicit prompt, piped stdin
becomes context. Without one, stdin is the prompt. Piped content is never
parsed for image attachments.

Print-mode sessions persist. Continue with `myco --resume ID`, or
`myco --resume ID -p "Follow up on the previous answer"`. Print mode does not
write a console mirror. It shares the interactive runner's configured automatic
compaction and recovery behavior.

## Nested work

Myco can launch another local `myco` process through bash. `--parent-session ID`
links the child's hidden session to its parent; adding `--fork` starts it from
the parent's saved context. Each child has its own conversation and host pool.
The selected profile is inherited. The
[runtime overview](../manual/overview.md) documents the agent-facing recipe.

For the full command table, see the [CLI manual](../manual/cli.md).
