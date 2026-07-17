//! Auto-discovery of skill packs and project guidance files.
//!
//! Targets ([agentskills.io](https://agentskills.io/home)):
//! - `.claude/skills` directories
//! - directories that contain a `SKILL.md` (Agent Skills layout)
//! - `AGENTS.md` files (and common aliases)

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// A path the engine should index eagerly at host start.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct AutoIndexTarget {
    pub path: PathBuf,
    /// Human-readable reason (for tool / log output).
    pub reason: &'static str,
}

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
    // Don't recurse into worktree checkouts as separate discovery forests
    // beyond normal walk limits.
];

const AGENTS_NAMES: &[&str] = &["AGENTS.md", "CLAUDE.md"];

/// Max directory depth when scanning from `start` for skills / AGENTS.md.
const MAX_DISCOVERY_DEPTH: u32 = 6;
/// Hard cap on directories visited during discovery (safety for huge trees).
const MAX_DIRS_VISITED: usize = 4_000;

/// Discover auto-index roots under `start` (typically process cwd).
///
/// Always includes `start/.claude/skills` when that directory exists.
/// Also walks a bounded tree for `SKILL.md` parents and `AGENTS.md` files.
pub fn discover_auto_index_targets(start: &Path) -> Vec<AutoIndexTarget> {
    let mut set: BTreeSet<AutoIndexTarget> = BTreeSet::new();

    let claude_skills = start.join(".claude").join("skills");
    if claude_skills.is_dir() {
        set.insert(AutoIndexTarget {
            path: claude_skills,
            reason: ".claude/skills",
        });
    }

    // Optional myco-local skills pack next to the project.
    let myco_skills = start.join(".myco").join("skills");
    if myco_skills.is_dir() {
        set.insert(AutoIndexTarget {
            path: myco_skills,
            reason: ".myco/skills",
        });
    }

    let mut dirs_visited = 0usize;
    walk_discover(start, 0, &mut set, &mut dirs_visited);

    set.into_iter().collect()
}

fn walk_discover(
    dir: &Path,
    depth: u32,
    out: &mut BTreeSet<AutoIndexTarget>,
    dirs_visited: &mut usize,
) {
    if depth > MAX_DISCOVERY_DEPTH || *dirs_visited >= MAX_DIRS_VISITED {
        return;
    }
    *dirs_visited += 1;

    let read = match std::fs::read_dir(dir) {
        Ok(r) => r,
        Err(_) => return,
    };

    let mut subdirs = Vec::new();
    for entry in read.flatten() {
        let path = entry.path();
        let Ok(ft) = entry.file_type() else { continue };
        // Never follow symlinks.
        if ft.is_symlink() {
            continue;
        }
        if ft.is_file() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.eq_ignore_ascii_case("SKILL.md") {
                out.insert(AutoIndexTarget {
                    path: dir.to_path_buf(),
                    reason: "SKILL.md directory",
                });
            } else if AGENTS_NAMES.iter().any(|n| name == *n) {
                out.insert(AutoIndexTarget {
                    path: path,
                    reason: "AGENTS.md / CLAUDE.md",
                });
            }
        } else if ft.is_dir() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if SKIP_DIR_NAMES.iter().any(|s| name == *s) {
                continue;
            }
            // `.claude` itself is entered so we can find nested skills, but
            // `.myco/worktrees` is huge — skip worktrees checkout dirs.
            if name == "worktrees" && dir.ends_with(".myco") {
                continue;
            }
            subdirs.push(path);
        }
    }

    for sub in subdirs {
        walk_discover(&sub, depth + 1, out, dirs_visited);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn discovers_claude_skills_skill_md_and_agents() {
        let dir = tempfile_dir();
        fs::create_dir_all(dir.join(".claude/skills/foo")).unwrap();
        fs::write(
            dir.join(".claude/skills/foo/SKILL.md"),
            "---\nname: foo\ndescription: Foo skill\n---\n# Foo\n",
        )
        .unwrap();
        fs::create_dir_all(dir.join("nested/bar")).unwrap();
        fs::write(
            dir.join("nested/bar/SKILL.md"),
            "---\nname: bar\ndescription: Bar\n---\n",
        )
        .unwrap();
        fs::write(dir.join("AGENTS.md"), "# agents\n").unwrap();
        fs::write(dir.join("nested/CLAUDE.md"), "# claude\n").unwrap();

        let targets = discover_auto_index_targets(&dir);
        let paths: Vec<_> = targets.iter().map(|t| t.path.clone()).collect();
        assert!(paths.iter().any(|p| p.ends_with(".claude/skills")));
        assert!(paths.iter().any(|p| p.ends_with("nested/bar")));
        assert!(paths.iter().any(|p| p.ends_with("AGENTS.md")));
        assert!(paths.iter().any(|p| p.ends_with("CLAUDE.md")));
    }

    fn tempfile_dir() -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("myco-discover-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&p).unwrap();
        p
    }
}
