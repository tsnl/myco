# myco (v3)

A from-scratch rebuild of myco around one idea: a workspace is a pool of
**instances** (chats, terminals, browsers, schedules) that humans and
agents drive through the same **verbs**. See `DESIGN.md` — it is the
contract this tree is held to, including the ledger of what crosses over
from v2 (`main-v2` branch) and why.

Current state: **M4** — the product. Protocol providers (`crates/wire` +
`myco-hostd`) export a pool over NDJSON stdio; the host kind adopts
remote instances without L2 learning a new route. Cron, headless
browser (`a11y` / `screenshot` / driven gestures), web-push delivery
for the notifier (RFC 8291/8292). Client at `http://127.0.0.1:7773/`.

```
cargo test --workspace
rustup target add wasm32-unknown-unknown && cargo install trunk
(cd clients/web && trunk build)
printf '[[users]]\nid = "%s"\n' "$USER" > ~/.myco/server.toml
cargo run -p myco
```

For the agent, add a model to `~/.myco/server.toml`:

```toml
[gateways.anthropic]
protocol = "anthropic-messages"
base_url = "https://api.anthropic.com"
auth = { source = "env", var_name = "ANTHROPIC_API_KEY" }

[models.claude]
gateway = "anthropic"
context_window = 200000
```

For another machine, add a host (any command whose stdio reaches a
`myco-hostd`):

```toml
[[hosts]]
name = "buildbox"
command = "ssh buildbox myco-hostd"
```

Remote access is an SSH tunnel (`ssh -L 7773:localhost:7773 host`),
never a bind on `0.0.0.0`.
