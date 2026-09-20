# Rust API reference

The reference is generated from the same workspace revision as this book.
Each crate has symbol search, method signatures, trait implementations, and
links to the corresponding Rust source.

| Crate | Reference | Guide |
| --- | --- | --- |
| `myco-model` | [Messages and model drivers](../api/myco_model/index.html) | [Inference API](inference.md) |
| `myco-agent` | [Headless execution](../api/myco_agent/index.html) | [Agent API](agents.md) |
| `myco` | [Application components](../api/myco/index.html) | [Architecture](architecture.md) |

The book search covers prose and the bundled manual. Inside the Rust reference,
use rustdoc's search to find symbols across all three workspace crates.

To generate only the reference locally:

```bash
cargo doc --locked --workspace --no-deps --lib --open
```

To preview the complete book with working API links, follow the
[documentation build instructions](../contributing.md).
