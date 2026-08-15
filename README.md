# myco (v3)

A from-scratch rebuild of myco around one idea: a workspace is a pool of
**instances** (chats, terminals, browsers, schedules) that humans and
agents drive through the same **verbs**. See `DESIGN.md` — it is the
contract this tree is held to, including the ledger of what crosses over
from v2 (`main-v2` branch) and why.

Current state: **M1** — L2 is the human's adapter to the bus. Auth
(one-time codes, passkeys, operator), the generic verb gateway, one
multiplexed watch (`/api/instances/{id}/changed` + `/api/ws`), the
operator admin surface, and `clients/myco.py`. Kinds: tty. No agent,
no GUI yet.

```
cargo test --workspace
printf '[[users]]\nid = "%s"\n' "$USER" > ~/.myco/server.toml
cargo run -p myco
# banner prints a one-time code; then drive tty verbs over /api
```
