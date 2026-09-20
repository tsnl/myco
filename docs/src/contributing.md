# Contributing to the docs

The site combines an mdBook guide and rustdoc for the three workspace libraries.
It uses GitHub Actions to publish a single artifact to GitHub Pages.

## Build and preview

Install stable Rust, Python 3.11 or later, and the pinned mdBook version:

```bash
cargo install mdbook --locked --version 0.5.4
bash scripts/build-docs.sh
python3 -m http.server 8000 --directory target/site
```

Open `http://localhost:8000`. The build checks book links and anchors, including
links into rustdoc, and verifies that every registered
runtime manual article appears in the book with the agent-access note.
Generated files stay in the ignored `target/` directory.
Rustdoc builds with warnings denied to catch broken API documentation links;
its generated JavaScript navigation is not traversed by the book link checker.

For fast prose edits, `mdbook serve docs --open` rebuilds the book automatically.
That command previews the book; use the full build and static server above for
the combined API reference. Rust examples are included directly from their source:

```bash
cargo check --locked -p myco-model -p myco-agent --examples
cargo run --locked -p myco-agent --example headless
```

The headless example is offline. The inference example requires an endpoint and
credentials you supply and is compiled, but not run, in CI.

## Edit the right source

| Content | Source |
| --- | --- |
| Navigation | `docs/src/SUMMARY.md` |
| User guide | `docs/src/guide/` |
| Reuse and architecture guides | `docs/src/developers/` |
| Bundled runtime manual | `src/manual/articles/` |
| API contracts | Rust doc comments in `crates/` and `src/` |
| Runnable examples | Each library's `examples/` directory |
| Theme and book settings | `docs/theme/myco.css`, `docs/book.toml` |

Manual chapters are thin wrappers: an agent-access note followed by a full
`{{#include}}` of the original article. Keep edits in the runtime source so
the binary and website stay aligned. When registering a new runtime article,
add its wrapper and navigation entry too. Website-only explanations belong in
the guide. Do not copy manual text into a second maintained version.

## GitHub Pages

The repository's Pages publishing source must be **GitHub Actions** (Settings →
Pages → Build and deployment). The workflow in `.github/workflows/docs.yml`
validates pull requests and attaches a downloadable `myco-docs` artifact.
Only `main` pushes or manual runs on `main` deploy to the `github-pages`
environment. A pull request does not publish a website.

The expected site is `https://tsnl.github.io/myco/`. Document-relative links
work there and under a local preview; `site-url = "/myco/"` also gives the
generated 404 page the correct asset paths. Update that setting if the hosting
prefix changes. Restrict the deployment environment to `main` in repository
settings if it has additional deployment rules.

The build uses mdBook's [built-in includes](https://rust-lang.github.io/mdBook/format/mdbook.html#including-files)
and GitHub's [Pages workflow](https://docs.github.com/en/pages/getting-started-with-github-pages/using-custom-workflows-with-github-pages)
actions. It needs no JavaScript package manager, separate publishing branch,
or runtime server.
