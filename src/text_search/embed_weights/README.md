# embed_weights

Semantic search uses **Candle** + **all-MiniLM-L6-v2** (no ONNX Runtime).

`build.rs` fetches assets into this directory; `include_bytes!` bakes them into
the `honk` binary. Only this README and `MODEL.manifest` are tracked in git.

| File | Source |
|------|--------|
| `model.safetensors` | sentence-transformers/all-MiniLM-L6-v2 (~87 MiB) |
| `tokenizer.json` | same repo |
| `config.json` | same repo |

```bash
BASE=https://huggingface.co/sentence-transformers/all-MiniLM-L6-v2/resolve/main
mkdir -p src/text_search/embed_weights
curl -fL -o src/text_search/embed_weights/model.safetensors "$BASE/model.safetensors"
curl -fL -o src/text_search/embed_weights/tokenizer.json "$BASE/tokenizer.json"
curl -fL -o src/text_search/embed_weights/config.json "$BASE/config.json"
```

After a successful build, weights live **inside** the binary (no runtime model pack).
