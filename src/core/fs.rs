//! Where myco keeps its files, and how it replaces one.
//!
//! Both primitives are wanted by nearly every layer: prompts resolve the manual
//! directory under [`myco_home`], the manual exporter and the session store both
//! publish files with [`atomically_write`], and startup preflight needs the home
//! to export into.

use std::io::Write;
use std::path::{Path, PathBuf};

/// The selected profile's data root: `$MYCO_HOME/profiles/$MYCO_PROFILE`.
/// Defaults are `~/.myco` and `default`; all durable application data lives here.
pub fn myco_home() -> Result<PathBuf, String> {
    myco_home_with(|key| std::env::var(key).ok())
}

pub fn validate_profile(name: &str) -> Result<String, String> {
    if name.is_empty()
        || !name
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
    {
        return Err(
            "profile must contain only letters, digits, '-' or '_' and must not be empty".into(),
        );
    }
    Ok(name.to_string())
}

pub(crate) fn myco_home_with(env: impl Fn(&str) -> Option<String>) -> Result<PathBuf, String> {
    let profile = validate_profile(&env("MYCO_PROFILE").unwrap_or_else(|| "default".into()))?;
    let root = match env("MYCO_HOME").filter(|s| !s.is_empty()) {
        Some(root) => PathBuf::from(root),
        None => dirs::home_dir()
            .map(|h| h.join(".myco"))
            .ok_or_else(|| "could not resolve home directory".to_string())?,
    };
    Ok(root.join("profiles").join(profile))
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

    #[test]
    fn profile_roots_are_isolated_and_default_is_named() {
        for profile in [None, Some("default"), Some("testing")] {
            let path = myco_home_with(|key| match key {
                "MYCO_HOME" => Some("/tmp/myco-profiles".into()),
                "MYCO_PROFILE" => profile.map(str::to_string),
                _ => None,
            })
            .unwrap();
            assert_eq!(
                path,
                Path::new("/tmp/myco-profiles/profiles").join(profile.unwrap_or("default"))
            );
        }
        for name in [
            "",
            "..",
            "../other",
            "/tmp/escape",
            "a/b",
            "a\\b",
            "two words",
        ] {
            assert!(validate_profile(name).is_err(), "{name}");
        }
    }

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
