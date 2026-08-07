//! Small filesystem plumbing the server owns: where durable state lives,
//! and how it is written without ever being half-written.

use std::path::{Path, PathBuf};

/// `$MYCO_HOME` → `~/.myco`, then the `v3` generation directory. Every
/// durable file the server writes lives under here.
pub fn data_root() -> Result<PathBuf, String> {
    let home = match std::env::var_os("MYCO_HOME") {
        Some(p) => PathBuf::from(p),
        None => std::env::var_os("HOME")
            .map(|h| PathBuf::from(h).join(".myco"))
            .ok_or("neither $MYCO_HOME nor $HOME is set")?,
    };
    Ok(home.join("v3"))
}

/// Write via a sibling temp file and rename, so a crash mid-write leaves
/// the old file, never a torn one.
pub fn atomically_write(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("{} has no parent", path.display()))?;
    let temp = parent.join(format!(
        ".{}.tmp",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("write")
    ));
    std::fs::write(&temp, bytes).map_err(|e| format!("write {}: {e}", temp.display()))?;
    std::fs::rename(&temp, path).map_err(|e| format!("rename to {}: {e}", path.display()))
}
