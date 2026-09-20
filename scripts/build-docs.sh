#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."

# One artifact keeps book-to-rustdoc links on the same source revision.
mdbook build docs
RUSTDOCFLAGS="${RUSTDOCFLAGS:-} -D warnings" \
    cargo doc --locked --workspace --no-deps --lib --target-dir target
rm -rf target/site/api
cp -R target/doc target/site/api
cp docs/theme/api-index.html target/site/api/index.html
touch target/site/.nojekyll
python3 scripts/check-docs.py
