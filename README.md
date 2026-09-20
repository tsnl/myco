# myco

A programmable conversation server with shared services and a web GUI.

The implementation is organized into separately reviewable steps in
[DESIGN.md](DESIGN.md). The engine is one `myco` crate, organized into `model`,
`thread`, `logic`, `service`, and `api` modules. The first implementation step is
the `model` module. The server and remote workers are binary targets; the browser
GUI is a separate application.

The current implementation is the standalone
[`myco-genai`](crates/myco-genai/README.md) crate. Start review at its
[public interface](crates/myco-genai/src/lib.rs) and
[request/response types](crates/myco-genai/src/types.rs).

```sh
cargo test --locked --offline --workspace
cargo clippy --locked --offline --workspace --all-targets -- -D warnings
cargo fmt --all --check
```

Run `cargo fetch --locked` once if dependencies are not cached. Protocol tests
bind loopback HTTP sockets. They make no external API calls.

Licensed under the [Apache License 2.0](LICENSE). See [NOTICE](NOTICE) for
attribution.
