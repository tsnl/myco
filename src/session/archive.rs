//! Archive relocation keeps the writer-lock inode fixed and moves the JSON last.
//! Interrupted moves remain readable: sidecars resolve in either location, and
//! repeating archive/restore moves the remaining files without overwriting any.

use std::fs;
use std::path::PathBuf;

use super::{
    Session, SessionLockError, SessionWriteLock, iter_sharded_session_files, session_file_path,
    session_root,
};

/// Move legacy archives at startup, leaving live writers and unreadable files alone.
pub fn migrate_archived_sessions() -> Result<(), String> {
    let root = session_root()?;
    if !root.exists() {
        return Ok(());
    }
    for path in iter_sharded_session_files(&root)? {
        // Active histories can be large; only archived documents need a full load.
        #[derive(serde::Deserialize)]
        struct Status {
            #[serde(default)]
            archived: bool,
        }
        let Some(id) = path.file_stem().and_then(|id| id.to_str()) else {
            continue;
        };
        let Ok(bytes) = fs::read(&path) else { continue };
        let Ok(Status { archived: true }) = serde_json::from_slice(&bytes) else {
            continue;
        };
        if let Err(error) = migrate_one(id) {
            eprintln!(
                "warning: could not move archived session {}: {error}",
                path.display()
            );
        }
    }
    Ok(())
}

fn migrate_one(id: &str) -> Result<(), String> {
    let _lock = match SessionWriteLock::acquire(id) {
        Ok(lock) => lock,
        Err(SessionLockError::Busy { .. }) => return Ok(()),
        Err(error) => return Err(error.to_string()),
    };
    // The previous writer may have restored the session before releasing its lock.
    if Session::load_by_id_or_prefix(id)?.archived {
        Relocation::prepare(id, true)?.apply()?;
    }
    Ok(())
}

pub(super) fn file_path(id: &str, ext: &str, archived: bool) -> PathBuf {
    let mut root = session_root().unwrap_or_else(|_| PathBuf::from(".myco/session"));
    if archived {
        root.push("archived");
    }
    let shard: String = id.chars().take(2).collect();
    root.join(shard).join(format!("{id}.{ext}"))
}

pub(super) struct Relocation(Vec<(PathBuf, PathBuf)>);

impl Relocation {
    pub(super) fn prepare(id: &str, archived: bool) -> Result<Self, String> {
        let current = session_file_path(id, "json");
        let source = file_path(id, "json", !archived);
        let target = file_path(id, "json", archived);
        let parent = target.parent().unwrap();
        fs::create_dir_all(parent).map_err(|e| format!("create {}: {e}", parent.display()))?;
        let mut files = Vec::new();
        let directory = source.parent().unwrap();
        if directory.exists() {
            for entry in fs::read_dir(directory).map_err(|e| e.to_string())? {
                let entry = entry.map_err(|e| e.to_string())?;
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if name.starts_with(&format!("{id}."))
                    && name != format!("{id}.lock")
                    && entry.path() != source
                {
                    files.push((entry.path(), parent.join(name.as_ref())));
                }
            }
        }
        if current != target || source.exists() {
            files.push((source, target));
        }
        for (_, target) in &files {
            match fs::symlink_metadata(target) {
                Ok(_) => return Err(format!("refusing to overwrite {}", target.display())),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(format!("inspect {}: {error}", target.display())),
            }
        }
        Ok(Self(files))
    }

    pub(super) fn apply(self) -> Result<(), String> {
        for (source, target) in self.0 {
            fs::rename(&source, &target)
                .map_err(|e| format!("move {} to {}: {e}", source.display(), target.display()))?;
        }
        Ok(())
    }
}
