"""myco.py — the myco REST API as a small Python client.

The whole surface of `myco --mode serve`, importable or runnable. This file
doubles as executable documentation of the wire protocol: every method is one
HTTP call with the same names the server uses.

Auth: pass a bearer token, or let it find one — $MYCO_API_TOKEN (set inside
agent-spawned tools), then $MYCO_HOME/v2/operator.token (written by
`--mode serve` for local scripts). Remote users sign in with a password via
`Myco.login(...)`.

    from myco import Myco
    m = Myco()                            # local server, operator token
    s = m.create_session(model="opus")
    print(m.ask(s["id"], "summarize src/"))

    m.update_session(sid, model="haiku", effort="low")
    for shell in m.shells(sid)["shells"]:
        print(m.shell_screen(sid, shell["host"], shell["id"])["text"])

CLI:  python3 myco.py ask "prompt" [--session ID] [--model KEY]
      python3 myco.py sessions | shells SID | screen SID HOST SHELL | subagents SID

Dependencies: none (urllib only).
"""

from __future__ import annotations

import json
import os
import pathlib
import time
import urllib.parse
import urllib.request


def _default_base() -> str:
    return os.environ.get("MYCO_API", "http://127.0.0.1:7773/api")


def _default_token() -> str:
    tok = os.environ.get("MYCO_API_TOKEN")
    if tok:
        return tok
    home = pathlib.Path(os.environ.get("MYCO_HOME", pathlib.Path.home() / ".myco"))
    path = home / "v2" / "operator.token"
    try:
        return path.read_text().strip()
    except OSError as e:
        raise RuntimeError(
            f"no token: set $MYCO_API_TOKEN or start `myco --mode serve` (looked in {path})"
        ) from e


class MycoError(RuntimeError):
    """An API error reply: `{error, kind}`."""


class Myco:
    def __init__(self, base: str | None = None, token: str | None = None):
        self.base = (base or _default_base()).rstrip("/")
        self.token = token if token is not None else _default_token()

    @classmethod
    def login(cls, username: str, password: str, base: str | None = None) -> "Myco":
        """OAuth password grant → an authenticated client."""
        base = (base or _default_base()).rstrip("/")
        body = urllib.parse.urlencode(
            {"grant_type": "password", "username": username, "password": password}
        ).encode()
        req = urllib.request.Request(f"{base}/auth/token", data=body, method="POST")
        req.add_header("Content-Type", "application/x-www-form-urlencoded")
        with urllib.request.urlopen(req) as resp:
            token = json.load(resp)["access_token"]
        return cls(base, token)

    # -- plumbing ----------------------------------------------------------

    def _call(self, method: str, path: str, body: dict | None = None):
        req = urllib.request.Request(
            f"{self.base}{path}",
            data=None if body is None else json.dumps(body).encode(),
            method=method,
        )
        req.add_header("Authorization", f"Bearer {self.token}")
        if body is not None:
            req.add_header("Content-Type", "application/json")
        try:
            with urllib.request.urlopen(req) as resp:
                return json.load(resp)
        except urllib.error.HTTPError as e:
            try:
                raise MycoError(json.load(e)["error"]) from None
            except (ValueError, KeyError):
                raise MycoError(f"http {e.code} on {method} {path}") from None

    def _get(self, path):
        return self._call("GET", path)

    # -- sessions ----------------------------------------------------------

    def whoami(self):
        return self._get("/whoami")

    def models(self):
        return self._get("/models")

    def sessions(self, include_archived: bool = False):
        return self._get(f"/sessions?include_archived={str(include_archived).lower()}")

    def create_session(self, model: str | None = None, parent_session: str | None = None,
                       fork: bool = False):
        return self._call("POST", "/sessions", {
            "model": model, "parent_session": parent_session, "fork": fork,
        })

    def session(self, sid: str):
        return self._get(f"/sessions/{sid}")

    def update_session(self, sid: str, *, title: str | None = None,
                       archived: bool | None = None, model: str | None = None,
                       effort: str | None = None):
        """PATCH metadata. `effort=""` clears the override; model/effort apply
        from the next turn."""
        body = {k: v for k, v in dict(
            title=title, archived=archived, model=model, effort=effort).items()
            if v is not None}
        return self._call("PATCH", f"/sessions/{sid}", body)

    def post(self, sid: str, text: str):
        """Post a message; returns `{busy, ...}` — busy means a reply is coming."""
        return self._call("POST", f"/sessions/{sid}/messages", {"text": text})

    def poll(self, sid: str, since: int = 0):
        return self._get(f"/sessions/{sid}/poll?since={since}")

    def cancel(self, sid: str):
        return self._call("POST", f"/sessions/{sid}/cancel")

    def compact(self, sid: str):
        return self._call("POST", f"/sessions/{sid}/compact")

    def ask(self, sid: str, text: str, poll_interval: float = 0.5) -> str:
        """Post and block until the turn settles; returns the agent's final prose."""
        self.post(sid, text)
        while True:
            time.sleep(poll_interval)
            p = self.poll(sid)
            if not p["busy"]:
                break
        if p.get("last_error"):
            raise MycoError(p["last_error"])
        for entry in reversed(p["entries"]):
            body = entry["body"]
            if body.get("kind") == "agent":
                texts = [c["Text"]["text"] for c in body.get("content", []) if "Text" in c]
                if texts:
                    return "\n".join(texts)
        return ""

    # -- shells ------------------------------------------------------------

    def shells(self, sid: str):
        return self._get(f"/sessions/{sid}/shells")

    def shell_tail(self, sid: str, host: str, shell: str, from_: int = 0):
        return self._get(f"/sessions/{sid}/shells/{host}/{shell}?from={from_}")

    def shell_screen(self, sid: str, host: str, shell: str):
        return self._get(f"/sessions/{sid}/shells/{host}/{shell}/screen")

    def shell_input(self, sid: str, host: str, shell: str, data: str):
        return self._call("POST", f"/sessions/{sid}/shells/{host}/{shell}/input",
                          {"data": data})

    def shell_lock(self, sid: str, host: str, shell: str, lock: str):
        """lock: "user" (take the keyboard) or "assistant" (hand it back)."""
        return self._call("POST", f"/sessions/{sid}/shells/{host}/{shell}/lock",
                          {"lock": lock})

    def shell_start(self, sid: str, host: str = "local", name: str = None,
                    command: str = None, pty: bool = True):
        """Open a terminal on `host`: a real bash session owned by the
        session's agent, starting user-held."""
        return self._call("POST", f"/sessions/{sid}/shells/{host}",
                          {"shell": name, "command": command, "pty": pty})

    def shell_rename(self, sid: str, host: str, shell: str, title: str):
        """Set (or clear, with "") a terminal's display name."""
        return self._call("POST", f"/sessions/{sid}/shells/{host}/{shell}/rename",
                          {"title": title})

    def shell_resize(self, sid: str, host: str, shell: str, cols: int, rows: int):
        """Fit the terminal (requires the user keyboard lock); a pty child
        learns via SIGWINCH."""
        return self._call("POST", f"/sessions/{sid}/shells/{host}/{shell}/resize",
                          {"cols": cols, "rows": rows})

    # -- subagents ---------------------------------------------------------

    def subagents(self, sid: str):
        """Live subagent children of `sid` (each is a full session by id)."""
        return self._get(f"/sessions/{sid}/subagents")

    def subagent_lock(self, sid: str, child: str, lock: str):
        """lock: "user" (take the child over) or "assistant" (hand it back).
        While user-held, the parent agent's `subagent` calls to it are
        refused."""
        return self._call("POST", f"/sessions/{sid}/subagents/{child}/lock",
                          {"lock": lock})

    def subagent_input(self, sid: str, child: str, text: str):
        """Post into a user-held child; its agent answers you directly."""
        return self._call("POST", f"/sessions/{sid}/subagents/{child}/input",
                          {"text": text})


def _main(argv: list[str]) -> int:
    if len(argv) < 2:
        print(__doc__)
        return 2
    m = Myco()
    cmd, args = argv[1], argv[2:]
    if cmd == "ask":
        sid = None
        model = None
        rest = []
        it = iter(args)
        for a in it:
            if a == "--session":
                sid = next(it)
            elif a == "--model":
                model = next(it)
            else:
                rest.append(a)
        if sid is None:
            sid = m.create_session(model=model)["id"]
        print(m.ask(sid, " ".join(rest)))
        print(f"session={sid}", file=__import__("sys").stderr)
    elif cmd == "sessions":
        for s in m.sessions():
            print(f"{s['id'][:8]}  {s['model']:<12} {s.get('title') or s['snippet']}")
    elif cmd == "shells":
        for s in m.shells(args[0])["shells"]:
            print(f"{s['host']}/{s['id']}  running={s['running']} lock={s['lock']} pty={s['pty']}")
    elif cmd == "subagents":
        for s in m.subagents(args[0])["subagents"]:
            print(f"{s['id'][:8]}  {s['model']:<12} busy={s['busy']} lock={s['lock']}")
    elif cmd == "screen":
        print(m.shell_screen(args[0], args[1], args[2])["text"])
    else:
        print(__doc__)
        return 2
    return 0


if __name__ == "__main__":
    import sys

    sys.exit(_main(sys.argv))
