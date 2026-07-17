#!/usr/bin/env bash
# Install repo pre-commit hooks (formatting + lint) into this checkout's git hooks dir.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
HOOK_SRC="$ROOT/scripts/pre-commit"

if [[ ! -f "$HOOK_SRC" ]]; then
  echo "ERROR: missing hook script at $HOOK_SRC" >&2
  exit 1
fi

if ! git -C "$ROOT" rev-parse --is-inside-work-tree >/dev/null 2>&1; then
  echo "ERROR: $ROOT is not inside a git work tree" >&2
  exit 1
fi

HOOK_DIR="$(git -C "$ROOT" rev-parse --git-path hooks)"
mkdir -p "$HOOK_DIR"
HOOK_DST="$HOOK_DIR/pre-commit"

# Copy (not symlink) so linked worktrees share a stable hook under .git/hooks.
# Remove any existing file/symlink first so `install` does not follow a stale link.
rm -f "$HOOK_DST"
install -m 755 "$HOOK_SRC" "$HOOK_DST"
echo "Installed pre-commit hook: $HOOK_DST"
echo "Runs: cargo fmt --check, cargo clippy (same bar as CI). Bypass: git commit --no-verify"
