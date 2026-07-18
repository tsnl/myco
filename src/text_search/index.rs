//! Tantivy (exact) + Candle MiniLM (semantic) index over watched files.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use tantivy::collector::TopDocs;
use tantivy::query::{Query, QueryParser};
use tantivy::schema::{
    Field, IndexRecordOption, STORED, STRING, Schema, TantivyDocument, TextFieldIndexing,
    TextOptions, Value,
};
use tantivy::{Index, IndexWriter, ReloadPolicy, Term, doc};

/// Hard caps so accidental huge trees don't blow memory.
pub const MAX_FILE_BYTES: u64 = 512 * 1024;
pub const MAX_TOTAL_BYTES: u64 = 64 * 1024 * 1024;
pub const MAX_FILES: usize = 20_000;
/// Max chars embedded per file (skills / docs are short; avoid huge ONNX batches).
const MAX_EMBED_CHARS: usize = 8_000;

const SKIP_DIR_NAMES: &[&str] = &[
    ".git",
    "target",
    "node_modules",
    "dist",
    "build",
    ".hg",
    ".svn",
    ".jj",
    "__pycache__",
    ".venv",
    "venv",
    ".tox",
    ".mypy_cache",
    ".pytest_cache",
    ".cargo",
];

#[derive(Debug, Clone)]
pub struct Hit {
    pub path: PathBuf,
    pub score: f32,
    pub line_number: Option<usize>,
    pub line_text: Option<String>,
    pub snippet: String,
}

struct FileRecord {
    path: PathBuf,
    #[allow(dead_code)]
    text: String,
    lines: Vec<String>,
}

/// Combined exact (Tantivy) + semantic (Candle MiniLM vectors) store.
pub struct SearchIndex {
    tantivy: Index,
    writer: IndexWriter,
    /// Full path key; STRING so deletes/lookups are exact (not tokenized).
    path_field: Field,
    /// Tokenized path + basename for filename / path-component queries.
    path_text_field: Field,
    body_field: Field,
    /// path_key -> file body (snippets / line hits / embed source)
    files: HashMap<String, FileRecord>,
    /// path_key -> L2-normalized embedding
    vectors: HashMap<String, Vec<f32>>,
    total_bytes: u64,
}

impl SearchIndex {
    pub fn new() -> Result<Self, String> {
        let mut schema_builder = Schema::builder();
        let path_field = schema_builder.add_text_field("path", STRING | STORED);
        let text_opts = TextOptions::default().set_stored().set_indexing_options(
            TextFieldIndexing::default()
                .set_tokenizer("default")
                .set_index_option(IndexRecordOption::WithFreqsAndPositions),
        );
        // Not stored: path_field already holds the full path for retrieval.
        let path_text_opts = TextOptions::default().set_indexing_options(
            TextFieldIndexing::default()
                .set_tokenizer("default")
                .set_index_option(IndexRecordOption::WithFreqsAndPositions),
        );
        let path_text_field = schema_builder.add_text_field("path_text", path_text_opts);
        let body_field = schema_builder.add_text_field("body", text_opts);
        let schema = schema_builder.build();
        let tantivy = Index::create_in_ram(schema);
        let writer = tantivy
            .writer(50_000_000)
            .map_err(|e| format!("tantivy writer: {e}"))?;
        Ok(Self {
            tantivy,
            writer,
            path_field,
            path_text_field,
            body_field,
            files: HashMap::new(),
            vectors: HashMap::new(),
            total_bytes: 0,
        })
    }

    pub fn file_count(&self) -> usize {
        self.files.len()
    }

    pub fn contains_file(&self, path: &Path) -> bool {
        self.files.contains_key(&path_key(path))
    }

    pub fn remove_file(&mut self, path: &Path) {
        let key = path_key(path);
        if let Some(rec) = self.files.remove(&key) {
            self.total_bytes = self.total_bytes.saturating_sub(rec.text.len() as u64);
        }
        self.vectors.remove(&key);
        let term = Term::from_field_text(self.path_field, &key);
        self.writer.delete_term(term);
    }

    pub fn remove_tree(&mut self, root: &Path) {
        let prefix = path_key(root);
        let keys: Vec<String> = self
            .files
            .keys()
            .filter(|k| *k == &prefix || k.starts_with(&(prefix.clone() + "/")))
            .cloned()
            .collect();
        for k in keys {
            if let Some(rec) = self.files.get(&k) {
                let p = rec.path.clone();
                self.remove_file(&p);
            }
        }
        let _ = self.commit();
    }

    /// Upsert file. Returns false if skipped (binary, too large, caps).
    ///
    /// `vector` is the file's precomputed [`embed_for_index`] embedding
    /// (`None` ⇒ exact search only). Embedding is the caller's job so this
    /// method stays cheap under the engine lock — see [`embed_for_index`].
    pub fn upsert_file(&mut self, path: &Path, text: String, vector: Option<Vec<f32>>) -> bool {
        if self.files.len() >= MAX_FILES && !self.contains_file(path) {
            return false;
        }
        let bytes = text.len() as u64;
        if bytes > MAX_FILE_BYTES {
            return false;
        }
        if text.chars().take(8192).any(|c| c == '\0') {
            return false;
        }
        let non_text = text
            .chars()
            .take(4096)
            .filter(|c| {
                (*c as u32) < 9 || ((*c as u32) < 32 && *c != '\n' && *c != '\r' && *c != '\t')
            })
            .count();
        if non_text > 16 {
            return false;
        }

        // Cap math accounts for replacing the existing version, and is checked
        // *before* the removal: an over-cap rewrite keeps the old version
        // searchable instead of deleting it and then indexing nothing.
        let old_bytes = self
            .files
            .get(&path_key(path))
            .map(|rec| rec.text.len() as u64)
            .unwrap_or(0);
        if self.total_bytes - old_bytes + bytes > MAX_TOTAL_BYTES {
            return false;
        }
        if self.contains_file(path) {
            self.remove_file(path);
        }

        let key = path_key(path);
        let path_text = path_search_text(path);
        let lines: Vec<String> = text.lines().map(|l| l.to_string()).collect();

        let term = Term::from_field_text(self.path_field, &key);
        self.writer.delete_term(term);
        if self
            .writer
            .add_document(doc!(
                self.path_field => key.as_str(),
                self.path_text_field => path_text.as_str(),
                self.body_field => text.as_str(),
            ))
            .is_err()
        {
            return false;
        }

        // Best-effort semantic vector; exact search still works without one.
        if let Some(vec) = vector {
            self.vectors.insert(key.clone(), vec);
        }

        self.total_bytes += bytes;
        self.files.insert(
            key,
            FileRecord {
                path: path.to_path_buf(),
                text,
                lines,
            },
        );
        true
    }

    pub fn commit(&mut self) -> Result<(), String> {
        self.writer
            .commit()
            .map_err(|e| format!("tantivy commit: {e}"))?;
        Ok(())
    }

    /// Exact search via Tantivy query parser + line-level enrichment.
    pub fn search_exact(
        &mut self,
        query: &str,
        path_prefix: Option<&Path>,
        limit: usize,
    ) -> Result<Vec<Hit>, String> {
        let q = query.trim();
        if q.is_empty() || limit == 0 {
            return Ok(Vec::new());
        }
        self.commit()?;

        let reader = self
            .tantivy
            .reader_builder()
            .reload_policy(ReloadPolicy::Manual)
            .try_into()
            .map_err(|e| format!("tantivy reader: {e}"))?;
        reader
            .reload()
            .map_err(|e| format!("tantivy reload: {e}"))?;
        let searcher = reader.searcher();

        // Default fields: body content + tokenized path/filename.
        let parser =
            QueryParser::for_index(&self.tantivy, vec![self.body_field, self.path_text_field]);
        let tq: Box<dyn Query> = match parser.parse_query(q) {
            Ok(qq) => qq,
            Err(_) => {
                let escaped = q.replace('"', " ");
                parser
                    .parse_query(&escaped)
                    .or_else(|_| parser.parse_query(&format!("\"{escaped}\"")))
                    .map_err(|e| format!("invalid query: {e}"))?
            }
        };

        let top = searcher
            .search(
                &tq,
                &TopDocs::with_limit(limit.saturating_mul(4).max(limit)),
            )
            .map_err(|e| format!("tantivy search: {e}"))?;

        let prefix = path_prefix.map(path_key);
        let mut hits = Vec::new();
        for (score, addr) in top {
            let doc: TantivyDocument = searcher
                .doc(addr)
                .map_err(|e| format!("tantivy doc: {e}"))?;
            let Some(path_val) = doc.get_first(self.path_field) else {
                continue;
            };
            let Some(path_str) = path_val.as_str() else {
                continue;
            };
            if let Some(ref p) = prefix
                && path_str != p.as_str()
                && !path_str.starts_with(&(p.clone() + "/"))
            {
                continue;
            }
            let path = PathBuf::from(path_str);
            let rec = self.files.get(path_str);
            let (line_number, line_text, snippet) = match rec {
                Some(rec) => {
                    let body_hit = line_hit(&rec.lines, q);
                    // Path-only matches have no body line; show path as snippet.
                    if body_hit.0.is_none() && path_matches_query(path_str, q) {
                        (
                            None,
                            None,
                            truncate(
                                path.file_name()
                                    .and_then(|n| n.to_str())
                                    .unwrap_or(path_str),
                                240,
                            ),
                        )
                    } else {
                        body_hit
                    }
                }
                None => {
                    let body = doc
                        .get_first(self.body_field)
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    (None, None, truncate(body.lines().next().unwrap_or(""), 240))
                }
            };
            hits.push(Hit {
                path,
                score,
                line_number,
                line_text,
                snippet,
            });
            if hits.len() >= limit {
                break;
            }
        }
        Ok(hits)
    }

    /// Semantic search: cosine over MiniLM vectors.
    ///
    /// `q_vec` is the query's precomputed [`embed_for_index`] embedding
    /// (embedding is the caller's job — see [`embed_for_index`]).
    pub fn search_semantic(
        &mut self,
        query: &str,
        q_vec: &[f32],
        path_prefix: Option<&Path>,
        limit: usize,
    ) -> Result<Vec<Hit>, String> {
        let q = query.trim();
        if q.is_empty() || limit == 0 {
            return Ok(Vec::new());
        }
        let prefix = path_prefix.map(path_key);

        let mut scored: Vec<(f32, String)> = Vec::new();
        for (key, vec) in &self.vectors {
            if let Some(ref p) = prefix
                && key != p
                && !key.starts_with(&(p.clone() + "/"))
            {
                continue;
            }
            let score = cosine(q_vec, vec);
            if score > 0.0 {
                scored.push((score, key.clone()));
            }
        }
        scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        scored.truncate(limit);

        let mut hits = Vec::with_capacity(scored.len());
        for (score, key) in scored {
            let Some(rec) = self.files.get(&key) else {
                continue;
            };
            let snippet = first_queryish_line(&rec.lines, q);
            hits.push(Hit {
                path: rec.path.clone(),
                score,
                line_number: None,
                line_text: None,
                snippet,
            });
        }
        Ok(hits)
    }
}

fn line_hit(lines: &[String], query: &str) -> (Option<usize>, Option<String>, String) {
    let terms: Vec<String> = query
        .split_whitespace()
        .map(|t| {
            t.trim_matches(|c: char| !c.is_alphanumeric() && c != '_' && c != '$')
                .to_lowercase()
        })
        .filter(|t| t.len() >= 2)
        .collect();
    if !terms.is_empty() {
        for (i, line) in lines.iter().enumerate() {
            let lower = line.to_lowercase();
            if terms.iter().all(|t| lower.contains(t)) {
                return (Some(i + 1), Some(line.clone()), truncate(line, 240));
            }
        }
        for (i, line) in lines.iter().enumerate() {
            let lower = line.to_lowercase();
            if terms.iter().any(|t| lower.contains(t)) {
                return (Some(i + 1), Some(line.clone()), truncate(line, 240));
            }
        }
    }
    let snip = lines.first().map(|l| truncate(l, 240)).unwrap_or_default();
    (None, None, snip)
}

fn first_queryish_line(lines: &[String], query: &str) -> String {
    let terms: Vec<String> = query
        .split_whitespace()
        .map(|t| t.to_lowercase())
        .filter(|t| t.len() >= 2)
        .collect();
    for line in lines {
        let lower = line.to_lowercase();
        if terms.iter().any(|t| lower.contains(t)) {
            return truncate(line, 240);
        }
    }
    lines.first().map(|l| truncate(l, 240)).unwrap_or_default()
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let t: String = s.chars().take(max).collect();
        format!("{t}…")
    }
}

fn path_key(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

/// Text indexed for path/filename search.
///
/// Emits the full path with separators turned into spaces (so the default
/// tokenizer can split components) plus the basename again so names like
/// `SKILL.md` remain findable as a single token-friendly phrase.
fn path_search_text(path: &Path) -> String {
    let key = path_key(path);
    let spaced = key
        .chars()
        .map(|c| if c == '/' || c == '\\' { ' ' } else { c })
        .collect::<String>();
    match path.file_name().and_then(|n| n.to_str()) {
        Some(name) if !name.is_empty() => format!("{spaced} {name}"),
        _ => spaced,
    }
}

fn path_matches_query(path_str: &str, query: &str) -> bool {
    let lower_path = path_str.to_lowercase();
    let name = Path::new(path_str)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("")
        .to_lowercase();
    let terms: Vec<String> = query
        .split_whitespace()
        .map(|t| {
            t.trim_matches(|c: char| !c.is_alphanumeric() && c != '_' && c != '$' && c != '.')
                .to_lowercase()
        })
        .filter(|t| t.len() >= 2)
        .collect();
    if terms.is_empty() {
        let q = query.trim().to_lowercase();
        return !q.is_empty() && (lower_path.contains(&q) || name.contains(&q));
    }
    terms
        .iter()
        .all(|t| lower_path.contains(t) || name.contains(t))
}

fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let mut dot = 0.0f32;
    for (x, y) in a.iter().zip(b.iter()) {
        dot += x * y;
    }
    // vectors stored L2-normalized
    dot
}

/// Clip and embed file/query text for the semantic index (L2-normalized
/// MiniLM vector via Candle).
///
/// This is heavy synchronous CPU work — the first call in a process also
/// loads the BERT model, which takes seconds (tens of seconds in debug
/// builds). Callers on an async runtime must run it via `spawn_blocking`:
/// on an executor thread it starves timers and sibling tasks, and wedges
/// current-thread runtimes outright.
pub(crate) fn embed_for_index(text: &str) -> Result<Vec<f32>, String> {
    let clipped: String = text.chars().take(MAX_EMBED_CHARS).collect();
    crate::text_search::embed_model::embed_text(&clipped)
}

/// Collect text files under `root` without following symlinks.
pub fn walk_text_files(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if root.is_file() {
        out.push(root.to_path_buf());
        return out;
    }
    walk_dir(root, &mut out);
    out
}

fn walk_dir(dir: &Path, out: &mut Vec<PathBuf>) {
    let read = match std::fs::read_dir(dir) {
        Ok(r) => r,
        Err(_) => return,
    };
    let mut subdirs = Vec::new();
    for entry in read.flatten() {
        let path = entry.path();
        let Ok(ft) = entry.file_type() else { continue };
        if ft.is_symlink() {
            continue;
        }
        if ft.is_file() {
            out.push(path);
        } else if ft.is_dir() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if SKIP_DIR_NAMES.iter().any(|s| name == *s) {
                continue;
            }
            if name == "worktrees" && dir.ends_with(".myco") {
                continue;
            }
            subdirs.push(path);
        }
    }
    for s in subdirs {
        if out.len() >= MAX_FILES {
            break;
        }
        walk_dir(&s, out);
    }
}

/// Read a file if it is a reasonable text candidate.
pub fn read_text_file(path: &Path) -> Option<String> {
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() || meta.len() > MAX_FILE_BYTES {
        return None;
    }
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        const BIN: &[&str] = &[
            "png", "jpg", "jpeg", "gif", "webp", "ico", "pdf", "zip", "gz", "tgz", "bz2", "xz",
            "7z", "rar", "woff", "woff2", "ttf", "otf", "eot", "mp3", "mp4", "mov", "avi", "wasm",
            "so", "dylib", "a", "o", "class", "jar", "exe", "dll", "bin", "pyc", "pyo",
        ];
        if BIN.iter().any(|b| ext.eq_ignore_ascii_case(b)) {
            return None;
        }
    }
    let bytes = std::fs::read(path).ok()?;
    if bytes.contains(&0) {
        return None;
    }
    String::from_utf8(bytes).ok()
}

pub fn resolve_path(path: &Path) -> Result<PathBuf, String> {
    let abs = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|e| format!("cwd: {e}"))?
            .join(path)
    };
    match abs.canonicalize() {
        Ok(c) => Ok(c),
        Err(_) => Ok(normalize_lexically(&abs)),
    }
}

fn normalize_lexically(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

pub fn is_under(root: &Path, child: &Path) -> bool {
    let r = path_key(root);
    let c = path_key(child);
    c == r || c.starts_with(&(r + "/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_search_basic() {
        let mut idx = SearchIndex::new().unwrap();
        idx.upsert_file(
            Path::new("/tmp/a/SKILL.md"),
            "---\nname: pdf\ndescription: Extract PDF text and forms\n---\n# PDF skill\n".into(),
            None,
        );
        idx.upsert_file(
            Path::new("/tmp/a/other.md"),
            "unrelated content about bananas\n".into(),
            None,
        );
        idx.commit().unwrap();
        let hits = idx.search_exact("PDF forms", None, 10).unwrap();
        assert!(!hits.is_empty(), "{hits:?}");
        assert!(hits[0].path.ends_with("SKILL.md"), "{hits:?}");
    }

    #[test]
    fn exact_search_by_filename() {
        let mut idx = SearchIndex::new().unwrap();
        idx.upsert_file(
            Path::new("/tmp/skills/publish/SKILL.md"),
            "---\nname: publish\ndescription: ship releases\n---\n".into(),
            None,
        );
        idx.upsert_file(
            Path::new("/tmp/skills/publish/notes.txt"),
            "no skill marker here, just notes\n".into(),
            None,
        );
        idx.commit().unwrap();

        let hits = idx.search_exact("SKILL.md", None, 10).unwrap();
        assert_eq!(hits.len(), 1, "{hits:?}");
        assert!(hits[0].path.ends_with("SKILL.md"), "{hits:?}");

        let hits = idx.search_exact("publish", None, 10).unwrap();
        assert!(
            hits.iter().any(|h| h.path.ends_with("SKILL.md")),
            "path component 'publish' should match: {hits:?}"
        );
    }

    #[test]
    fn path_search_text_includes_basename() {
        let t = path_search_text(Path::new("/tmp/skills/publish/SKILL.md"));
        assert!(t.contains("SKILL.md"), "{t}");
        assert!(t.contains("publish"), "{t}");
        assert!(!t.contains('/'), "{t}");
    }

    #[test]
    fn semantic_search_basic() {
        let mut idx = SearchIndex::new().unwrap();
        // Uses compile-time embedded MiniLM (Candle; build.rs).
        embed_for_index("warmup").expect("MiniLM embedder must load offline");
        let skill = "---\nname: pdf\ndescription: Extract PDF text and fill forms\n---\n";
        let other = "recipe for banana bread and muffins\n";
        idx.upsert_file(
            Path::new("/tmp/a/SKILL.md"),
            skill.into(),
            embed_for_index(skill).ok(),
        );
        idx.upsert_file(
            Path::new("/tmp/a/other.md"),
            other.into(),
            embed_for_index(other).ok(),
        );
        let q = "extract documents and forms";
        let q_vec = embed_for_index(q).unwrap();
        let hits = idx.search_semantic(q, &q_vec, None, 5).unwrap();
        assert!(!hits.is_empty(), "{hits:?}");
        assert!(hits[0].path.ends_with("SKILL.md"), "{hits:?}");
    }
}
