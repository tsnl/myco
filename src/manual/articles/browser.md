# Browser UI

Start the server, then open the URL it prints:

```bash
myco
myco --port 8766 --profile research
myco --port 0 --config /path/to/config.toml --resume <session-id>
myco --bind 0.0.0.0
```

The HTTPS server uses port 8765 by default; `--port 0` picks a free port. The server binds
to `127.0.0.1` unless `--bind` names another address. Its launch URL signs
this browser into this server; keep that URL private. Assets and Markdown
rendering are bundled with myco, with no frontend build step or CDN. Stop the
server with Ctrl-C in the launching terminal.

HTTPS is the default. Myco generates a persistent local certificate and prints
the path to its public `.crt` file. Trust that certificate on the browser's device,
or provide a trusted PEM certificate chain and key with `--tls-cert PATH
--tls-key PATH`. Trust only the public certificate; keep the `.pem` identity private.
Generated identities live under the selected profile's `tls/` directory, with
owner-only private-key permissions on Unix. Restarting reuses the identity.

The generated certificate covers localhost, the local hostname, loopback IPs,
and a concrete `--bind` address. For another LAN name or IP, add `--tls-name NAME`
(repeatable). Changing the name set creates a different identity that needs its
own trust setup. Myco does not install trust or disable certificate validation.
Explicit `--insecure-http` is available only on a loopback bind; remote listeners
require HTTPS. Plain HTTP and untrusted-certificate failures do not downgrade to
an unencrypted connection.

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
browser permission and HTTPS or localhost; city search does not need device permission.
Weather failures do not affect conversations.

`--bind 0.0.0.0` listens on every interface, so the UI answers any machine
that can route here, under whatever name they dial. The launch token is then the
only thing between them and these sessions — and a session is a shell on your
machine. HTTPS encrypts the connection; there is no multi-user permission model.
Treat the URL as the credential,
prefer an SSH tunnel or a trusted network, and pick a hostname over an address
when handing the link out. Restarting the server mints a new token, which
invalidates the previous launch URL.

The home page lists your visible, unarchived sessions, with search, recent update
times, and running status. **New session** creates a session and opens its own
`/sessions/<id>` URL. Session links work with bookmarks, middle-click, and browser
tab groups. Click the myco name to return home. `--resume <id>` opens that session
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
Shift-Enter or Alt-Enter inserts a newline. Mention `@path/to/image.png` to attach
an image. Supported extensions are PNG, JPEG, GIF, and WebP; bytes determine
the media type. Each image is limited by the model’s `max_image_base64_bytes`
(default 5 MiB of base64), and attachments in one message have a 20 MiB budget.
Bad paths or oversized images fail before the model request. Image paths with spaces
are not supported as attachments.

During a turn, **Queue** accepts follow-up messages in submission order. The
composer shows pending messages. They join the next model request after the
current tool batch finishes, alongside its recorded results; if no tools are
running, they are sent when the current response finishes. Up to 20 messages
can wait per session, shared across its tabs and preserved when a tab refreshes
or closes. **Cancel & send queued** stops the current run, records cancelled
tool results, and sends the pending messages with a fresh cancellation token.
Queues live in the running server and are not restored after a server restart.
A rejected submission keeps its draft.

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
The server rewrites workspace image and file links to authenticated `/files/`
URLs, including browser formats such as SVG and AVIF. Relative paths resolve
from the server's launch directory; absolute paths, `~/`, and `file://` URLs
inside that directory also map to `/files/`. URL-encode spaces and special
characters, or use Markdown's angle-bracket syntax for paths with spaces.
Saved images use the profile's image store. Explicit PNG, JPEG, GIF, and WebP
paths outside the workspace retain the authenticated `/api/image` endpoint.

`GET /files/path/to/file` and `HEAD` expose regular files below the launch
directory, including dotfiles. Anyone with the launch credential can read them.
Traversal and symlinks that escape that directory are refused. Files stream
without loading the whole document into memory; a single byte range supports
media seeking. A directory with `index.html` displays that file; directories
without an index are not listed. There is no file upload or write route.

Workspace HTML and SVG have their own restrictive content policy. Static HTML,
relative images, and styles render; scripts, forms, and embedded frames are
disabled. This keeps generated or checked-out content from executing with the
conversation UI's credentials. Interactive applications need a separate preview
origin instead of weakening this policy on the Myco origin.

USER and ASSISTANT headers have UTC timestamps on the next line. Recorded
messages use the turn's saved acceptance time; older turns without one show
`unknown`. The input box has no timestamp.

The model selector in the input bar lists the active configuration's model keys.
Choose a model between turns to use it for subsequent requests and compaction.
Switching preserves conversation history and live tool sessions, and records
the change in the session. An unavailable model leaves the current selection
unchanged and shows an error. The selector does not change the configured
startup default.

**New** opens a fresh session in the current tab; the previous session continues
running on the server. **Compact** creates a successor thread without changing
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
within a running session preserves live tools. After a server restart, open the
new launch URL once to sign in again, then reopen your saved session URLs. Keep
the same port to reuse bookmarks (`--port 0` chooses a new port each time).

## Server API and automation

Automated clients use the same authenticated HTTP API as the browser. Sign in
by requesting the printed `/auth?token=...` URL and retaining its cookie, or by
setting `Authorization: Bearer <launch-token>` on requests. Both authenticate
the API, event streams, images, assets, and workspace files. The browser uses a
host-only `Secure`, `HttpOnly`, `SameSite=Strict` cookie automatically; image
and event-stream URLs do not carry the token. Plain loopback HTTP uses a separate
cookie without `Secure` only when `--insecure-http` was explicitly selected.

Cookie-authenticated writes require an `Origin` equal to the HTTPS server
origin. Bearer clients can omit `Origin`; a supplied mismatched origin is
rejected for both authentication modes. No CORS access is granted. Restarting
changes the credential. Access to this API also grants session tools and shell
execution: the token is an owner credential, not a read-only file-share token.

| Request | Body / result |
| --- | --- |
| `POST /api/sessions` | `{"request_id":"UUID"}` → `{"id":"SESSION_ID"}` |
| `GET /api/sessions` | Visible sessions; add `?archived=true` for archives |
| `GET /api/sessions/ID` | Snapshot at `change.snapshot`, including `busy`, `status`, `blocks`, and `queued` |
| `POST /api/sessions/ID/action` | `{"request_id":"UUID","session_id":"ID","action":{"kind":"submit","text":"PROMPT"}}` → 202 accepted |
| `POST /api/sessions/ID/action` | The same envelope with `{"kind":"compact"}` or `{"kind":"select_model","key":"KEY"}` |
| `POST /api/sessions/ID/cancel` | `{"session_id":"ID"}` → 204 |
| `POST /api/sessions/ID/archive` | `{"session_id":"ID","archived":true}` → 204; false restores |
| `GET /api/events` | Server-sent events with session IDs, revisions, and changes |
| `GET /files/PATH`, `HEAD /files/PATH` | Authenticated launch-directory files; optional single `Range: bytes=START-END` |

Use a fresh UUID per operation and reuse it when retrying that operation.
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

For example, a Python client can authenticate and create a session using only
the standard library. Supply the launch URL through `MYCO_LAUNCH_URL`. For a
generated local certificate, also set `MYCO_TLS_CA` to the public `.crt` path;
omit it when the certificate is already trusted by the system:

```python
import json, os, ssl, urllib.parse, urllib.request, uuid

launch = os.environ["MYCO_LAUNCH_URL"]
url = urllib.parse.urlsplit(launch)
origin = f"{url.scheme}://{url.netloc}"
token = dict(urllib.parse.parse_qsl(url.query))["token"]
client = urllib.request.build_opener(
    urllib.request.HTTPSHandler(context=ssl.create_default_context(
        cafile=os.environ.get("MYCO_TLS_CA"))))

def post(path, payload):
    request = urllib.request.Request(origin + path,
        data=json.dumps(payload).encode(),
        headers={"Content-Type": "application/json", "Authorization": f"Bearer {token}"})
    with client.open(request) as response:
        return response.read()

session = json.loads(post("/api/sessions", {"request_id": str(uuid.uuid4())}))["id"]
post(f"/api/sessions/{session}/action", {
    "request_id": str(uuid.uuid4()), "session_id": session,
    "action": {"kind": "submit", "text": "Explain this repository"}})
```

Clients must be given the launch credential by their operator. The server does
not inject it into model context. API-created sessions use the server's launch
directory and profile; no new process or model configuration is needed per child.
