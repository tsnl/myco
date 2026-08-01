//! Compile-time Myco manual: markdown articles embedded via [`include_str!`].
//!
//! Startup copies the articles to `<myco home>/manual/<version>/<commit>/`
//! ([`export`]) and the agent system prompt points there, so agents read and
//! search the manual with the tools they already have (`rg`, the editor)
//! instead of a dedicated tool. `myco --help [article]` prints the same text.
//! Always-on agent policy (worktrees, computer-use, coding norms, user authority) lives in
//! [`myco_prompts`] instead.

use std::path::{Path, PathBuf};

/// One embedded manual page.
#[derive(Debug, Clone, Copy)]
pub struct Article {
    /// Stable id (`overview`, `harness-ops`, …).
    pub id: &'static str,
    /// Short human title for catalog listings.
    pub title: &'static str,
    /// One-line summary shown by catalog / tool `list`.
    pub summary: &'static str,
    /// Full markdown body (`include_str!`).
    pub body: &'static str,
}

/// Catalog of manual articles. Order is display order for listings.
pub const ARTICLES: &[Article] = &[
    Article {
        id: "overview",
        title: "Myco overview",
        summary: "Architecture, config paths, host routing, V1 product limits",
        body: include_str!("articles/overview.md"),
    },
    Article {
        id: "api",
        title: "Driving myco",
        summary: "The HTTP API, GUI, turn/compaction lifecycle, nested agents",
        body: include_str!("articles/api.md"),
    },
    Article {
        id: "harness-ops",
        title: "Harness ops",
        summary: "Find hosts, install from releases or git source, diagnosis checklist",
        body: include_str!("articles/harness-ops.md"),
    },
];

/// Build identity of the exported copy: package version and the git commit
/// stamped by `build.rs` (`unknown` when built from a tree without history).
/// Both are path components, so a version bump or a rebuild from a new commit
/// lands in a fresh directory rather than editing one an older myco may still
/// be pointing its agents at.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
pub const GIT_COMMIT: &str = env!("MYCO_GIT_COMMIT");

/// Directory the articles are exported to under `home` (`~/.myco`).
pub fn dir(home: &Path) -> PathBuf {
    home.join("manual").join(VERSION).join(GIT_COMMIT)
}

/// Write the articles (plus `index.md`) to [`dir`], returning that directory.
///
/// Files are written only when their bytes differ from what is on disk, so a
/// second startup on the same build does nothing, while a dev build that
/// edited an article without committing still refreshes it — the commit alone
/// cannot tell those two apart. Writes are atomic: several myco processes
/// (a supervisor and its nested agents) can start at once without any of them
/// reading a half-written article.
pub fn export(home: &Path) -> Result<PathBuf, String> {
    let dir = dir(home);
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    write_if_changed(&dir.join("index.md"), &format_index())?;
    for a in ARTICLES {
        write_if_changed(&dir.join(format!("{}.md", a.id)), a.body)?;
    }
    Ok(dir)
}

fn write_if_changed(path: &Path, content: &str) -> Result<(), String> {
    if std::fs::read_to_string(path).is_ok_and(|on_disk| on_disk == content) {
        return Ok(());
    }
    myco_core::atomically_write(path, content.as_bytes())
        .map_err(|e| format!("{}: {e}", path.display()))
}

/// `index.md`: what each article file covers, for an agent that lands in the
/// directory before it knows which one to read.
fn format_index() -> String {
    let mut out = format!("# Myco manual ({VERSION}, commit {GIT_COMMIT})\n\n");
    out.push_str(
        "Runtime docs for this myco build, exported at startup. Read or search these files \
         directly (`rg`, the editor); `myco --help <id>` prints the same text.\n\n",
    );
    for a in ARTICLES {
        out.push_str(&format!("- `{}.md` — {} — {}\n", a.id, a.title, a.summary));
    }
    out
}

/// Look up an article by id (exact match).
pub fn article(id: &str) -> Option<&'static Article> {
    ARTICLES.iter().find(|a| a.id == id)
}

/// Known article ids, comma-separated (for error messages).
pub fn known_ids() -> String {
    ARTICLES.iter().map(|a| a.id).collect::<Vec<_>>().join(", ")
}

/// Catalog text for the CLI help footer.
pub fn format_catalog() -> String {
    let mut out = String::from("Myco manual — `myco --help <id>`:\n\n");
    for a in ARTICLES {
        out.push_str(&format!("- `{}` — {}\n  {}\n", a.id, a.title, a.summary));
    }
    out
}

/// Full article body with a title header (CLI / tool `get`).
pub fn format_article(id: &str) -> Result<String, String> {
    match article(id) {
        Some(a) => Ok(format!("# {} (`{}`)\n\n{}", a.title, a.id, a.body)),
        None => Err(format!(
            "unknown manual article {id:?}; known: {}",
            known_ids()
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn articles_nonempty_unique_ids() {
        assert!(!ARTICLES.is_empty());
        let mut ids: Vec<&str> = ARTICLES.iter().map(|a| a.id).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), ARTICLES.len(), "duplicate article ids");
        assert_eq!(ids, ["api", "harness-ops", "overview"]); // sorted unique
        for a in ARTICLES {
            assert!(!a.body.trim().is_empty(), "empty body: {}", a.id);
            assert!(!a.title.is_empty());
            assert!(!a.summary.is_empty());
        }
        // Policy sections live in the always-on system prompt, not the manual catalog.
        for id in ["worktrees", "computer-use", "coding-norms"] {
            assert!(article(id).is_none(), "{id} should not be a manual article");
        }
    }

    /// The export is the agent-visible manual: every article lands as its own
    /// file under the build-keyed directory, next to an index naming them.
    #[test]
    fn export_writes_one_file_per_article_plus_index() {
        let home = tempdir();
        let dir = export(&home).unwrap();
        assert!(
            dir.ends_with(format!("manual/{VERSION}/{GIT_COMMIT}")),
            "{dir:?}"
        );

        let index = std::fs::read_to_string(dir.join("index.md")).unwrap();
        for a in ARTICLES {
            let body = std::fs::read_to_string(dir.join(format!("{}.md", a.id))).unwrap();
            assert_eq!(body, a.body, "{} body differs from the embedded copy", a.id);
            assert!(index.contains(&format!("`{}.md`", a.id)), "{index}");
            assert!(index.contains(a.summary), "{index}");
        }
    }

    /// A build that edits an article without committing keeps the same commit,
    /// so existence of the directory cannot stand in for freshness.
    #[test]
    fn export_refreshes_a_stale_article_in_an_existing_dir() {
        let home = tempdir();
        let dir = export(&home).unwrap();
        let article = dir.join("overview.md");
        std::fs::write(&article, "stale").unwrap();
        std::fs::write(dir.join("unrelated.md"), "kept").unwrap();

        export(&home).unwrap();
        assert_eq!(std::fs::read_to_string(&article).unwrap(), ARTICLES[0].body);
        // Only the exported files are managed; anything else in the directory
        // is left alone.
        assert_eq!(
            std::fs::read_to_string(dir.join("unrelated.md")).unwrap(),
            "kept"
        );
    }

    /// Same build, second startup: nothing is rewritten, so a myco already
    /// running off these files never sees them change under it.
    #[test]
    fn export_is_a_no_op_when_the_files_already_match() {
        let home = tempdir();
        let dir = export(&home).unwrap();
        let article = dir.join("api.md");
        let before = std::fs::metadata(&article).unwrap().modified().unwrap();

        export(&home).unwrap();
        assert_eq!(
            std::fs::metadata(&article).unwrap().modified().unwrap(),
            before
        );
    }

    fn tempdir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("myco-manual-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn catalog_and_format_article() {
        let cat = format_catalog();
        for a in ARTICLES {
            assert!(cat.contains(a.id), "catalog missing {}", a.id);
            let body = format_article(a.id).unwrap();
            assert!(body.contains(a.title));
        }
        assert!(format_article("nope").is_err());
    }
}
