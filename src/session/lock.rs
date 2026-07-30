//! Single-writer guard for a live session.
//!
//! Two myco processes resuming the same session id both rewrite the whole
//! document every turn, so the loser's turns disappear — and both append to one
//! `.console`. That is easy to hit by accident in the tool's own habitat: a
//! second tmux pane, or `--resume` in a script while the REPL is open.
//!
//! The guard is an advisory `flock(2)` on a sibling `{id}.lock`, held for as
//! long as the session is live. The lock file is a separate inode on purpose:
//! [`crate::session::atomically_write`] replaces `{id}.json` by rename, which
//! would leave a lock on the old inode.
//!
//! Reading is never blocked — `session_history`, `list_recent`, the session
//! browser and `/sessions` all take no lock. Only a process that intends to
//! *write* a session acquires one, and the kernel releases it if that process
//! dies, so there is no stale-lock recovery to get wrong.

use std::fs::{File, OpenOptions};
use std::path::PathBuf;

use super::session_file_path;

/// Why a session could not be locked for writing.
#[derive(Debug)]
pub enum SessionLockError {
    /// Another live process holds this session.
    Busy { path: PathBuf },
    /// Locking is unavailable here (filesystem without `flock` support, missing
    /// permissions). Callers continue **unlocked** — refusing to open a session
    /// because the lock file could not be created would be worse than the
    /// concurrent-write risk it guards against.
    Unavailable(String),
}

impl std::fmt::Display for SessionLockError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SessionLockError::Busy { path } => write!(
                f,
                "session is already open in another myco process (lock: {})",
                path.display()
            ),
            SessionLockError::Unavailable(why) => write!(f, "could not lock session: {why}"),
        }
    }
}

/// Held for as long as a session is the live one. Dropping it closes the
/// descriptor, which releases the `flock`.
#[derive(Debug)]
pub struct SessionWriteLock {
    _file: File,
    path: PathBuf,
}

impl SessionWriteLock {
    /// Take the write lock for `session_id`.
    pub fn acquire(session_id: &str) -> Result<Self, SessionLockError> {
        let path = session_file_path(session_id, "lock");
        if let Some(parent) = path.parent()
            && let Err(e) = std::fs::create_dir_all(parent)
        {
            return Err(SessionLockError::Unavailable(format!(
                "create {}: {e}",
                parent.display()
            )));
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(|e| SessionLockError::Unavailable(format!("open {}: {e}", path.display())))?;

        match try_lock_exclusive(&file) {
            LockAttempt::Acquired => Ok(Self { _file: file, path }),
            LockAttempt::Busy => Err(SessionLockError::Busy { path }),
            LockAttempt::Failed(why) => Err(SessionLockError::Unavailable(why)),
        }
    }

    pub fn path(&self) -> &std::path::Path {
        &self.path
    }
}

enum LockAttempt {
    Acquired,
    Busy,
    Failed(String),
}

#[cfg(unix)]
fn try_lock_exclusive(file: &File) -> LockAttempt {
    use std::os::unix::io::AsRawFd;
    // SAFETY: flock(2) takes a file descriptor and a flag; no pointers. The fd
    // is owned by `file` and outlives the call.
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc == 0 {
        return LockAttempt::Acquired;
    }
    let err = std::io::Error::last_os_error();
    match err.raw_os_error() {
        // Both spellings mean "held by someone else"; they are equal on Linux
        // and distinct on some BSDs.
        Some(code) if code == libc::EWOULDBLOCK || code == libc::EAGAIN => LockAttempt::Busy,
        _ => LockAttempt::Failed(err.to_string()),
    }
}

#[cfg(not(unix))]
fn try_lock_exclusive(_file: &File) -> LockAttempt {
    LockAttempt::Failed("advisory locking is only implemented on unix".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The second writer of a session must be refused, and must become
    /// acceptable again once the first releases.
    #[test]
    fn a_second_writer_is_refused_until_the_first_releases() {
        // RAII: MYCO_HOME under the myco-home lock, cleared on drop.
        let _home = crate::test_support::temp_home("lock-busy");

        let id = "aa00bb11cc22dd33ee44ff5566778899";
        let first = SessionWriteLock::acquire(id).expect("first writer acquires");

        match SessionWriteLock::acquire(id) {
            Err(SessionLockError::Busy { .. }) => {}
            Ok(_) => panic!("second writer must not acquire while the first holds"),
            Err(other) => panic!("expected Busy, got {other}"),
        }

        // A different session is unaffected.
        let other_id = "bb00cc11dd22ee33ff44005566778899";
        let _other = SessionWriteLock::acquire(other_id).expect("distinct session locks freely");

        drop(first);
        let _reacquired = SessionWriteLock::acquire(id).expect("lock is released on drop");
    }

    /// The message a refused writer prints has to name the session and the lock.
    #[test]
    fn busy_error_message_points_at_the_other_process() {
        let path = PathBuf::from("/tmp/x/aa.lock");
        let msg = SessionLockError::Busy { path }.to_string();
        assert!(
            msg.contains("already open in another myco process"),
            "{msg}"
        );
        assert!(msg.contains("aa.lock"), "{msg}");
    }
}
