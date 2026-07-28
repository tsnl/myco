//! Per-session plain-text mirror of the interactive console.
//!
//! The interactive CLI's Ui ([`crate::tui::TuiProducer`]) subscribes a
//! [`crate::tui::ConsoleTuiSink`] to the TUI stream, which plain-encodes every
//! user-visible block — banner, preflight WARNING, USER headers + input, the
//! streamed ASSISTANT section, live ERROR / cancellation notices, and
//! meta-command output (`/hosts`, `/session`, …) — into [`ConsoleLog::append`],
//! landing in `{id}.console` beside the session JSON. The result is an
//! append-only, escape-free transcript of exactly what scrolled past — the
//! agent can read it (via its file tools) to answer questions about the live
//! session, including the live-only sections that never reach the JSON
//! history (see [`crate::session::write_session_history`]).
//!
//! Escape-freedom is structural, not filtered: styling and content are
//! different [`crate::tui::TuiEvent`] variants, and the plain encoding simply
//! never emits style events — there is nothing to strip. One thing this is
//! *not*: a VT framebuffer. Cursor-addressed repaints (input re-echo, resize
//! reflow, Ctrl-L) are redraws of content already in the stream and are not
//! fed here, so the log is the logical transcript, not a screen snapshot.
//!
//! The current session id is resolved from the shared [`ActiveSession`] on
//! every append, so `/new`, `/compact`, and `/resume` (which swap the active
//! session) redirect the mirror to the new `{id}.console` with no extra
//! wiring. Files are opened for append, so a resumed session accumulates its
//! console across runs.

use std::fs::{File, OpenOptions};
use std::sync::Mutex;

use super::{ActiveSession, session_file_path};

/// Append-only plain-text mirror of the CLI console for the active session.
pub struct ConsoleLog {
    /// `None` disables the mirror (non-TTY stdout): every append is a no-op.
    active: Option<ActiveSession>,
    /// The `{id}.console` file currently open, tagged with its session id so a
    /// session swap triggers a reopen.
    open: Mutex<Option<(String, File)>>,
}

impl ConsoleLog {
    /// Mirror the console of whichever session `active` currently holds.
    /// `enabled` is the TTY decision — `false` yields a no-op mirror.
    pub fn new(active: ActiveSession, enabled: bool) -> Self {
        Self {
            active: enabled.then_some(active),
            open: Mutex::new(None),
        }
    }

    /// A permanently disabled mirror (tests, headless callers).
    pub fn disabled() -> Self {
        Self {
            active: None,
            open: Mutex::new(None),
        }
    }

    /// Append plain console text (escape-free by the TUI-stream contract) to
    /// the active session's `.console` file. Silent no-op when disabled or on
    /// any IO error — the mirror must never disrupt the live session.
    pub fn append(&self, text: &str) {
        use std::io::Write;
        let Some(active) = &self.active else {
            return;
        };
        // Resolve the id before locking `open` to keep the two session locks
        // strictly ordered.
        let id = active.id();
        let mut open = self.open.lock().unwrap_or_else(|e| e.into_inner());

        let stale = open.as_ref().is_none_or(|(open_id, _)| open_id != &id);
        if stale {
            match open_console_file(&id) {
                Ok(file) => *open = Some((id, file)),
                Err(_) => {
                    *open = None;
                    return;
                }
            }
        }

        if let Some((_, file)) = open.as_mut() {
            let _ = file.write_all(text.as_bytes());
        }
    }
}

fn open_console_file(id: &str) -> std::io::Result<File> {
    let path = session_file_path(id, "console");
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    OpenOptions::new().create(true).append(true).open(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_log_is_a_silent_noop() {
        // No panic, no file, nothing to observe.
        let log = ConsoleLog::disabled();
        log.append("hello\n");
    }

    #[test]
    fn console_path_is_the_id_sibling() {
        let session = crate::session::Session::new("m");
        let path = session.console_path();
        assert!(
            path.to_string_lossy()
                .ends_with(&format!("{}.console", session.id)),
            "{}",
            path.display()
        );
    }
}
