//! Where myco keeps its files, and how it replaces one.
//!
//! Both primitives are wanted by nearly every layer: prompts resolve the manual
//! directory under [`myco_home`], the manual exporter and the session store both
//! publish files with [`atomically_write`], and startup preflight needs the home
//! to export into.

use std::io::Write;
use std::path::{Path, PathBuf};

/// myco's data root: `$MYCO_HOME` when set to a non-empty value, else
/// `~/.myco`. Sessions, the soul, the exported manual and the config all hang
/// off this.
///
/// `$MYCO_HOME` is what tests point at a scratch directory, so nothing that
/// resolves a myco path may hardcode `~/.myco` instead of calling this.
pub fn myco_home() -> Result<PathBuf, String> {
    if let Ok(root) = std::env::var("MYCO_HOME") {
        let p = PathBuf::from(root);
        if !p.as_os_str().is_empty() {
            return Ok(p);
        }
    }
    dirs::home_dir()
        .map(|h| h.join(".myco"))
        .ok_or_else(|| "could not resolve home directory".into())
}

/// Publish `content` at `path` in one step: write a temporary sibling, fsync it,
/// rename over `path`, then fsync the directory.
///
/// A reader therefore sees either the old bytes or the new ones, never a
/// half-written file — which matters because myco rewrites whole documents
/// (a session's `{id}.json`, a manual article) on every change, and a reader
/// can arrive at any moment.
///
/// `path` is replaced, not followed: a symlink at `path` becomes a regular
/// file. Callers that must write *through* a symlink resolve it first.
pub fn atomically_write(path: &Path, content: &[u8]) -> Result<(), String> {
    let mut file = atomic_write_file::AtomicWriteFile::options()
        .open(path)
        .map_err(|e| e.to_string())?;
    file.write_all(content).map_err(|e| e.to_string())?;
    file.commit().map_err(|e| e.to_string())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Replacing an existing file leaves no window where a reader sees neither
    /// version, and no leftover temporary sibling in the directory.
    #[test]
    fn replacing_a_file_leaves_only_the_new_content() {
        let tmp = crate::test_support::temp_dir("core-fs-atomic");
        let path = tmp.path().join("doc.json");
        atomically_write(&path, b"first").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"first");

        atomically_write(&path, b"second").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"second");

        let entries: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(entries, ["doc.json"], "temporary sibling was left behind");
    }

    /// The documented symlink behaviour: the link is replaced, and the file it
    /// pointed at keeps its old content. Callers that need the opposite
    /// canonicalize before calling (see `text_editor_service`).
    #[test]
    fn a_symlink_is_replaced_not_followed() {
        let tmp = crate::test_support::temp_dir("core-fs-symlink");
        let target = tmp.path().join("target");
        let link = tmp.path().join("link");
        std::fs::write(&target, b"target content").unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();

        atomically_write(&link, b"through the link").unwrap();

        assert_eq!(std::fs::read(&target).unwrap(), b"target content");
        assert_eq!(std::fs::read(&link).unwrap(), b"through the link");
        assert!(!std::fs::symlink_metadata(&link).unwrap().is_symlink());
    }
}
