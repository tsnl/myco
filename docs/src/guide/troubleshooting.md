# Troubleshooting

Start with the error Myco reports, `myco --version`, and the selected profile.
The installed `myco --help overview` describes your exact build's runtime
contract; this website tracks `main`.

| Symptom | What to check |
| --- | --- |
| No models configured | Create `[gateways]` / `[models]` in the config path printed in the error. Myco ships no model catalog. |
| Unknown model | `--model` takes a catalog key, not necessarily the provider's wire ID. Set `api_id` separately. |
| Missing credential | Read the named environment variable or file source. Check the environment of the process launching Myco. |
| Provider rejects thinking | Set the model's `thinking` mode to one the endpoint supports, or `none`. |
| Remote is DOWN | Run the non-interactive SSH checks in [remote hosts](hosts.md); verify version, PATH, and authentication. |
| Session missing | Check the profile and archived-session filter. Nested and compaction worker sessions are hidden in ordinary listings. |
| Session is locked | Another running process owns the session. Continue there or use a different session. |
| Shell missing after resume | Conversation history persists; live tools do not survive process exit. |
| Unexpected submission | Most terminals send Shift-Enter as Enter. Use Alt-Enter or Ctrl-J. |

## Oversized input

Myco checks per-image and per-message attachment budgets before sending input.
Images already in the conversation also contribute to the whole request size.
A request rejected for size triggers recovery into a successor thread without
the last user turn; the predecessor preserves the rejected input and recorded
tool actions. Resend smaller input, compact, or start a new session. Recovery
does not undo tool side effects.

## Repeated failures or interruption

Only transient failures before response parts arrive are retried automatically.
An error after partial output is surfaced rather than replayed. Ctrl-C cancels
the turn, including retry waits; tools have their own cancellation cleanup.
Inspect current files and processes before resubmitting work with side effects.

## Recover useful evidence

`/session` shows the session's data and console paths. In an interactive TTY
run, the `.console` file records startup warnings and live errors as well as
the conversation. Print mode sends diagnostics to stderr and has no console
mirror. Read saved threads through `session_history` for structured history.

When reporting a bug, include the version, relevant config shape with credentials
removed, host and protocol involved, and the smallest reproduction. The
[harness operations manual](../manual/harness-ops.md#diagnosis-checklist) covers
host diagnosis in greater depth.
