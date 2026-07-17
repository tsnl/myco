# embed_weights

Semantic search uses **Candle** + **all-MiniLM-L6-v2** (no ONNX Runtime).

`build.rs` downloads via the **`hf-hub`** crate (Rust counterpart of Python
`huggingface_hub`) into the **shared Hub cache** (`HF_HUB_CACHE` /
`$HF_HOME/hub` / `~/.cache/huggingface/hub`), then stages copies under
**`OUT_DIR/embed_weights/`** and generates `OUT_DIR/embed_assets.rs` so
`include_bytes!` bakes them into the `myco` binary without modifying the
package source tree (required for `cargo publish` verify).

Only this README and `MODEL.manifest` are tracked in git.

| File | Source |
|------|--------|
| `model.safetensors` | sentence-transformers/all-MiniLM-L6-v2 (~87 MiB) |
| `tokenizer.json` | same repo |
| `config.json` | same repo |

## Caching (worktrees / CI)

Downloads are **system-wide** (or `HF_HOME`-scoped), not per-worktree:

- Default: `~/.cache/huggingface/` (same layout as Python `huggingface_hub`)
- Override: `HF_HOME`, `HF_HUB_CACHE`, or `HF_ENDPOINT` / `MYCO_EMBED_ENDPOINT`
- GitHub Actions: workflow sets `HF_HOME=$GITHUB_WORKSPACE/.hf` and caches
  `.hf/hub` (plus the optional flat seed dir below)

A second worktree / branch build reuses the hub cache automatically.

## Seeding / offline builds

Preferred: let `build.rs` fetch once via `hf-hub` (network on first build).

Optional flat seed — place files here **or** set `MYCO_EMBED_CACHE` (checked
before the Hub cache):

```bash
# optional; usually unnecessary once the hub cache is warm
bash scripts/seed-minilm-weights.sh
# or:
export MYCO_EMBED_CACHE=/path/to/dir-with-model.safetensors-tokenizer-config
```

`MYCO_EMBED_OFFLINE=1` fails the build if assets cannot be found in
`MYCO_EMBED_CACHE` / this directory / the Hub cache / `OUT_DIR`.

After a successful build, weights live **inside** the binary (no runtime model pack).
