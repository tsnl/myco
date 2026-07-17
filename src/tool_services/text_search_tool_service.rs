//! Host tool service: incremental text search over watched directories.
//!
//! Thin wrapper around [`crate::text_search::TextSearchEngine`]. Auto-indexes
//! skills and AGENTS.md at host start; agents may `index_directory` more.

use std::path::PathBuf;
use std::sync::Arc;

use crate::core::Async;
use crate::generative_model::{self, ToolResult};
use crate::text_search::{SearchOptions, TextSearchEngine};

use super::{HostDispatchContext, ToolService};

const INDEX_DIRECTORY_DESC: &str = r#"
Register a directory (or single file) as a **persistently indexed root**.

- Performs the initial crawl and returns when the root is searchable.
- Installs a filesystem watcher; creates/modifies/deletes update the index
  incrementally until `drop_directory_index`.
- Symlinks are never followed. Search only works under registered roots.
- Indexing a parent of an existing root expands that index in place (children absorbed).

On host start, myco already registers (same persistent model):
- `.claude/skills`
- `.myco/skills` (if present)
- directories containing `SKILL.md` (Agent Skills layout; see https://agentskills.io)
- `AGENTS.md` / `CLAUDE.md` files under a bounded walk from cwd

**Prefer bash + rg/grep for large code trees** (node_modules, monorepos, home).
Only register scopes you will query repeatedly (skills, docs, a small package).
"#;

const EXACT_SEARCH_DESC: &str = r#"
Exact full-text search over the **Tantivy** index of persistently watched roots
(query parser / BM25). Matches **file body** and **path/filename** tokens
(e.g. `SKILL.md`, skill folder names). Requires the path to fall under a
registered root (auto skills roots count). Waits only if an initial crawl for a
covering root is still running.

Prefer this for identifiers, skill names, filenames, and literal phrases in
indexed content. For huge unindexed trees, use bash + rg instead of
index_directory.
"#;

const SEMANTIC_SEARCH_DESC: &str = r#"
Semantic search over persistently indexed roots using **Candle** (all-MiniLM-L6-v2)
embeddings and cosine similarity. Best for skills / AGENTS.md intent queries
("how do I extract PDFs"). Same coverage rules as exact search.
Weights are embedded at **compile time** (`build.rs` stages MiniLM
safetensors + tokenizer under `OUT_DIR`; no ONNX Runtime). Rebuild with curl
if assets are missing (see embed_weights/README.md / harness-ops).

Not a substitute for grep over a full repo. Prefer exact (Tantivy) search for symbols.
"#;

const DROP_DESC: &str = r#"
Stop watching a persistently indexed root and remove its files from the index.
Pass a root previously registered via index_directory / auto-index.
"#;

/// Host-placed text search tools backed by [`TextSearchEngine`].
pub struct TextSearchToolService {
    engine: TextSearchEngine,
}

impl TextSearchToolService {
    /// Build service and start engine (auto-index under process cwd).
    pub fn new() -> Self {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        Self {
            engine: TextSearchEngine::start(cwd),
        }
    }

    #[cfg(test)]
    fn new_for_tests(engine: TextSearchEngine) -> Self {
        Self { engine }
    }
}

impl Default for TextSearchToolService {
    fn default() -> Self {
        Self::new()
    }
}

impl ToolService for TextSearchToolService {
    fn tool_specs(&self) -> Vec<generative_model::ToolSpec> {
        vec![
            generative_model::ToolSpec {
                name: "index_directory".to_string(),
                description: INDEX_DIRECTORY_DESC.to_string(),
                input_schema: schemars::schema_for!(IndexDirectoryInput).to_value(),
                input_examples: vec![],
            },
            generative_model::ToolSpec {
                name: "indexed_exact_text_search".to_string(),
                description: EXACT_SEARCH_DESC.to_string(),
                input_schema: schemars::schema_for!(SearchInput).to_value(),
                input_examples: vec![],
            },
            generative_model::ToolSpec {
                name: "indexed_semantic_text_search".to_string(),
                description: SEMANTIC_SEARCH_DESC.to_string(),
                input_schema: schemars::schema_for!(SearchInput).to_value(),
                input_examples: vec![],
            },
            generative_model::ToolSpec {
                name: "drop_directory_index".to_string(),
                description: DROP_DESC.to_string(),
                input_schema: schemars::schema_for!(DropInput).to_value(),
                input_examples: vec![],
            },
        ]
    }

    fn dispatch_tool_use(
        self: Arc<Self>,
        tool_use: generative_model::ToolUse,
        _ctx: HostDispatchContext,
    ) -> Async<generative_model::ToolResult> {
        Box::pin(async move {
            match tool_use.name.as_str() {
                "index_directory" => {
                    let input: IndexDirectoryInput = match serde_json::from_value(tool_use.input) {
                        Ok(v) => v,
                        Err(e) => {
                            return ToolResult::err(format!("invalid index_directory input: {e}"));
                        }
                    };
                    match self.engine.index_directory(PathBuf::from(input.path)).await {
                        Ok(r) => ToolResult::text(format_index_report(&r)),
                        Err(e) => ToolResult::err(e),
                    }
                }
                "indexed_exact_text_search" => {
                    let input: SearchInput = match serde_json::from_value(tool_use.input) {
                        Ok(v) => v,
                        Err(e) => {
                            return ToolResult::err(format!(
                                "invalid indexed_exact_text_search input: {e}"
                            ));
                        }
                    };
                    match self.engine.search_exact(search_opts(input)).await {
                        Ok(r) => ToolResult::text(format_search_report(&r)),
                        Err(e) => ToolResult::err(e),
                    }
                }
                "indexed_semantic_text_search" => {
                    let input: SearchInput = match serde_json::from_value(tool_use.input) {
                        Ok(v) => v,
                        Err(e) => {
                            return ToolResult::err(format!(
                                "invalid indexed_semantic_text_search input: {e}"
                            ));
                        }
                    };
                    match self.engine.search_semantic(search_opts(input)).await {
                        Ok(r) => ToolResult::text(format_search_report(&r)),
                        Err(e) => ToolResult::err(e),
                    }
                }
                "drop_directory_index" => {
                    let input: DropInput = match serde_json::from_value(tool_use.input) {
                        Ok(v) => v,
                        Err(e) => {
                            return ToolResult::err(format!(
                                "invalid drop_directory_index input: {e}"
                            ));
                        }
                    };
                    match self
                        .engine
                        .drop_directory_index(PathBuf::from(input.path))
                        .await
                    {
                        Ok(r) => ToolResult::text(format!(
                            "dropped={} path={}\n{}\n",
                            r.dropped,
                            r.path.display(),
                            r.message
                        )),
                        Err(e) => ToolResult::err(e),
                    }
                }
                other => ToolResult::err(format!("unknown text search tool '{other}'")),
            }
        })
    }
}

fn search_opts(input: SearchInput) -> SearchOptions {
    SearchOptions {
        query: input.query,
        path: input.path.map(PathBuf::from),
        max_results: input.max_results.unwrap_or(20),
    }
}

fn format_index_report(r: &crate::text_search::IndexReport) -> String {
    let mut out = format!(
        "path={}\nstatus={}\nwatching={}\nfiles_in_index≈{}\nexpanded_parent={}\n",
        r.path.display(),
        r.status,
        r.watching,
        r.files_indexed,
        r.expanded_parent
    );
    if !r.absorbed_children.is_empty() {
        out.push_str("absorbed_children:\n");
        for c in &r.absorbed_children {
            out.push_str(&format!("  - {}\n", c.display()));
        }
    }
    out.push_str(
        "note: root stays indexed; filesystem watcher applies incremental updates until drop_directory_index.\n",
    );
    out
}

fn format_search_report(r: &crate::text_search::SearchReport) -> String {
    let mut out = format!("mode={}\n{}\n", r.mode, r.note);
    out.push_str("roots:\n");
    for root in &r.roots_used {
        out.push_str(&format!("  - {}\n", root.display()));
    }
    out.push_str(&format!("hits: {}\n", r.hits.len()));
    for (i, h) in r.hits.iter().enumerate() {
        out.push_str(&format!(
            "\n[{}] score={:.4} path={}",
            i + 1,
            h.score,
            h.path.display()
        ));
        if let Some(n) = h.line_number {
            out.push_str(&format!(":{}", n));
        }
        out.push('\n');
        if let Some(line) = &h.line_text {
            out.push_str(&format!("  {}\n", line.trim_end()));
        } else if !h.snippet.is_empty() {
            out.push_str(&format!("  {}\n", h.snippet.trim_end()));
        }
    }
    if r.hits.is_empty() {
        out.push_str("(no hits)\n");
    }
    out
}

#[derive(Clone, Debug, schemars::JsonSchema, serde::Deserialize, serde::Serialize)]
struct IndexDirectoryInput {
    /// Directory or file to index (absolute or cwd-relative). Symlinks not followed.
    path: String,
}

#[derive(Clone, Debug, schemars::JsonSchema, serde::Deserialize, serde::Serialize)]
struct SearchInput {
    /// Search query.
    query: String,
    /// Optional path filter under an indexed root.
    #[serde(default)]
    path: Option<String>,
    /// Max hits (default 20, max 200).
    #[serde(default)]
    max_results: Option<usize>,
}

#[derive(Clone, Debug, schemars::JsonSchema, serde::Deserialize, serde::Serialize)]
struct DropInput {
    /// Indexed root (or covered path) to drop.
    path: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CancelToken;
    use crate::generative_model::ToolUse;
    use serde_json::json;
    use std::fs;
    use std::time::Duration;

    fn ctx() -> HostDispatchContext {
        HostDispatchContext {
            agent_id: uuid::Uuid::nil(),
            cancel: CancelToken::new(),
            agent_root: None,
        }
    }

    fn tool_text(r: &generative_model::ToolResult) -> String {
        r.content
            .iter()
            .filter_map(|c| match c {
                generative_model::Content::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }

    #[tokio::test]
    async fn tools_index_and_search() {
        let dir = std::env::temp_dir().join(format!("myco-tstool-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(dir.join(".claude/skills/demo")).unwrap();
        fs::write(
            dir.join(".claude/skills/demo/SKILL.md"),
            "---\nname: demo\ndescription: Demo skill for worktrees\n---\n# Demo\nworktree helper steps\n",
        )
        .unwrap();

        let eng = TextSearchEngine::start_for_tests();
        let svc = Arc::new(TextSearchToolService::new_for_tests(eng));

        let idx = svc
            .clone()
            .dispatch_tool_use(
                ToolUse {
                    id: "1".into(),
                    name: "index_directory".into(),
                    input: json!({"path": dir.join(".claude/skills").to_string_lossy()}),
                },
                ctx(),
            )
            .await;
        assert!(!idx.is_error, "{idx:?}");

        // Wait for index
        for _ in 0..100 {
            let roots = svc.engine.list_roots().await;
            if roots.iter().any(|(_, r, _)| *r) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        let search = svc
            .clone()
            .dispatch_tool_use(
                ToolUse {
                    id: "2".into(),
                    name: "indexed_exact_text_search".into(),
                    input: json!({
                        "query": "worktree",
                        "path": dir.join(".claude/skills").to_string_lossy(),
                    }),
                },
                ctx(),
            )
            .await;
        assert!(!search.is_error, "{search:?}");
        let text = tool_text(&search);
        assert!(text.contains("SKILL.md"), "{text}");

        let sem = svc
            .dispatch_tool_use(
                ToolUse {
                    id: "3".into(),
                    name: "indexed_semantic_text_search".into(),
                    input: json!({
                        "query": "helper for git worktrees",
                        "path": dir.join(".claude/skills").to_string_lossy(),
                    }),
                },
                ctx(),
            )
            .await;
        // Semantic uses compile-time MiniLM (Candle); should work offline after a successful build.
        assert!(!sem.is_error, "semantic search: {}", tool_text(&sem));
        assert!(tool_text(&sem).contains("hits"), "{sem:?}");
    }
}
