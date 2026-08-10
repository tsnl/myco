# myco (v3)

A from-scratch rebuild of myco around one idea: a workspace is a pool of
**instances** (chats, terminals, browsers, schedules) that humans and
agents drive through the same **verbs**. See `DESIGN.md` — it is the
contract this tree is held to, including the ledger of what crosses over
from v2 (`main-v2` branch) and why.

Current state: **M3** — the client is a fold (one reducer, one action
stream; see DESIGN.md L3). Rust→wasm, one-origin delivery: the `myco`
binary serves the bundle at `/`, `/api/*` wins. Tree, split panes, tty +
chat renderers, `Cmd+P` over the derived registry, operator panel.
Visual contract: `STYLE.md`.

```
cargo test --workspace
rustup target add wasm32-unknown-unknown && cargo install trunk
(cd clients/web && trunk build)
printf '[[users]]\nid = "%s"\n' "$USER" > ~/.myco/server.toml
cargo run -p myco
# → http://127.0.0.1:7773/
```
