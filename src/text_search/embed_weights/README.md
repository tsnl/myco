# embed_weights

Semantic search uses **Candle** + **all-MiniLM-L6-v2** (no ONNX Runtime).

Weights are **not** compiled into the binary and **not** stored in git (LFS or
otherwise). `src/text_search/embed_assets.rs` fetches them on the first
semantic search into:

```
$MYCO_EMBED_CACHE  →  else  ~/.myco/models/all-MiniLM-L6-v2/
```

and verifies every file against the sha256 digests in `MODEL.manifest`, which
is compiled in with `include_str!` and is the single source of truth shared
with `scripts/seed-minilm-weights.sh`. A file that is missing or fails its
digest is re-downloaded; writes are atomic, so concurrent `myco` processes
can't observe a half-written blob.

This directory holds only the manifest and this README — building `myco`
needs no network, and there is no `build.rs`.

| File | Source |
|------|--------|
| `model.safetensors` | sentence-transformers/all-MiniLM-L6-v2 (~87 MiB) |
| `tokenizer.json` | same repo |
| `config.json` | same repo |

## Environment

| Var | Effect |
|-----|--------|
| `MYCO_EMBED_CACHE` | Directory to read/write instead of `~/.myco/models/…` |
| `MYCO_EMBED_OFFLINE=1` | Never download; error if the cache is incomplete |
| `MYCO_EMBED_ENDPOINT` / `HF_ENDPOINT` | Mirror base URL (default `https://huggingface.co`) |

The cache deliberately ignores `MYCO_HOME`: tests and `--mode host` workers
retarget that at throwaway directories, and an 87 MiB download must not follow
session storage around.

## Seeding

Optional — only to avoid paying for the download on first search (CI does this
so the test suite starts warm):

```bash
bash scripts/seed-minilm-weights.sh          # → ~/.myco/models/all-MiniLM-L6-v2
MYCO_EMBED_CACHE=/opt/minilm bash scripts/seed-minilm-weights.sh
```

Copying the three files between machines works too — the digests are checked,
not the provenance.
