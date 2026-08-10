# myco (v3)

A from-scratch rebuild of myco around one idea: a workspace is a pool of
**instances** (chats, terminals, browsers, schedules) that humans and
agents drive through the same **verbs**. See `DESIGN.md` — it is the
contract this tree is held to, including the ledger of what crosses over
from v2 (`main-v2` branch) and why.

Current state: **M1 started** — L2 skeleton. `crates/server` serves
`GET /api/kinds` (capability discovery off `Pool::kinds()`) with the
COOP/COEP isolation headers. `crates/myco` is the binary: register tty,
serve loopback (default `:7773`). No verb gateway, no auth, no GUI yet.

```
cargo test --workspace
cargo run -p myco
# → http://127.0.0.1:7773/api/kinds
```
