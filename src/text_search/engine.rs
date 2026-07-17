//! Text search engine: incremental index, watchers, parent expand.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use tokio::sync::{Mutex, Notify};

use super::discover::discover_auto_index_targets;
use super::index::{Hit, SearchIndex, is_under, read_text_file, resolve_path, walk_text_files};

/// Shared engine handle (cheap to clone).
///
/// Holds a forest of **persistently indexed roots**. Each root is watched with
/// `notify` and kept incrementally up to date until [`TextSearchEngine::drop_directory_index`].
#[derive(Clone)]
pub struct TextSearchEngine {
    inner: Arc<Mutex<EngineState>>,
    /// Wakes waiters when an initial crawl finishes (or a root is dropped).
    job_notify: Arc<Notify>,
}

struct EngineState {
    index: SearchIndex,
    /// Persistently indexed roots (canonical). Non-nested forest preferred;
    /// parent expand absorbs children.
    roots: Vec<IndexedRoot>,
    /// Roots still doing their initial crawl (path key -> generation).
    pending: HashMap<String, u64>,
    job_gen: u64,
    /// Live filesystem watchers keyed by root path string.
    watchers: HashMap<String, RecommendedWatcher>,
    /// Event sink for watchers (path to reindex / remove).
    event_tx: Option<std::sync::mpsc::Sender<WatchMsg>>,
}

struct IndexedRoot {
    path: PathBuf,
    /// Initial crawl finished; watcher continues to update the index.
    ready: bool,
    error: Option<String>,
}

enum WatchMsg {
    Upsert(PathBuf),
    Remove(PathBuf),
}

#[derive(Debug, Clone)]
pub struct IndexReport {
    pub path: PathBuf,
    pub status: String,
    /// Files present in the shared index after this call (approx / global).
    pub files_indexed: usize,
    pub expanded_parent: bool,
    pub absorbed_children: Vec<PathBuf>,
    /// True once the root is ready and under active watch (until drop).
    pub watching: bool,
}

#[derive(Debug, Clone)]
pub struct DropReport {
    pub path: PathBuf,
    pub dropped: bool,
    pub message: String,
}

#[derive(Debug, Clone)]
pub struct SearchOptions {
    pub query: String,
    /// Optional path filter (must be under an indexed root).
    pub path: Option<PathBuf>,
    pub max_results: usize,
}

#[derive(Debug, Clone)]
pub struct SearchHit {
    pub path: PathBuf,
    pub score: f32,
    pub line_number: Option<usize>,
    pub line_text: Option<String>,
    pub snippet: String,
}

#[derive(Debug, Clone)]
pub struct SearchReport {
    pub mode: &'static str,
    pub hits: Vec<SearchHit>,
    pub roots_used: Vec<PathBuf>,
    pub note: String,
}

impl TextSearchEngine {
    /// Create engine and kick off auto-discovery indexing under `cwd` (async).
    pub fn start(cwd: PathBuf) -> Self {
        let (event_tx, event_rx) = std::sync::mpsc::channel::<WatchMsg>();
        let index = SearchIndex::new().expect("tantivy ram index");
        let inner = Arc::new(Mutex::new(EngineState {
            index,
            roots: Vec::new(),
            pending: HashMap::new(),
            job_gen: 0,
            watchers: HashMap::new(),
            event_tx: Some(event_tx),
        }));
        let job_notify = Arc::new(Notify::new());
        let engine = Self {
            inner: inner.clone(),
            job_notify: job_notify.clone(),
        };

        // Watch event bridge: blocking recv on a worker, apply on runtime.
        let eng = engine.clone();
        std::thread::Builder::new()
            .name("myco-text-search-watch".into())
            .spawn(move || {
                // One runtime for the thread's lifetime; building one per
                // event turns a busy tree (a build writing target/) into a
                // runtime-construction storm.
                let Ok(rt) = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                else {
                    return;
                };
                while let Ok(msg) = event_rx.recv() {
                    let eng = eng.clone();
                    // Best-effort: block_on is fine on a dedicated thread.
                    rt.block_on(async {
                        eng.apply_watch_msg(msg).await;
                    });
                }
            })
            .ok();

        // Auto-index skills / AGENTS.md (and .claude/skills) under cwd.
        // Skip when no Tokio runtime (e.g. listing standard tool specs in unit tests).
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            let eng = engine.clone();
            handle.spawn(async move {
                // Tiny delay so host hello isn't contending with first walk.
                tokio::time::sleep(Duration::from_millis(10)).await;
                let targets = discover_auto_index_targets(&cwd);
                for t in targets {
                    let _ = eng.index_directory(t.path).await;
                }
            });
        }

        engine
    }

    /// Test helper: no auto-index, no watch thread required beyond normal.
    #[cfg(test)]
    pub fn start_for_tests() -> Self {
        let (event_tx, _event_rx) = std::sync::mpsc::channel::<WatchMsg>();
        let index = SearchIndex::new().expect("tantivy ram index");
        Self {
            inner: Arc::new(Mutex::new(EngineState {
                index,
                roots: Vec::new(),
                pending: HashMap::new(),
                job_gen: 0,
                watchers: HashMap::new(),
                event_tx: Some(event_tx),
            })),
            job_notify: Arc::new(Notify::new()),
        }
    }

    /// Register `path` as a **persistently indexed** root.
    ///
    /// - Installs a recursive filesystem watcher (symlinks never followed).
    /// - Performs the initial crawl and **awaits** until the root is searchable.
    /// - After return, the root stays live: create/modify/remove events update
    ///   the index incrementally until [`Self::drop_directory_index`].
    ///
    /// Indexing a parent of existing roots expands in place (children absorbed;
    /// already-indexed file content is kept). Paths already under a live root
    /// are no-ops.
    ///
    /// Prefer small/repeated scopes (skills, docs). For huge trees use bash+rg.
    pub async fn index_directory(&self, path: PathBuf) -> Result<IndexReport, String> {
        let resolved = resolve_path(&path)?;
        if !resolved.exists() {
            return Err(format!("path does not exist: {}", resolved.display()));
        }

        let (expanded_parent, absorbed, need_crawl) = {
            let mut state = self.inner.lock().await;

            // Already covered by a live root → no-op.
            if let Some(root) = state.roots.iter().find(|r| is_under(&r.path, &resolved)) {
                return Ok(IndexReport {
                    path: resolved,
                    status: format!(
                        "already covered by persistently indexed root {} (watching={})",
                        root.path.display(),
                        root.ready
                    ),
                    files_indexed: state.index.file_count(),
                    expanded_parent: false,
                    absorbed_children: vec![],
                    watching: root.ready,
                });
            }

            // Parent expand: absorb children under `resolved`.
            let mut absorbed = Vec::new();
            let children: Vec<PathBuf> = state
                .roots
                .iter()
                .filter(|r| is_under(&resolved, &r.path) && r.path != resolved)
                .map(|r| r.path.clone())
                .collect();

            for child in &children {
                let key = path_key(child);
                state.watchers.remove(&key);
                state.pending.remove(&key);
                absorbed.push(child.clone());
            }
            state
                .roots
                .retain(|r| !children.iter().any(|c| c == &r.path));

            let expanded_parent = !absorbed.is_empty();

            state.roots.push(IndexedRoot {
                path: resolved.clone(),
                ready: false,
                error: None,
            });
            state.job_gen += 1;
            let job_id = state.job_gen;
            state.pending.insert(path_key(&resolved), job_id);

            // Persistent watcher for this root (dir) or parent of a single file.
            // A single-file root watches its parent NON-recursively: for a
            // repo-root AGENTS.md the parent is the whole project tree, and a
            // recursive watch there costs one inotify watch per subdirectory
            // plus an event for every build artifact write.
            let (watch_target, watch_mode) = if resolved.is_dir() {
                (resolved.clone(), RecursiveMode::Recursive)
            } else {
                (
                    resolved
                        .parent()
                        .map(|p| p.to_path_buf())
                        .unwrap_or_else(|| resolved.clone()),
                    RecursiveMode::NonRecursive,
                )
            };
            if let Some(tx) = state.event_tx.clone() {
                match build_watcher(tx, resolved.clone()) {
                    Ok(mut w) => {
                        if let Err(e) = w.watch(&watch_target, watch_mode) {
                            // Still index; live updates may be missing.
                            if let Some(r) = state.roots.iter_mut().find(|r| r.path == resolved) {
                                r.error = Some(format!("watcher install failed: {e}"));
                            }
                        }
                        state.watchers.insert(path_key(&resolved), w);
                    }
                    Err(e) => {
                        if let Some(r) = state.roots.iter_mut().find(|r| r.path == resolved) {
                            r.error = Some(format!("watcher create failed: {e}"));
                        }
                    }
                }
            }

            (expanded_parent, absorbed, true)
        };

        let _ = need_crawl;
        // Initial crawl on this task (tool call blocks until searchable).
        self.crawl_root(resolved.clone()).await?;

        let files_now = {
            let st = self.inner.lock().await;
            st.index.file_count()
        };
        let watching = {
            let st = self.inner.lock().await;
            st.watchers.contains_key(&path_key(&resolved))
                && st.roots.iter().any(|r| r.path == resolved && r.ready)
        };

        Ok(IndexReport {
            path: resolved,
            status: if expanded_parent {
                "persistently indexed (parent expand; children absorbed; watching for changes)"
                    .into()
            } else {
                "persistently indexed and watching for changes".into()
            },
            files_indexed: files_now,
            expanded_parent,
            absorbed_children: absorbed,
            watching,
        })
    }

    /// Walk `root`, upsert files, commit, mark ready. Used by register + auto-index.
    async fn crawl_root(&self, root: PathBuf) -> Result<(), String> {
        let files = tokio::task::spawn_blocking({
            let root = root.clone();
            move || walk_text_files(&root)
        })
        .await
        .map_err(|e| format!("walk join: {e}"))?;

        for f in files {
            let text = tokio::task::spawn_blocking({
                let f = f.clone();
                move || read_text_file(&f)
            })
            .await
            .ok()
            .flatten();
            if let Some(text) = text {
                let mut st = self.inner.lock().await;
                st.index.upsert_file(&f, text);
            }
        }

        {
            let mut st = self.inner.lock().await;
            if let Err(e) = st.index.commit() {
                if let Some(r) = st.roots.iter_mut().find(|r| r.path == root) {
                    r.ready = false;
                    r.error = Some(e.clone());
                }
                st.pending.remove(&path_key(&root));
                self.job_notify.notify_waiters();
                return Err(e);
            }
            if let Some(r) = st.roots.iter_mut().find(|r| r.path == root) {
                r.ready = true;
                // Keep watcher error notes if any; clear crawl errors.
                if r.error
                    .as_ref()
                    .is_some_and(|e| e.starts_with("tantivy") || e.contains("commit"))
                {
                    r.error = None;
                }
            }
            st.pending.remove(&path_key(&root));
        }
        self.job_notify.notify_waiters();
        Ok(())
    }

    pub async fn drop_directory_index(&self, path: PathBuf) -> Result<DropReport, String> {
        let resolved = resolve_path(&path)?;
        let mut state = self.inner.lock().await;

        // Prefer exact root match; also allow dropping a covered path by finding root.
        let root_idx = state
            .roots
            .iter()
            .position(|r| r.path == resolved)
            .or_else(|| {
                state
                    .roots
                    .iter()
                    .position(|r| is_under(&r.path, &resolved) || is_under(&resolved, &r.path))
            });

        let Some(i) = root_idx else {
            return Ok(DropReport {
                path: resolved,
                dropped: false,
                message: "no indexed root matches path".into(),
            });
        };

        let root = state.roots.remove(i);
        let key = path_key(&root.path);
        state.watchers.remove(&key);
        state.pending.remove(&key);
        state.index.remove_tree(&root.path);
        self.job_notify.notify_waiters();

        Ok(DropReport {
            path: root.path,
            dropped: true,
            message: "index dropped".into(),
        })
    }

    pub async fn list_roots(&self) -> Vec<(PathBuf, bool, Option<String>)> {
        let st = self.inner.lock().await;
        st.roots
            .iter()
            .map(|r| (r.path.clone(), r.ready, r.error.clone()))
            .collect()
    }

    pub async fn search_exact(&self, opts: SearchOptions) -> Result<SearchReport, String> {
        self.search(opts, false).await
    }

    pub async fn search_semantic(&self, opts: SearchOptions) -> Result<SearchReport, String> {
        self.search(opts, true).await
    }

    async fn search(&self, opts: SearchOptions, semantic: bool) -> Result<SearchReport, String> {
        let query = opts.query.trim().to_string();
        if query.is_empty() {
            return Err("query must not be empty".into());
        }
        let limit = opts.max_results.clamp(1, 200);

        let filter = match &opts.path {
            Some(p) => Some(resolve_path(p)?),
            None => None,
        };

        // Wait until covering roots are ready (or fail if none).
        let roots_used = self.wait_for_coverage(filter.as_deref()).await?;

        let mut st = self.inner.lock().await;
        let hits_raw: Vec<Hit> = if semantic {
            st.index.search_semantic(&query, filter.as_deref(), limit)?
        } else {
            st.index.search_exact(&query, filter.as_deref(), limit)?
        };
        let hits = hits_raw
            .into_iter()
            .map(|h| SearchHit {
                path: h.path,
                score: h.score,
                line_number: h.line_number,
                line_text: h.line_text,
                snippet: h.snippet,
            })
            .collect();

        Ok(SearchReport {
            mode: if semantic {
                "semantic_candle"
            } else {
                "exact_tantivy"
            },
            hits,
            roots_used,
            note: if semantic {
                "semantic = Candle MiniLM (compile-time weights) cosine over indexed files. \
                 Prefer for skills / AGENTS.md intent; use exact for identifiers."
                    .into()
            } else {
                "exact = Tantivy full-text over indexed files. \
                 For large unindexed code trees, prefer bash + rg/grep instead of indexing."
                    .into()
            },
        })
    }

    /// Ensure `path` (or entire forest if None) has ready covering roots.
    async fn wait_for_coverage(&self, path: Option<&Path>) -> Result<Vec<PathBuf>, String> {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
        loop {
            {
                let st = self.inner.lock().await;
                if st.roots.is_empty() {
                    return Err(
                        "no directories indexed. Auto-index covers .claude/skills, SKILL.md \
                         folders, and AGENTS.md; call index_directory for more."
                            .into(),
                    );
                }
                if let Some(p) = path {
                    let covering: Vec<_> =
                        st.roots.iter().filter(|r| is_under(&r.path, p)).collect();
                    if covering.is_empty() {
                        return Err(format!(
                            "path {} is not under any indexed root. Indexed roots: {}. \
                             Call index_directory first (or rely on skills auto-index).",
                            p.display(),
                            st.roots
                                .iter()
                                .map(|r| r.path.display().to_string())
                                .collect::<Vec<_>>()
                                .join(", ")
                        ));
                    }
                    let pending = covering.iter().any(|r| !r.ready);
                    if !pending {
                        return Ok(covering.iter().map(|r| r.path.clone()).collect());
                    }
                } else {
                    // No path filter: wait for all pending jobs that exist; allow search
                    // across ready roots even if some pending — but if ALL pending, wait.
                    let any_ready = st.roots.iter().any(|r| r.ready);
                    let any_pending = !st.pending.is_empty();
                    if any_ready && !any_pending {
                        return Ok(st.roots.iter().map(|r| r.path.clone()).collect());
                    }
                    if any_ready && any_pending {
                        // Search partial index.
                        return Ok(st
                            .roots
                            .iter()
                            .filter(|r| r.ready)
                            .map(|r| r.path.clone())
                            .collect());
                    }
                    // all pending
                }
            }
            if tokio::time::Instant::now() > deadline {
                return Err("timed out waiting for indexing to complete".into());
            }
            let notified = self.job_notify.notified();
            tokio::pin!(notified);
            tokio::select! {
                _ = &mut notified => {}
                _ = tokio::time::sleep(Duration::from_millis(50)) => {}
            }
        }
    }

    async fn apply_watch_msg(&self, msg: WatchMsg) {
        match msg {
            WatchMsg::Upsert(path) => {
                // Only if under some persistently indexed root.
                let under = {
                    let st = self.inner.lock().await;
                    st.roots.iter().any(|r| is_under(&r.path, &path))
                };
                if !under {
                    return;
                }
                let text = tokio::task::spawn_blocking({
                    let path = path.clone();
                    move || read_text_file(&path)
                })
                .await
                .ok()
                .flatten();
                let mut st = self.inner.lock().await;
                match text {
                    Some(text) => {
                        st.index.upsert_file(&path, text);
                    }
                    // Unreadable now: renames/moves arrive as Modify events
                    // carrying the *old* path (inotify `Name(From)`), never as
                    // Remove — and a file can also grow past the size cap.
                    // Either way, keeping the old entry would leave stale
                    // content searchable forever; drop it.
                    None => {
                        st.index.remove_file(&path);
                    }
                }
                let _ = st.index.commit();
            }
            WatchMsg::Remove(path) => {
                let mut st = self.inner.lock().await;
                st.index.remove_file(&path);
                let _ = st.index.commit();
            }
        }
    }
}

fn path_key(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn build_watcher(
    tx: std::sync::mpsc::Sender<WatchMsg>,
    _root: PathBuf,
) -> notify::Result<RecommendedWatcher> {
    // Debounce lightly by coalescing isn't done here; apply is cheap.
    RecommendedWatcher::new(
        move |res: Result<Event, notify::Error>| {
            let Ok(event) = res else { return };
            match event.kind {
                EventKind::Remove(_) => {
                    for p in event.paths {
                        let _ = tx.send(WatchMsg::Remove(p));
                    }
                }
                EventKind::Create(_) | EventKind::Modify(_) | EventKind::Any => {
                    for p in event.paths {
                        // Skip dirs; upsert only files.
                        if p.is_file() {
                            let _ = tx.send(WatchMsg::Upsert(p));
                        } else if p.exists() && p.is_dir() {
                            // ignore pure dir create; files will event separately
                        } else {
                            // might be remove+recreate race
                            let _ = tx.send(WatchMsg::Upsert(p));
                        }
                    }
                }
                _ => {}
            }
        },
        notify::Config::default(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tmp() -> PathBuf {
        let p = std::env::temp_dir().join(format!("myco-ts-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&p).unwrap();
        p
    }

    #[tokio::test]
    async fn index_and_exact_search() {
        let dir = tmp();
        fs::create_dir_all(dir.join("skills/pdf")).unwrap();
        fs::write(
            dir.join("skills/pdf/SKILL.md"),
            "---\nname: pdf\ndescription: Extract PDF forms\n---\nUse for PDF extraction.\n",
        )
        .unwrap();

        let eng = TextSearchEngine::start_for_tests();
        let report = eng.index_directory(dir.join("skills")).await.unwrap();
        assert!(
            report.watching || report.status.contains("persistently indexed"),
            "{report:?}"
        );

        // Wait for ready
        for _ in 0..100 {
            let roots = eng.list_roots().await;
            if roots.iter().any(|(_, ready, _)| *ready) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        let res = eng
            .search_exact(SearchOptions {
                query: "PDF forms".into(),
                path: Some(dir.join("skills")),
                max_results: 10,
            })
            .await
            .unwrap();
        assert!(!res.hits.is_empty(), "{res:?}");
        assert!(res.hits[0].path.ends_with("SKILL.md"));
    }

    #[tokio::test]
    async fn parent_expand_absorbs_child() {
        let dir = tmp();
        fs::create_dir_all(dir.join("a/b")).unwrap();
        fs::write(dir.join("a/b/x.md"), "hello child unique_token_xyz\n").unwrap();
        fs::write(dir.join("a/y.md"), "hello parent unique_token_xyz\n").unwrap();

        let eng = TextSearchEngine::start_for_tests();
        eng.index_directory(dir.join("a/b")).await.unwrap();
        for _ in 0..100 {
            if eng.list_roots().await.iter().any(|(_, r, _)| *r) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        let rep = eng.index_directory(dir.join("a")).await.unwrap();
        assert!(rep.expanded_parent, "{rep:?}");
        assert_eq!(rep.absorbed_children.len(), 1);

        for _ in 0..100 {
            let roots = eng.list_roots().await;
            if roots.len() == 1 && roots[0].1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let roots = eng.list_roots().await;
        assert_eq!(roots.len(), 1, "{roots:?}");

        let res = eng
            .search_exact(SearchOptions {
                query: "unique_token_xyz".into(),
                path: None,
                max_results: 10,
            })
            .await
            .unwrap();
        assert!(res.hits.len() >= 2, "{res:?}");
    }

    #[tokio::test]
    async fn search_requires_index() {
        let eng = TextSearchEngine::start_for_tests();
        let err = eng
            .search_exact(SearchOptions {
                query: "x".into(),
                path: None,
                max_results: 5,
            })
            .await
            .unwrap_err();
        assert!(err.contains("no directories indexed"), "{err}");
    }
}
