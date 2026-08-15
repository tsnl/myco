"""myco.py — the myco v3 wire, as a small Python client.

This file doubles as executable documentation of the protocol, which is
deliberately tiny: everything is a verb.

    GET  /api/kinds                          capability discovery
    POST /api/instances                      {kind, project, title, args}
    GET  /api/instances?project=             listing
    POST /api/instances/{id}/verbs/{verb}    body = args, reply = result
    GET  /api/instances/{id}/changed         ?since=&timeout_ms= (long-poll)
    POST /api/auth/token                     grant_type=code&username=&code=
    GET  /api/whoami · POST /api/auth/logout
    /api/admin/...                           operator only (see Myco.admin_*)

Auth: pass a bearer token, or let it find one — $MYCO_API_TOKEN, then
$MYCO_HOME/operator.token (written by the server at boot for local
scripts). Remote users redeem a one-time code via Myco.login_with_code
(minted in the admin surface, or printed by the server at startup).

    from myco import Myco
    m = Myco()                                  # local server, operator token
    tty = m.create("tty", args={"command": "cat"})
    m.call(tty["id"], "input", {"data": "hello\\n"})
    m.changed(tty["id"], since=0)               # wait for the echo
    print(m.call(tty["id"], "text")["text"])

CLI:  python3 myco.py kinds | ls [PROJECT] | new KIND [JSON_ARGS]
      python3 myco.py verb ID VERB [JSON_ARGS] | watch ID | whoami

Dependencies: none (urllib only).
"""

from __future__ import annotations

import json
import os
import pathlib
import sys
import urllib.error
import urllib.parse
import urllib.request


def _default_base() -> str:
    return os.environ.get("MYCO_API", "http://127.0.0.1:7773/api")


def _default_token() -> str:
    tok = os.environ.get("MYCO_API_TOKEN")
    if tok:
        return tok
    home = pathlib.Path(os.environ.get("MYCO_HOME", pathlib.Path.home() / ".myco"))
    path = home / "operator.token"
    try:
        return path.read_text().strip()
    except OSError as e:
        raise RuntimeError(
            f"no token: set $MYCO_API_TOKEN or start the server (looked in {path})"
        ) from e


class MycoError(RuntimeError):
    """An API refusal: carries the wire's own error name and reason."""


class Myco:
    def __init__(self, base: str | None = None, token: str | None = None):
        self.base = (base or _default_base()).rstrip("/")
        self.token = token if token is not None else _default_token()

    @classmethod
    def login_with_code(cls, username: str, code: str, base: str | None = None) -> "Myco":
        """Redeem a one-time code (admin surface, or the startup banner)."""
        base = (base or _default_base()).rstrip("/")
        body = urllib.parse.urlencode(
            {"grant_type": "code", "username": username, "code": code}
        ).encode()
        req = urllib.request.Request(f"{base}/auth/token", data=body, method="POST")
        req.add_header("Content-Type", "application/x-www-form-urlencoded")
        with urllib.request.urlopen(req) as resp:
            return cls(base, json.load(resp)["access_token"])

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
                err = json.load(e)
                raise MycoError(f"{err.get('error')}: {err.get('why', json.dumps(err))}") from None
            except (ValueError, KeyError):
                raise MycoError(f"http {e.code} on {method} {path}") from None

    def _get(self, path: str):
        return self._call("GET", path)

    # -- the whole protocol ------------------------------------------------

    def whoami(self):
        return self._get("/whoami")

    def logout(self):
        return self._call("POST", "/auth/logout")

    def kinds(self):
        """Every kind's spec: verbs, flags, version, default-read hints."""
        return self._get("/kinds")

    def create(self, kind: str, project: str = "", title: str = "", args: dict | None = None):
        return self._call("POST", "/instances", {
            "kind": kind, "project": project, "title": title, "args": args or {},
        })

    def list(self, project: str | None = None):
        query = f"?project={urllib.parse.quote(project)}" if project else ""
        return self._get(f"/instances{query}")

    def call(self, instance_id: str, verb: str, args: dict | None = None):
        """The generic gateway: any verb of any kind, sys.* included."""
        return self._call("POST", f"/instances/{instance_id}/verbs/{verb}", args)

    def changed(self, instance_id: str, since: int = 0, timeout_ms: int = 30000) -> int:
        """Long-poll until the watermark passes `since` (or timeout); returns
        the current watermark either way — re-read and re-poll."""
        got = self._get(
            f"/instances/{instance_id}/changed?since={since}&timeout_ms={timeout_ms}"
        )
        return got["watermark"]

    # -- administration (operator token only) ------------------------------

    def admin_users(self):
        return self._get("/admin/users")

    def mint_code(self, username: str):
        """One-time sign-in code — single use, 15 minutes, replaces any
        earlier unredeemed code for them."""
        return self._call("POST", f"/admin/users/{username}/code")

    def disable_user(self, username: str):
        return self._call("POST", f"/admin/users/{username}/disable")

    def enable_user(self, username: str):
        return self._call("POST", f"/admin/users/{username}/enable")

    def revoke_sessions(self, username: str):
        return self._call("POST", f"/admin/users/{username}/revoke")

    def remove_user(self, username: str):
        return self._call("DELETE", f"/admin/users/{username}")


def _main(argv: list[str]) -> int:
    if len(argv) < 2:
        print(__doc__)
        return 2
    m = Myco()
    cmd, args = argv[1], argv[2:]
    if cmd == "kinds":
        for k in m.kinds():
            verbs = ", ".join(v["name"] for v in k["verbs"])
            print(f"{k['kind']} v{k['version']}: {verbs}")
    elif cmd == "ls":
        for i in m.list(args[0] if args else None):
            driver = i.get("driver") or {}
            seat = driver.get("id", "open")
            print(f"{i['id'][:8]}  {i['kind']:<8} {i['project']:<12} seat={seat}  {i['title']}")
    elif cmd == "new":
        extra = json.loads(args[1]) if len(args) > 1 else {}
        print(json.dumps(m.create(args[0], args=extra), indent=2))
    elif cmd == "verb":
        payload = json.loads(args[2]) if len(args) > 2 else None
        print(json.dumps(m.call(args[0], args[1], payload), indent=2))
    elif cmd == "watch":
        seen = 0
        while True:
            seen = m.changed(args[0], since=seen, timeout_ms=60000)
            print(f"watermark={seen}", file=sys.stderr)
    elif cmd == "whoami":
        print(json.dumps(m.whoami()))
    else:
        print(__doc__)
        return 2
    return 0


if __name__ == "__main__":
    sys.exit(_main(sys.argv))
