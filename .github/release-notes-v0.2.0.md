v0.2.0 is a feature release centered on making myco sessions durable and self-managing: agents can now remember things across sessions, compact a long conversation into a fresh one, and see their own context budget — plus a leaner startup and a round of correctness and performance fixes.

## Highlights

### Cross-session memory (#25)
A new root-only `memory` tool, shared by the supervisor and all subagents across sessions. Entries are immutable, UUID-keyed markdown records stored under `~/.myco/memory/` with RFC-822-style headers (Id, Date, Agent, Title) and the same two-hex fanout as the session store — concurrent sessions never take a lock and never rewrite a file in place; correcting a stale fact is append-then-delete, and a mistaken delete echoes the entry back so it can be re-appended. Verbs: write, `list` (compact id/timestamp/title index), `read` (the whole document in time order, or one entry by id — unique prefixes accepted), `search` (exact via Tantivy or semantic via MiniLM, entry-shaped hits), and `delete`.

### Session compaction (#12)
`/compact` opens a fresh successor session seeded with a summary of the current one plus a well-formed recent tail. Under the hood, a hidden compact worker explores the predecessor through the new root-only `session_history` tool (stats / range / expand / search / write_summary) and writes `{id}.summary.md`. The predecessor keeps its full history; the two sessions are linked via `successor_id` / `predecessor_id`.

### Sessions, context, and the transcript
- **Hidden sessions; subagents persist as sessions** (#9): sessions now carry a kind (user / subagent / compact). Subagent runs persist their history as first-class hidden sessions (session id == agent id) with the debug log as a sidecar. Default listings and bare `--resume` show only user sessions; get-by-id and `list(include_hidden)` still reach everything.
- **Context meter** (#10): every `USER` prompt header now shows used/max context tokens, plumbed from real provider usage events, with per-model window sizes.
- **Transcript sections** (#15): the assistant turn header is now `ASSISTANT` (was `RESPONSE`), and generate failures (context overflow, provider errors) render as headed `ERROR` blocks instead of a bare stderr line.
- **Config + colors** (#19): startup `Config` resolution, and transcript colors are on by default.

### Leaner startup (#28)
The startup banner is a single line (model, session, `/help` hint). The host status list no longer prints at startup — `/hosts` shows hosts and attach status on demand — and the ssh-agent preflight is silent when healthy. Problems print a colored `WARNING` transcript block before the first `USER` block instead.

## Changed
- **Remote host configuration moved from `config.toml` to `~/.ssh/config`** (#17). If you had host entries in `config.toml`, move them to your SSH config.
- Newline in the composer is documented as Alt-Enter / Ctrl-J; Shift-Enter is no longer advertised (#27).
- Tool descriptions now embed their enforced defaults and limits (timeouts, byte caps, widths) directly from the constants that implement them, with guard tests keeping them honest (#24). Bash tool output is capped at `max_bytes`, keeping head and tail (#20).
- Agent guardrails: a new always-on prompt fragment forbids force-merges and admin bypasses without explicit user approval — capability is not permission (#22); code comments ship only if they belong in the codebase (#29).

## Fixed
- **CI hang / suite-wide slowness** (#21): MiniLM embedding (and its tens-of-seconds model load in debug builds) no longer runs on tokio executor threads, which could wedge single-threaded runtimes mid-tool-call. Auto-indexing is now an explicit owner request: attaching a harness has zero background side effects, and each process entrypoint opts in deliberately. Local test times dropped from ~107s to 6.6s for the lib suite.
- Reliable `CancelToken` wakeups under suite load (#3).

## Internal
- `standard_tool_specs` is static; tool schemas come from associated functions (#23).
- README now carries crates.io and CI badges (#16); a publish skill documents the release process (#3).
