# Server launcher

Run `myco` in your project directory and open its printed launch URL. The server
owns sessions and tools; the browser supplies conversation controls. The launcher
also provides the internal SSH host worker. It has no terminal chat or stdin prompt
mode. `myco-eval` remains a separate evaluation utility.

| Option | Meaning |
| --- | --- |
| `--port PORT` | Loopback HTTP port, default 8765; `0` chooses a free port |
| `--bind ADDR` | Loopback IP or `localhost`, default `127.0.0.1`; `::1` selects IPv6 |
| `--profile NAME` | Select profile, overriding `MYCO_PROFILE` (default `default`) |
| `--config PATH` | Config path, overriding `MYCO_CONFIG` and the profile default |
| `--model KEY` | Default model from the config catalog |
| `--effort LEVEL` | Reasoning effort: `low`, `medium`, `high`, `max`; default `high` |
| `--resume ID` | Open a saved session or unique prefix from the launch URL |
| `--debug-dump-api-requests` | Write provider request bodies to stderr |
| `--help [ARTICLE]` | Launcher help or the embedded manual article |
| `--version` | Package and build identity |
| `--mode host` | Internal SSH worker speaking NDJSON on stdin/stdout |

`--web [PORT]` and `--web-bind ADDR` remain aliases for `--port` and `--bind`.
Non-loopback addresses, including wildcard binds, are rejected. Use an SSH tunnel
for remote access; Myco has no HTTPS listener or certificate options. Tunneling,
authentication, and the read-only `/files/` workspace routes are described in `browser`.
Host workers accept `--name` and `--max-image-base64-bytes`, supplied by the
server when it attaches a remote. The local host is always in-process.

Profiles put config, sessions, images, workspace, and manual under
`$MYCO_HOME/profiles/NAME/`; `MYCO_HOME` defaults to `~/.myco`. Local tool
processes inherit absolute `MYCO_HOME` and the selected `MYCO_PROFILE` even
when they change directories. Child sessions created by the server API share
that server's profile. Remote workers need no model credentials.

`.env` in the launch directory is loaded at startup. Configure at least one
model before starting; the overview describes the catalog. Browser controls,
attachments, archived sessions, and automation are documented in `browser`.

Ctrl-C stops the server, cancels active turns, and saves their recorded outcomes.
Closing a browser tab leaves its session running. Restarting requires opening
the new launch URL to authenticate again. Saved session URLs keep the same port;
use a fixed port for bookmarks.
