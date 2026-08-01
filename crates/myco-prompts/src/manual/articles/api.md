# Driving myco (API, GUI, nested agents)

myco runs as a **server**: `myco` serves an HTTP API on `http://127.0.0.1:7773/api`
(`--port` to change) and the web GUI connects to it. There is no interactive
terminal UI; users talk to you through a client, and programs — including you —
drive the same API.

## The API

The base URL is exported as `$MYCO_API` in every local bash session.

| Endpoint | Meaning |
|----------|---------|
| `GET /sessions` | Recent visible sessions (id, title, model, live/busy) |
| `POST /sessions` | Create: `{model?, parent_session?, fork?}` → summary with `id` |
| `GET /sessions/<id>` | Metadata + full transcript entries |
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
  too large for the provider drops the offending message so the session can
  continue (the error says so).
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

The GUI is one URL per conversation: transcripts, live streaming output, a
send box, cancel. There are no slash-commands anymore; anything a user asks
about "commands" maps to API calls or to asking you (titles and scratchpads
are yours via `session_meta`).
