# myco

A programmable conversation server with shared services and a web GUI.

The implementation is organized into separately reviewable steps in
[DESIGN.md](DESIGN.md). The engine is one `myco` crate, organized into `model`,
`thread`, `logic`, `service`, and `api` modules. The server and remote workers are
planned binary targets; the browser GUI is a separate application.

Implemented so far:

- [`myco::model`](src/model/README.md): a concrete `GenAiClient`, private provider
  drivers, multimodal input, and a generation stream.
- [`myco::thread`](src/thread/README.md): owned conversation values with synchronous
  appends, read-only slicing, copies, and a context for referenced blobs.

Each module's public interface lives in its `mod.rs`. Workflow logic, durable
storage, services, and the HTTP API are subsequent review steps in the design.

```sh
cargo test --locked --offline --workspace
cargo clippy --locked --offline --workspace --all-targets -- -D warnings
cargo fmt --all --check
```

Run `cargo fetch --locked` once if dependencies are not cached. Protocol tests
bind loopback HTTP sockets. They make no external API calls.

Licensed under the [Apache License 2.0](LICENSE). See [NOTICE](NOTICE) for
attribution.
