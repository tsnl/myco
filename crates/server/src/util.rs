//! Small filesystem plumbing the server owns: where durable state lives,
//! and how it is written without ever being half-written.

use std::path::{Path, PathBuf};

/// `$MYCO_HOME` → `~/.myco`. Every durable file the server writes lives
/// directly here, shared with v1's home rather than a versioned subfolder:
/// v3's files (`server.toml`, `auth.json`, `passkeys.json`,
/// `operator.token`) collide with nothing v1 ever wrote (`config.toml`,
/// its own data), and v2 keeps to `~/.myco/v2/`, so all three generations
/// coexist in one home.
pub fn data_root() -> Result<PathBuf, String> {
    match std::env::var_os("MYCO_HOME") {
        Some(p) => Ok(PathBuf::from(p)),
        None => std::env::var_os("HOME")
            .map(|h| PathBuf::from(h).join(".myco"))
            .ok_or_else(|| "neither $MYCO_HOME nor $HOME is set".to_string()),
    }
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
