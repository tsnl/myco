# Browser UI

Start the server, then open the URL it prints:

```bash
myco
myco --port 8766 --profile research
myco --port 0 --config /path/to/config.toml --resume <session-id>
myco --bind ::1
```

The HTTP server uses port 8765 by default; `--port 0` picks a free port. The server binds
to `127.0.0.1` unless `--bind` names another loopback IP (`localhost` selects IPv4).
Non-loopback addresses, including `0.0.0.0` and `::`, are rejected. Open the printed
URL directly; there is no login, token, or browser cookie. Assets and Markdown
rendering are bundled with myco, with no frontend build step or CDN. Stop the
server with Ctrl-C in the launching terminal.

For remote access, start Myco on the remote host, then open an SSH tunnel from
the computer running your browser:

```bash
ssh -N -o ExitOnForwardFailure=yes -L 127.0.0.1:8766:127.0.0.1:8765 user@remote-host
```

This forwards local port 8766 to the remote server's default port 8765. Change
the printed launch URL's address to `http://127.0.0.1:8766`, keeping its session
path when resuming. Keep SSH running while using Myco. If either
port differs, adjust the command and browser URL to match. For a remote server
bound to `::1`, use `[::1]:8765` as the forwarding destination.

Myco does not serve HTTPS or manage certificates. SSH provides encryption and
host authentication between the two computers; HTTP stays on each loopback
connection. Keep the forwarding listener on `127.0.0.1` or `::1`. Files,
actions, and live event streams work through the tunnel, including when its
local and remote ports differ. Requests must address `localhost` or a loopback
IP; LAN names and other hostnames are refused.

The browser uses square-edged translucent panels over a locally rendered sky.
The conversation stays in a central well, with the sky visible on both sides;
floating controls use background blur. The conversation well is the lightest
surface; the input bar and top banner share darker translucent glass, with
dialogs darkest in front. Translucent clouds drift slowly in three
layers: fine high wisps near the top, soft middle billows below them, and broader
low clouds near the horizon. Broken banks vary in shape and height; increasing
cover adds thin, broad veils. Their feathered edges and overlapping layers let the
sky show through. Gentle directional light travels across the clouds through the
day, softening in overcast weather. Colors follow daylight, sunset, and night;
stars twinkle behind the clouds. An occasional distant airplane crosses the sky,
with a faint contrail or navigation lights after dark. These are decorative
flybys, with at most two visible at once. Wet weather adds fine rain streaks at
several depths; wind influences their slant, and heavier precipitation increases
their density. Overcast skies darken the clouds and obscure stars and airplanes.
Reduced-motion settings keep clouds and stars still and disable airplanes and
falling rain. Hidden tabs pause animation and airplane arrivals.
The bundled renderer is adapted from [Horizon](https://github.com/dnlzro/horizon),
with its MIT license retained in the served source.

The **Settings** icon in the top bar opens a centered modal. Its **Sky** section
contains the weather controls. Choose a city or explicitly select **Use my
location** to reflect its current cloud cover, rain, showers, and wind from
[Open-Meteo](https://open-meteo.com/en/docs). The panel names the reported weather
condition. The layers roughly represent below 3 km,
3–8 km, and above 8 km. Surface wind influences the deliberately slow drift;
the art is an impression of modelled current conditions, not a view of individual
real clouds. Rain intensity accounts for the feed's accumulation interval; a
drizzle or rain code can still produce light streaks when its amount rounds to
zero. Snow-only reports do not produce rain.
The approximate day/night cycle and direction of light follow that location's
clock and update once a minute; they do not calculate seasonal sunrise, sunset,
or the position of the moon.

The location is saved in this browser and shared across its tabs. Coordinates
are rounded to two decimal places before Myco forwards them to Open-Meteo.
City searches also pass through Myco, with location data from GeoNames. The
browser contacts only Myco. Weather refreshes every 15 minutes while visible;
the server coalesces requests and keeps bounded caches. The free weather service
is for [non-commercial use](https://open-meteo.com/en/terms).

With no selected location, **Illustrated sky** uses decorative clouds and the
browser's clock without weather requests or location permission. This is also
the fallback when weather is unavailable; settings label any retained conditions
as last available, and discard them after two hours. Device location requires
browser permission and a loopback browser URL; city search does not need device permission.
Weather failures do not affect conversations.

SSH controls remote access. Any process or user able to reach the loopback port
can use the sessions, tools, and workspace files; there is no per-user permission
model. The server retains Host and browser-origin checks to reject requests from
other websites. Restarting does not require signing in again.

The home page lists your visible, unarchived sessions, with search, recent update
times, and live activity indicators. A pulsing square beside **Running** in the
session header and browser list means a run is in progress, including time waiting
for model output and executing tools. **Background tasks** has a steady indicator
when tools remain open after the run. **Ready**, **Stopped**, and **Saved** are idle;
**Reconnecting…** means the current state is unknown. Reduced-motion preferences
disable the pulse. Status changes arrive through the shared event stream; the
browser also polls for changes made by other server processes.

**New session** opens a separate tab, creates a session,
and navigates that tab to its `/sessions/<id>` URL. Session links work with
bookmarks, middle-click, and browser tab groups. Click the myco name to return
home. `--resume <id>` opens that session
directly from the launch URL.

Click **Archive** beside a session to hide it from the active list. Choose
**Archived sessions** above the list to find archived sessions and **Restore**
them. Archiving preserves history, the session URL, and running tools; an open
tab can continue its turn. It does not archive child sessions. A session held
by another myco process must be archived from that process or after it closes.
The list refreshes while the home page is visible; changes made outside this
server can take up to ten seconds to appear.

`--model`, `--effort`, `--profile`, `--config`, and `--resume <id>` apply at
startup. Effort and configuration-file changes require restarting the server.
The launcher options are documented in `cli`.

## Conversation controls

The floating input bar stays pinned while the conversation scrolls. Enter sends;
Shift-Enter or Alt-Enter inserts a newline. Use **Attach**, paste an image from
the clipboard, or drop images onto the composer. Preview and remove images before
sending; an image-only message is allowed. PNG, JPEG, GIF, and WebP are supported,
with up to 20 selected or pasted images per message. Bytes determine the media
type. The browser sends the images when you press **Send** or **Queue**;
rejected sends keep the draft and attachments for retry.

Mention `@path/to/image.png` to attach a file on the server instead. Each image is
limited by the model’s `max_image_base64_bytes` (default 5 MiB of base64), and all
attachments in one message share a 20 MiB budget, including `@path` images. Bad
paths or oversized images fail before the model request. Image paths with spaces
are not supported in `@path` mentions; selected files may have spaces in their names.
Accepted uploads use the profile's image store and remain available in saved
sessions after restart. Unsent attachments stay in the current tab's draft.

During a turn, **Queue** accepts follow-up messages in submission order. The
composer shows pending messages. They join the next model request after the
current tool batch finishes, alongside its recorded results; if no tools are
running, they are sent when the current response finishes. Up to 20 messages
can wait per session, shared across its tabs and preserved when a tab refreshes
or closes. **Cancel & send queued** stops the current run, records cancelled
tool results, and sends the pending messages with a fresh cancellation token.
Queues live in the running server and are not restored after a server restart.
A rejected submission keeps its draft.
Queued image thumbnails are visible across session tabs and after a tab reload.

The page heading and browser tab title follow the session title, including the
first-message title and agent renames during a running turn. Hover over a truncated
heading to read its full title. The top bar aligns with the conversation and keeps
its controls on a separate row on narrow screens. Activity counts appear only
while tools or background sessions are running.

Each tool appears as soon as execution starts, with a truncated argument
preview in its collapsed header. Running calls are cyan; completed calls turn
green, and failures red. Expand a block to inspect its complete recorded input,
output, images, and outcome. Input shows each top-level key as a bold label above
its value. Strings retain their newlines without JSON quoting; nested objects and
arrays retain JSON structure. There is no browser verbose mode.

Tool calls show elapsed execution time to tenths of a second in their headers
and in the Activity drawer. Running timers update every 0.1 seconds and survive
page refreshes; completion, failure, or cancellation freezes the final duration.
Durations are observed by the running browser server and retained while viewing
the same thread. Saved history opened after a server restart has no timing data.

**Activity** opens a right-hand drawer with separate sections for active tool
calls and local background tasks, such as bash sessions that continue between
turns. The drawer starts closed; its button shows the current activity count.
Click an active call to close the drawer and open its block. Close the drawer
with its close button, Escape, or a click outside it. Background-task summaries
refresh every second without consuming tool output, and disappear when the task
ends. Background summaries cover the local host; active calls
include remote tools too.

Assistant responses render Markdown headings, lists, tables, task lists,
blockquote text, code blocks, links, and images. Text uses one font size;
headings use weight and underlines. Raw HTML is displayed as text. Markdown
images can reference HTTP(S) URLs, supported image data URLs, or local files.
The server rewrites workspace image and file links to `/files/`
URLs, including browser formats such as SVG and AVIF. Relative paths resolve
from the server's launch directory; absolute paths, `~/`, and `file://` URLs
inside that directory also map to `/files/`. URL-encode spaces and special
characters, or use Markdown's angle-bracket syntax for paths with spaces.
Saved images use the profile's image store. Explicit PNG, JPEG, GIF, and WebP
paths outside the workspace use the `/api/image` endpoint.

`GET /files/path/to/file` and `HEAD` expose regular files below the launch
directory, including dotfiles. Anyone able to reach the loopback port can read them.
Traversal and symlinks that escape that directory are refused. Files stream
without loading the whole document into memory; a single byte range supports
media seeking on GET requests. HEAD always describes the complete file. With
`If-Range`, the server returns the full file because it does not issue validators.
A directory with `index.html` displays that file; directories
without an index are not listed. Workspace files have no upload or write route;
message attachments are stored separately in the profile's image store.

Workspace HTML and SVG have their own restrictive content policy. Static HTML,
relative images, and styles render; scripts, forms, and embedded frames are
disabled. This keeps generated or checked-out content from executing with the
conversation UI's access to the local API. Interactive applications need a
separate preview origin instead of weakening this policy on the Myco origin.

USER and ASSISTANT headers have UTC timestamps on the next line. A reply that
starts with tools gets its ASSISTANT header before those calls, live and on replay.
Recorded messages use the turn's saved acceptance time; older turns without one show
`unknown`. The input box has no timestamp.

The model selector in the input bar lists the active configuration's model keys.
Choose a model between turns to use it for subsequent requests and compaction.
Switching preserves conversation history and live tool sessions, and records
the change in the session. An unavailable model leaves the current selection
unchanged and shows an error. The selector does not change the configured
startup default.

**New** opens a fresh session in a new tab; the current tab, draft, and running
turn stay in place. **Compact** creates a successor thread without changing
the session URL. These controls also accept `/new`, `/compact`, and `/resume <id>`
in the input; `/resume` navigates only the current tab. Other slash commands are not available.

Automatic compaction is enabled per model with `auto_compact_at`, a fraction of
its context window (for example, `0.8`). Without this setting it is disabled.
When the threshold is reached, the runner settles pending tools, saves a summary
in a successor thread, and continues the task automatically. Queued follow-ups
join that continued context. Manual **Compact** finishes after creating the
thread and waits for your next message. Failed or ineffective automatic compaction
shows a warning and disables further attempts until manual compaction or a session
change; cancellation preserves the source thread.

## Running and resuming

One server can run several sessions concurrently. Each has its own runner, model
selection, cancellation, tool state, and writer lock. Tabs on different session
URLs work independently; tabs on the same URL observe the same run. Changing a
model or compacting is disabled during that session's turn. Another browser
server cannot write an opened session while this server holds its writer lock.
An unavailable or locked session shows an error without interrupting other tabs.

Refreshing, navigating away, or closing a tab does not cancel its active turn.
Reopening its session URL reconnects to its output and live tools. **Cancel run**
affects only that session; Ctrl-C in the server terminal cancels all active turns.
Opened sessions and their locks stay alive until the server stops. Request retries
do not submit the same action or create the same session twice. If submission
fails, the input draft remains available. Tabs share a live-output connection
through a browser SharedWorker so a tab group does not exhaust HTTP connections.

Restarting the server and resuming a saved session restores conversation history,
not old bash processes or editor state. Uncertain tool outcomes from an
interrupted process are recorded without rerunning their effects. Compaction
within a running session preserves live tools. After a server restart, reopen
your saved session URLs directly. Keep the same port to reuse bookmarks
(`--port 0` chooses a new port each time).

## Server API and automation

Automated clients use the same loopback HTTP API as the browser. No credential
or login request is needed. Use the local forwarding address when connecting over
SSH. POST bodies use `Content-Type: application/json`.

Browser requests must come from the same origin. A supplied `Origin` must match
the loopback address and port in `Host`; cross-site and cross-port browser requests
are rejected, and no CORS access is granted. Native clients can omit `Origin` and
Fetch Metadata. Access to this API includes session tools and shell execution.

| Request | Body / result |
| --- | --- |
| `POST /api/sessions` | `{"request_id":"UUID"}` → `{"id":"SESSION_ID"}` |
| `GET /api/sessions` | Visible sessions with `busy` and `status`; add `?archived=true` for archives |
| `GET /api/sessions/ID` | Snapshot at `change.snapshot`, including `busy`, `status`, `blocks`, and `queued` |
| `POST /api/sessions/ID/action` | `{"request_id":"UUID","session_id":"ID","action":{"kind":"submit","text":"PROMPT"}}` → 202 accepted |
| `POST /api/sessions/ID/action` | The same envelope with `{"kind":"compact"}` or `{"kind":"select_model","key":"KEY"}` |
| `POST /api/sessions/ID/cancel` | `{"session_id":"ID"}` → 204 |
| `POST /api/sessions/ID/archive` | `{"session_id":"ID","archived":true}` → 204; false restores |
| `GET /api/events` | Server-sent events with session IDs, revisions, and changes |
| `GET /files/PATH`, `HEAD /files/PATH` | Launch-directory files; GET supports a single `Range: bytes=START-END` |

Use a fresh UUID per operation and reuse it when retrying that operation.
Submit actions may include `images`, an array of base64 image data URLs. `text`
may be omitted when images are present. The server validates image type and size,
then keeps image-store references in its queue and history. Snapshots include
`attachment_limits` for the selected model; queued messages include their image
references. The action route allows up to 22 MiB of JSON for the image budget and
text envelope; other JSON routes retain their 2 MiB limit.

Creation uses the UUID as the session ID and survives restart without creating
a duplicate. Action deduplication lasts for the running session worker; after
restart, inspect the saved snapshot before deciding whether to submit again.
A 202 response means accepted, not finished. Wait until the snapshot is idle,
then inspect its output and errors and verify the task's artifacts.

To create a hidden child, include `"parent_session":"PARENT_ID"` in the
creation body. The parent must exist in this server's profile. Add `"fork":true`
to copy its saved context; a fork requires a parent. Children have independent
runners and tool state, remain hidden from both normal and archived listings,
and can be opened directly by ID. Retrying creation keeps its original context.
Model selection is a separate action before submission.

For example, a Python client can create a session using only
the standard library. Supply the launch URL through `MYCO_LAUNCH_URL`; when
using a tunnel, first change its address to the local forwarding address:

```python
import json, os, urllib.parse, urllib.request, uuid

launch = os.environ["MYCO_LAUNCH_URL"]
url = urllib.parse.urlsplit(launch)
origin = f"{url.scheme}://{url.netloc}"
client = urllib.request.build_opener(urllib.request.ProxyHandler({}))

def post(path, payload):
    request = urllib.request.Request(origin + path,
        data=json.dumps(payload).encode(),
        headers={"Content-Type": "application/json"})
    with client.open(request) as response:
        return response.read()

session = json.loads(post("/api/sessions", {"request_id": str(uuid.uuid4())}))["id"]
post(f"/api/sessions/{session}/action", {
    "request_id": str(uuid.uuid4()), "session_id": session,
    "action": {"kind": "submit", "text": "Explain this repository"}})
```

API-created sessions use the server's launch directory and profile; no new
process or model configuration is needed per child.
