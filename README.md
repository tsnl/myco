# myco (v3)

A from-scratch rebuild of myco around one idea: a workspace is a pool of
**instances** (chats, terminals, browsers, schedules) that humans and
agents drive through the same **verbs**. See `DESIGN.md` — it is the
contract this tree is held to, including the ledger of what crosses over
from v2 (`main-v2` branch) and why.

Current state: **M0** — the actor runtime (`crates/runtime`), the
instance framework (`crates/instance`), and the first real kind, a pty
terminal (`crates/kind-tty`), all driven through the bus in tests.

```
cargo test --workspace
```

No server, no GUI yet; those are M1/M3 in the design doc.
