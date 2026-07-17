#!/usr/bin/env bash
# Prefetch MiniLM assets so build.rs (hf-hub) can hit a warm cache.
#
# Primary seed path remains a flat directory that build.rs accepts via
# MYCO_EMBED_CACHE or the gitignored src/text_search/embed_weights/ tree.
# After the first real cargo build, blobs also live under the shared Hub
# cache (HF_HUB_CACHE / $HF_HOME/hub / ~/.cache/huggingface/hub), which is
# what worktrees and GHA should reuse.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
MODEL_ID="sentence-transformers/all-MiniLM-L6-v2"
MANIFEST="$ROOT/src/text_search/embed_weights/MODEL.manifest"
SEED_DIR="${MYCO_EMBED_CACHE:-$ROOT/src/text_search/embed_weights}"

ENDPOINT="${MYCO_EMBED_ENDPOINT:-${HF_ENDPOINT:-https://huggingface.co}}"
ENDPOINT="${ENDPOINT%/}"

sha256_file() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | awk '{print $1}'
  else
    shasum -a 256 "$1" | awk '{print $1}'
  fi
}

mkdir -p "$SEED_DIR"

need_download=0
while read -r hash name rest; do
  [[ -z "${hash:-}" || "$hash" == \#* ]] && continue
  name="${name#./}"
  path="$SEED_DIR/$name"
  if [[ ! -f "$path" ]]; then
    echo "missing $name"
    need_download=1
    continue
  fi
  got="$(sha256_file "$path")"
  if [[ "$got" != "$hash" ]]; then
    echo "sha256 mismatch $name (got $got want $hash)"
    need_download=1
  fi
done < <(grep -v '^[[:space:]]*#' "$MANIFEST" | grep -v '^[[:space:]]*$')

if [[ "$need_download" -eq 0 ]]; then
  echo "MiniLM embed weights already present under $SEED_DIR"
  ls -la "$SEED_DIR"
  exit 0
fi

echo "Seeding MiniLM assets for $MODEL_ID into $SEED_DIR"
for name in model.safetensors tokenizer.json config.json; do
  dest="$SEED_DIR/$name"
  tmp="$dest.partial"
  rm -f "$tmp"
  curl -fL --retry 5 --retry-delay 2 --retry-all-errors --connect-timeout 30 \
    -o "$tmp" "$ENDPOINT/$MODEL_ID/resolve/main/$name"
  mv "$tmp" "$dest"
done

while read -r hash name rest; do
  [[ -z "${hash:-}" || "$hash" == \#* ]] && continue
  name="${name#./}"
  path="$SEED_DIR/$name"
  got="$(sha256_file "$path")"
  if [[ "$got" != "$hash" ]]; then
    echo "ERROR: after download, sha256 mismatch for $name (got $got want $hash)" >&2
    exit 1
  fi
done < <(grep -v '^[[:space:]]*#' "$MANIFEST" | grep -v '^[[:space:]]*$')

echo "MiniLM embed weights seeded under $SEED_DIR"
ls -la "$SEED_DIR"
