//! Archive relocation keeps the writer-lock inode fixed and moves the JSON last.
//! Interrupted moves remain readable: sidecars resolve in either location, and
//! repeating archive/restore moves the remaining files without overwriting any.

use std::fs;
use std::path::PathBuf;

use super::listing::iter_sharded_session_files;
use super::{Session, SessionLockError, SessionWriteLock, session_file_path, session_root};

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::*;
    use crate::test_support::{persist_messages, temp_home, user};

    #[test]
    fn archived_sessions_remain_searchable_and_restorable_with_lineage_intact() {
        let _home = temp_home("session-archive");
        let mut session = Session::new("test");
        session.replace_context(vec![user("archive needle")], None);
        session.set_scratchpad("keep this context".into()).unwrap();
        session.save().unwrap();
        let child = session.fork_child("test");
        child.save().unwrap();
        let id = session.id.clone();
        let active = ActiveSession::new(session);
        let _lock = SessionWriteLock::acquire(&id).unwrap();
        active.set_session_archived(None, true).unwrap();
        assert!(list_sessions().unwrap().is_empty());
        let archived = list_sessions_with_filter(false, ArchiveFilter::Archived).unwrap();
        assert_eq!(archived.len(), 1);
        assert_eq!(
            search_sessions(&archived, "needle", 20).unwrap().entries[0].id,
            id
        );
        let saved = Session::load_by_id_or_prefix(&id).unwrap();
        assert!(saved.archived);
        assert_eq!(saved.scratchpad, "keep this context");
        let child = Session::load_by_id_or_prefix(&child.id).unwrap();
        assert!(!child.archived);
        assert_eq!(child.parent_session_id.as_deref(), Some(id.as_str()));
        active.set_session_archived(None, false).unwrap();
        assert_eq!(list_sessions().unwrap()[0].id, id);
        assert!(
            list_sessions_with_filter(false, ArchiveFilter::Archived)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn archive_moves_session_files_and_restores_them_without_releasing_the_writer_lock() {
        let home = temp_home("archive-files");
        let session = Session::new("test");
        session.save().unwrap();
        let original = session.json_path();
        let id = session.id.clone();
        let summary = session.thread_summary_path(&session.active_thread().id);
        fs::write(session.history_path(), "previous input\n").unwrap();
        fs::write(&summary, "thread summary").unwrap();
        let active = ActiveSession::new(session);
        let lock = SessionWriteLock::acquire(&id).unwrap();
        fs::write(active.snapshot().console_path(), "legacy console needle\n").unwrap();

        active.set_archived(true).unwrap();
        let saved = Session::load_by_id_or_prefix(&id[..12]).unwrap();
        let archive = home.path().join("session/archived").join(&id[..2]);
        assert_eq!(saved.json_path(), archive.join(format!("{id}.json")));
        assert!(!original.exists());
        assert!(!summary.exists());
        assert!(lock.path().exists());
        assert!(matches!(
            SessionWriteLock::acquire(&id),
            Err(SessionLockError::Busy { .. })
        ));
        assert_eq!(
            fs::read_to_string(saved.history_path()).unwrap(),
            "previous input\n"
        );
        assert_eq!(
            fs::read_to_string(saved.thread_summary_path(&saved.active_thread().id)).unwrap(),
            "thread summary"
        );
        let thread = active.with(|session| session.active_thread().id.clone());
        persist_messages(
            &active,
            &thread,
            &[user("continued while archived")],
            None,
            true,
        )
        .unwrap();
        assert!(!original.exists());
        let listed = list_sessions_with_filter(false, ArchiveFilter::Archived).unwrap();
        assert_eq!(
            search_sessions(&listed, "needle", 10).unwrap().entries[0].id,
            id
        );

        active.set_archived(false).unwrap();
        assert!(original.exists());
        assert!(!archive.join(format!("{id}.json")).exists());
        assert_eq!(
            fs::read_to_string(original.with_extension("console")).unwrap(),
            "legacy console needle\n"
        );
        assert!(matches!(
            SessionWriteLock::acquire(&id),
            Err(SessionLockError::Busy { .. })
        ));
        assert_eq!(
            Session::load_by_id_or_prefix(&id)
                .unwrap()
                .active_thread()
                .messages,
            vec![user("continued while archived")]
        );
    }

    #[test]
    fn legacy_archives_can_be_listed_and_moved_by_archiving_again() {
        let _home = temp_home("legacy-archive-files");
        let mut session = Session::new("test");
        session.archived = true;
        session.save().unwrap();
        let old_path = session.json_path();
        let listed = list_sessions_with_filter(false, ArchiveFilter::Archived).unwrap();
        assert_eq!(listed[0].id, session.id);
        let active = ActiveSession::new(Session::load_by_id_or_prefix(&session.id).unwrap());
        active.set_archived(true).unwrap();
        assert!(!old_path.exists());
        assert!(active.snapshot().json_path().exists());
        assert!(list_sessions().unwrap().is_empty());
    }

    #[test]
    fn startup_moves_legacy_archives_but_skips_live_writers_and_active_sessions() {
        let _home = temp_home("archive-startup");
        let active = Session::new("test");
        active.save().unwrap();
        let mut archived = Session::new("test");
        archived.archived = true;
        archived.save().unwrap();
        let original = archived.json_path();
        let bytes = fs::read(&original).unwrap();
        fs::write(archived.history_path(), "preserve history").unwrap();
        let lock = SessionWriteLock::acquire(&archived.id).unwrap();
        migrate_archived_sessions().unwrap();
        assert!(original.exists());
        drop(lock);
        migrate_archived_sessions().unwrap();
        migrate_archived_sessions().unwrap();
        assert!(!original.exists());
        assert_eq!(fs::read(archived.json_path()).unwrap(), bytes);
        assert_eq!(
            fs::read_to_string(archived.history_path()).unwrap(),
            "preserve history"
        );
        assert!(active.json_path().exists());
        assert_eq!(list_sessions().unwrap()[0].id, active.id);
    }

    #[test]
    fn interrupted_archive_moves_are_readable_and_completed_at_startup() {
        let _home = temp_home("archive-interrupted");
        let mut session = Session::new("test");
        session.archived = true;
        session.save().unwrap();
        let original = session.json_path();
        fs::write(session.console_path(), "preserved console").unwrap();
        fs::write(session.history_path(), "preserved history").unwrap();
        let moved = archive::file_path(&session.id, "console", true);
        fs::create_dir_all(moved.parent().unwrap()).unwrap();
        fs::rename(session.console_path(), &moved).unwrap();
        assert_eq!(
            fs::read_to_string(session.console_path()).unwrap(),
            "preserved console"
        );
        migrate_archived_sessions().unwrap();
        assert!(!original.exists());
        assert_eq!(
            fs::read_to_string(session.console_path()).unwrap(),
            "preserved console"
        );
        assert_eq!(
            fs::read_to_string(session.history_path()).unwrap(),
            "preserved history"
        );
        let active = ActiveSession::new(session);
        active.set_archived(false).unwrap();
        assert!(original.exists());
        assert!(!moved.exists());
    }

    #[test]
    fn archive_conflicts_preserve_both_copies_and_active_listings_skip_archive_files() {
        let _home = temp_home("archive-conflict");
        let session = Session::new("test");
        session.save().unwrap();
        let original = session.json_path();
        let bytes = fs::read(&original).unwrap();
        let target = archive::file_path(&session.id, "json", true);
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        fs::write(&target, "an existing, unreadable archive").unwrap();
        let active = ActiveSession::new(session);
        assert!(
            active
                .set_archived(true)
                .unwrap_err()
                .contains("refusing to overwrite")
        );
        assert!(!active.snapshot().archived);
        assert_eq!(fs::read(&original).unwrap(), bytes);
        assert_eq!(
            fs::read_to_string(&target).unwrap(),
            "an existing, unreadable archive"
        );
        let (entries, skipped) = crate::session::listing::collect_session_entries(
            &session_root().unwrap(),
            false,
            ArchiveFilter::Active,
        )
        .unwrap();
        assert_eq!(entries.len(), 1);
        assert!(
            skipped.is_empty(),
            "ordinary listings must not parse archived files"
        );
    }

    #[test]
    fn interrupted_restore_stays_archived_and_retries_reunite_sidecars() {
        let _home = temp_home("restore-interrupted");
        let session = Session::new("test");
        session.save().unwrap();
        let original = session.json_path();
        fs::write(session.history_path(), "preserve this").unwrap();
        let active = ActiveSession::new(session);
        active.set_archived(true).unwrap();
        // A restore interrupted between its sidecar and JSON renames.
        fs::rename(
            active.snapshot().history_path(),
            original.with_extension("history"),
        )
        .unwrap();
        assert_eq!(
            list_sessions_with_filter(false, ArchiveFilter::Archived)
                .unwrap()
                .len(),
            1
        );
        active.set_archived(true).unwrap();
        assert!(!original.with_extension("history").exists());
        assert_eq!(
            fs::read_to_string(active.snapshot().history_path()).unwrap(),
            "preserve this"
        );
        // A restore interrupted after the JSON move but before clearing the flag.
        archive::Relocation::prepare(&active.id(), false)
            .unwrap()
            .apply()
            .unwrap();
        assert!(original.exists());
        assert!(list_sessions().unwrap().is_empty());
        assert_eq!(
            list_sessions_with_filter(false, ArchiveFilter::Archived)
                .unwrap()
                .len(),
            1
        );
        active.set_archived(false).unwrap();
        assert_eq!(list_sessions().unwrap().len(), 1);
    }
}
