# myco

A programmable conversation server with shared services and a web GUI.

The implementation is organized into separately reviewable steps in
[DESIGN.md](DESIGN.md). The engine is one `myco` crate, organized into `model`,
`thread`, `logic`, `service`, and `api` modules. The server and remote workers are
planned binary targets; the browser GUI is a separate application.

Implemented so far: [`myco::model`](src/model/README.md), with a concrete
`GenAiClient`, private provider drivers, and a generation stream. Its entire
[public interface](src/model/mod.rs) is in `mod.rs`. Thread history, workflow logic,
services, and the HTTP API are subsequent review steps in the design.

```sh
cargo test --locked --offline --workspace
cargo clippy --locked --offline --workspace --all-targets -- -D warnings
cargo fmt --all --check
```

Run `cargo fetch --locked` once if dependencies are not cached. Protocol tests
bind loopback HTTP sockets. They make no external API calls.

Licensed under the [Apache License 2.0](LICENSE). See [NOTICE](NOTICE) for
attribution.
