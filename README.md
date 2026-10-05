# myco

An extensible HTTP server and Rust library for building agents.

The implementation is organized into separately reviewable steps in
[DESIGN.md](DESIGN.md). The engine is one `myco` crate, with planned `logic`,
`kernel`, `app`, and `api` modules building on the foundation below. Installable
apps supply tools; scripts, integrations, and a later web GUI use the same API.

Implemented so far:

- [`myco::blob`](src/blob/README.md): a shared store of immutable, content-addressed blobs.
- [`myco::gen_ai`](src/gen_ai/README.md): a concrete `GenAiClient`, private provider
  drivers, multimodal input through blob references, and a generation stream.
- [`myco::thread`](src/thread/README.md): owned conversation values with synchronous
  appends, read-only slicing, copies, and blob references.

Each module's public interface lives in its `mod.rs`. Pure workflow logic, durable
execution, apps, and the HTTP API are subsequent review steps in the design.

```sh
cargo test --locked --offline --workspace
cargo clippy --locked --offline --workspace --all-targets -- -D warnings
cargo fmt --all --check
```

Run `cargo fetch --locked` once if dependencies are not cached. Protocol tests
bind loopback HTTP sockets. They make no external API calls.

Licensed under the [Apache License 2.0](LICENSE). See [NOTICE](NOTICE) for
attribution.
