# myco

A programmable conversation server with shared services and a web GUI.

The implementation is organized into separately reviewable steps in
[DESIGN.md](DESIGN.md). The engine is one `myco` crate, organized into `model`,
`thread`, `logic`, `service`, and `api` modules. The first implementation step is
the `model` module. The server and remote workers are binary targets; the browser
GUI is a separate application.

Licensed under the [Apache License 2.0](LICENSE). See [NOTICE](NOTICE) for
attribution.
