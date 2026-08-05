# Driving myco (API, GUI, nested agents)

myco has one session runtime and two frontends. `myco` (default) is the
interactive CLI: it runs the runtime in-process, and the user's prompt stays
live while you work — their input queues as your next turns. `myco --mode
serve` is the multiplayer web experiment: the same runtime behind an HTTP API
on `http://127.0.0.1:7773/api` (`--port` to change) with the web GUI on top.
Everything below applies to serve mode; in CLI mode the same operations are
in-process (nested agents included — the `subagent` tool works identically in
both).

## The API

In serve mode the base URL is exported as `$MYCO_API` in every local bash
session (absent in CLI mode — use the `subagent` tool there).

| Endpoint | Meaning |
|----------|---------|
| `GET /sessions?include_archived=` | Recent visible sessions (id, title, model, live/busy, archived). Archived ones are excluded unless asked for |
| `POST /sessions` | Create: `{model?, parent_session?, fork?}` → summary with `id` |
| `GET /sessions/<id>` | Metadata + full transcript entries |
| `PATCH /sessions/<id>` | `{title?, archived?}` — set session metadata |
| `POST /sessions/<id>/messages` | `{text}` — queue one user turn (input queues while a turn runs) |
| `GET /sessions/<id>/poll?since=N` | `busy`, entries from `N`, `total` (next `since`), `last_error` |
| `GET /sessions/<id>/events` | SSE: turn start/finish/fail, text/thinking deltas, tool starts, compaction |
| `POST /sessions/<id>/cancel` | Cancel the in-flight turn (queued input survives) |
| `POST /sessions/<id>/compact` | Summarize into a **successor session (new id)** |
| `DELETE /sessions/<id>/live` | Retire the live agent task (session stays on disk, resumable) |
| `GET /models` | Catalog keys + default |

Opening a session (posting, subscribing to events) makes it **live**: an agent
task, its host pool, and the single-writer session lock exist server-side until
retired. A session open in another myco process answers 409.

## Turns, errors, compaction

- One `{text}` = one turn. Messages posted while busy queue and run in order.
- A turn that fails (provider error, cancel) produces no reply; the reason is
  on `poll.last_error` and the `turn_failed` SSE event until the next turn
  starts. Transient provider errors are retried once automatically. A request
  too large for the provider — a 413, or a 400 whose body names or describes
  the size — drops the offending message so the session can continue (the
  error says so).
- Compaction (manual via the endpoint, automatic when a turn ends with the
  context ~85% full) summarizes into a successor with a **new id** — follow
  the `compacted` SSE event to it. Predecessor and successor stay linked on
  disk.

## Messages

Mentioning `@<path>` in a message attaches that file as image input (png/jpg/
jpeg/gif/webp mentions; media type read from magic numbers; whitespace-
delimited paths; `~/` expands). Paths resolve on the **server's** machine. Per
image, the running model's `max_image_base64_bytes` applies (default 5 MiB on
the base64 payload — 4/3 of the file on disk).

## Nested agents

Use the **`subagent` tool** — not the API — to delegate: one call runs one full
turn of a hidden child session and returns its final answer plus `session: <id>`
for follow-ups (`fork: true` copies your conversation; `model` picks a catalog
key). The API's `parent_session`/`fork` fields exist for the same purpose from
scripts. See the Nested Agents section of your system prompt for judgment on
fork vs blank.

## What users see

CLI: the v1 terminal experience, now async — output streams above a live
prompt; typed lines queue while you work; Ctrl-C cancels the running turn;
`/new /resume <id> /sessions /session /title /archive /unarchive /compact
/hosts /exit`. Web GUI:
one URL per conversation, styled like the terminal — transcripts, live
streaming, send, cancel. Users cannot press your tools in either frontend;
titles and scratchpads are also yours via `session_meta`.
