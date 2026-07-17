#!/usr/bin/env bash
# Ensure MiniLM assets exist under src/text_search/embed_weights/ for build.rs
# (seed/cache). Files are gitignored; only README + MODEL.manifest are tracked.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DIR="$ROOT/src/text_search/embed_weights"
MANIFEST="$DIR/MODEL.manifest"
BASE="${MYCO_EMBED_BASE_URL:-https://huggingface.co/sentence-transformers/all-MiniLM-L6-v2/resolve/main}"

mkdir -p "$DIR"

need_download=0
while read -r hash name rest; do
  [[ -z "${hash:-}" || "$hash" == \#* ]] && continue
  # strip optional ./ prefix
  name="${name#./}"
  path="$DIR/$name"
  if [[ ! -f "$path" ]]; then
    echo "missing $name"
    need_download=1
    continue
  fi
  got="$(sha256sum "$path" | awk '{print $1}')"
  if [[ "$got" != "$hash" ]]; then
    echo "sha256 mismatch $name (got $got want $hash)"
    need_download=1
  fi
done < <(grep -v '^[[:space:]]*#' "$MANIFEST" | grep -v '^[[:space:]]*$')

if [[ "$need_download" -eq 0 ]]; then
  echo "MiniLM embed weights already present and match MODEL.manifest"
  ls -la "$DIR"
  exit 0
fi

echo "Seeding MiniLM assets from $BASE into $DIR"
for name in model.safetensors tokenizer.json config.json; do
  dest="$DIR/$name"
  tmp="$dest.partial"
  rm -f "$tmp"
  curl -fL --retry 5 --retry-delay 2 --retry-all-errors --connect-timeout 30 \
    -o "$tmp" "$BASE/$name"
  mv "$tmp" "$dest"
done

# Verify against manifest
while read -r hash name rest; do
  [[ -z "${hash:-}" || "$hash" == \#* ]] && continue
  name="${name#./}"
  path="$DIR/$name"
  got="$(sha256sum "$path" | awk '{print $1}')"
  if [[ "$got" != "$hash" ]]; then
    echo "ERROR: after download, sha256 mismatch for $name (got $got want $hash)" >&2
    exit 1
  fi
done < <(grep -v '^[[:space:]]*#' "$MANIFEST" | grep -v '^[[:space:]]*$')

echo "MiniLM embed weights seeded"
ls -la "$DIR"
