# myco (v3)

A from-scratch rebuild of myco around one idea: a workspace is a pool of
**instances** (chats, terminals, browsers, schedules) that humans and
agents drive through the same **verbs**. See `DESIGN.md` — it is the
contract this tree is held to, including the ledger of what crosses over
from v2 (`main-v2` branch) and why.

Current state: **M2** — the agent sits beside L2, not inside it. Chat is
an instance (`post` / cursored `tail`); a subagent is a chat with a
parent. Providers, a cancellable turn, the named-tool dispatcher (bash
as verbs — including piped mode and signals), standing subscriptions,
and the notifier kind (inbox; web-push delivery is M4). Still no GUI.

```
cargo test --workspace
printf '[[users]]\nid = "%s"\n' "$USER" > ~/.myco/server.toml
# optional: [gateways.*] / [models.*] for the agent
cargo run -p myco
```
