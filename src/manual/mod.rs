//! Compile-time Myco manual: markdown articles embedded via [`include_str!`].
//!
//! Used by the `manual` host tool and by `myco --help [article]`.
//! Always-on agent policy (worktrees, computer-use, coding norms) lives in
//! [`crate::prompts`] instead.

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
        id: "cli",
        title: "User-facing CLI",
        summary: "Slash-commands and keybindings the agent cannot press",
        body: include_str!("articles/cli.md"),
    },
    Article {
        id: "harness-ops",
        title: "Harness ops",
        summary: "Find hosts, install from releases or git source, diagnosis checklist",
        body: include_str!("articles/harness-ops.md"),
    },
];

/// Look up an article by id (exact match).
pub fn article(id: &str) -> Option<&'static Article> {
    ARTICLES.iter().find(|a| a.id == id)
}

/// Known article ids, comma-separated (for error messages).
pub fn known_ids() -> String {
    ARTICLES.iter().map(|a| a.id).collect::<Vec<_>>().join(", ")
}

/// Catalog text for the `manual` tool `list` action and CLI help footer.
pub fn format_catalog() -> String {
    let mut out = String::from("Myco manual — `myco --help <id>` or tool `manual` get:\n\n");
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
        assert_eq!(ids, ["cli", "harness-ops", "overview"]); // sorted unique
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
