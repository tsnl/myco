//! The agent memory: maildir-style entries under `~/.myco/workspace/memory/`.
//!
//! One write-once `*.md` file per entry. Every visible entry is rendered into
//! each agent system prompt ([`crate::prompts`]); the root-only `memory` tool
//! ([`crate::tool_services::MemoryTool`]) is the edit path. The storage rules
//! that keep concurrent agents safe on a weakly consistent filesystem live
//! here: entries are written under a hidden temp name and renamed into place,
//! never edited in place. Names are unique, so concurrent adds cannot
//! collide, and two agents replacing the same entry leave two candidate
//! entries (a duplicate to merge later) — never a lost one.

use std::path::{Path, PathBuf};

/// One memory entry: filename (its id) and trimmed text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryEntry {
    pub name: String,
    pub text: String,
}

/// The memory directory, respecting `MYCO_HOME`.
pub fn dir() -> Result<PathBuf, String> {
    Ok(crate::core::myco_home()?.join("workspace").join("memory"))
}

/// Whether `name` is a legal entry id: a plain visible `*.md` filename.
/// Hidden names are in-progress writes; path separators would escape the dir.
pub fn is_entry_name(name: &str) -> bool {
    name.len() > ".md".len()
        && !name.starts_with('.')
        && name.ends_with(".md")
        && !name.contains(['/', '\\'])
}

/// All visible entries in filename order — the render order. Hidden names,
/// non-`*.md` files, and whitespace-only entries are skipped.
pub fn entries(dir: &Path) -> Vec<MemoryEntry> {
    let Ok(read) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<MemoryEntry> = read
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_str()?.to_string();
            if !is_entry_name(&name) || !entry.path().is_file() {
                return None;
            }
            let text = std::fs::read_to_string(entry.path())
                .ok()?
                .trim()
                .to_string();
            (!text.is_empty()).then_some(MemoryEntry { name, text })
        })
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// The memory as it reads in a prompt (and in `memory` action=list): each entry
/// under a `[memory entry <name>]` label, so agents can `replace`/`remove` by
/// the id they see. Empty string when there are no entries.
pub fn rendered_body(entries: &[MemoryEntry]) -> String {
    let mut out = String::new();
    for entry in entries {
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        out.push_str(&format!("[memory entry {}]\n{}", entry.name, entry.text));
    }
    out
}

/// Create a new entry holding `text`; returns its filename. The name is a
/// UTC timestamp plus random hex, so concurrent adds land side by side, and
/// the write goes through a hidden temp name so a concurrently rendering
/// process never sees a partial entry.
pub fn add_entry(dir: &Path, text: &str) -> Result<String, String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    let body = format!("{}\n", text.trim_end());
    // 4 random hex per attempt: a same-second collision is one in 65536, and
    // an existing name is skipped, so the retry cap is unreachable in practice.
    for _ in 0..8 {
        let name = format!(
            "{}-{}.md",
            chrono::Utc::now().format("%Y%m%dT%H%M%SZ"),
            &crate::core::uuid_simple_hex(uuid::Uuid::new_v4())[..4]
        );
        let path = dir.join(&name);
        if path.exists() {
            continue;
        }
        let tmp = dir.join(format!(".tmp-{name}"));
        std::fs::write(&tmp, &body).map_err(|e| format!("write {}: {e}", tmp.display()))?;
        return match std::fs::rename(&tmp, &path) {
            Ok(()) => Ok(name),
            Err(e) => {
                let _ = std::fs::remove_file(&tmp);
                Err(format!("rename {} into place: {e}", tmp.display()))
            }
        };
    }
    Err("could not pick a fresh memory entry name".into())
}

/// Remove entry `name`. `Ok(false)` when it is already gone — a concurrent
/// agent superseding the same entry is normal, not an IO failure.
pub fn remove_entry(dir: &Path, name: &str) -> Result<bool, String> {
    if !is_entry_name(name) {
        return Err(format!(
            "{name:?} is not a memory entry id (a plain `*.md` filename, as listed)"
        ));
    }
    match std::fs::remove_file(dir.join(name)) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(format!("remove {name}: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("myco-memory-store-{tag}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn entries_render_in_filename_order_with_labels() {
        let dir = temp_dir("order");
        std::fs::write(dir.join("20260102T000000Z-bbbb.md"), "second\n").unwrap();
        std::fs::write(dir.join("20260101T000000Z-aaaa.md"), "first\n").unwrap();
        std::fs::write(dir.join(".tmp-20260103T000000Z-cccc.md"), "hidden\n").unwrap();
        std::fs::write(dir.join("notes.txt"), "not markdown\n").unwrap();
        std::fs::write(dir.join("20260104T000000Z-dddd.md"), "  \n\n").unwrap();

        let entries = entries(&dir);
        assert_eq!(
            entries.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(),
            vec!["20260101T000000Z-aaaa.md", "20260102T000000Z-bbbb.md"]
        );
        assert_eq!(
            rendered_body(&entries),
            "[memory entry 20260101T000000Z-aaaa.md]\nfirst\n\n\
             [memory entry 20260102T000000Z-bbbb.md]\nsecond"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn add_creates_a_visible_entry_and_leaves_no_temp_behind() {
        let dir = temp_dir("add");
        let name = add_entry(&dir, "  a fact  \n\n").unwrap();
        assert!(is_entry_name(&name), "{name}");
        assert_eq!(
            std::fs::read_to_string(dir.join(&name)).unwrap(),
            "  a fact\n"
        );
        // No stray temp files: hidden names would still pollute the dir.
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with('.'))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn remove_distinguishes_gone_from_failure() {
        let dir = temp_dir("remove");
        let name = add_entry(&dir, "ephemeral").unwrap();
        assert_eq!(remove_entry(&dir, &name), Ok(true));
        // Already gone reads as a concurrent supersede, not an error.
        assert_eq!(remove_entry(&dir, &name), Ok(false));
        // Ids never traverse paths or touch hidden names.
        assert!(remove_entry(&dir, "../escape.md").is_err());
        assert!(remove_entry(&dir, ".tmp-x.md").is_err());
        assert!(remove_entry(&dir, "plain.txt").is_err());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
