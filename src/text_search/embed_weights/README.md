# embed_weights

Semantic search uses **Candle** + **all-MiniLM-L6-v2** (no ONNX Runtime).

`build.rs` stages assets under **`OUT_DIR/embed_weights/`** (not this source
directory) and generates `OUT_DIR/embed_assets.rs` so `include_bytes!` bakes
them into the `myco` binary without modifying the package source tree
(required for `cargo publish` verify). Only this README and `MODEL.manifest`
are tracked in git.

| File | Source |
|------|--------|
| `model.safetensors` | sentence-transformers/all-MiniLM-L6-v2 (~87 MiB) |
| `tokenizer.json` | same repo |
| `config.json` | same repo |

## Seeding / offline builds

Preferred: let `build.rs` download on first build (`curl` + network).

Optional developer cache — place files here **or** set `MYCO_EMBED_CACHE`:

```bash
BASE=https://huggingface.co/sentence-transformers/all-MiniLM-L6-v2/resolve/main
mkdir -p src/text_search/embed_weights
curl -fL -o src/text_search/embed_weights/model.safetensors "$BASE/model.safetensors"
curl -fL -o src/text_search/embed_weights/tokenizer.json "$BASE/tokenizer.json"
curl -fL -o src/text_search/embed_weights/config.json "$BASE/config.json"
# or: export MYCO_EMBED_CACHE=/path/to/dir-with-those-files
```

`MYCO_EMBED_OFFLINE=1` fails the build if assets cannot be found in
`MYCO_EMBED_CACHE` / this directory / `OUT_DIR`.

After a successful build, weights live **inside** the binary (no runtime model pack).
