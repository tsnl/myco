//! Per-session plain-text mirror of the interactive console.
//!
//! The CLI feeds every user-visible block it prints (banner, preflight
//! WARNING, USER headers + input, the streamed ASSISTANT section, live ERROR
//! and cancellation notices) to [`ConsoleLog::append`], which strips ANSI and
//! writes it to `{id}.console` beside the session JSON. The result is an
//! append-only, escape-free transcript of exactly what scrolled past — the
//! agent can read it (via its file tools) to answer questions about the live
//! session, including the live-only sections that never reach the JSON
//! history (see [`crate::session::write_session_history`]).
//!
//! Two things this is *not*: it is not a VT framebuffer (cursor-addressed
//! repaints — input re-echo, resize reflow — are simply not fed here, so the
//! log never sees the pre-repaint state), and it is not styled (stripping SGR
//! from the already-rendered bytes recovers the plain content exactly, by the
//! additive-only rendering invariant).
//!
//! The current session id is resolved from the shared [`ActiveSession`] on
//! every append, so `/new`, `/compact`, and `/resume` (which swap the active
//! session) redirect the mirror to the new `{id}.console` with no extra
//! wiring. Files are opened for append, so a resumed session accumulates its
//! console across runs.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::sync::Mutex;

use super::{ActiveSession, session_file_path};

/// Append-only, ANSI-stripped mirror of the CLI console for the active session.
pub struct ConsoleLog {
    /// `None` disables the mirror (non-TTY stdout): every append is a no-op.
    active: Option<ActiveSession>,
    inner: Mutex<Inner>,
}

struct Inner {
    /// The `{id}.console` file currently open, tagged with its session id so a
    /// session swap triggers a reopen.
    open: Option<(String, File)>,
    stripper: AnsiStripper,
}

impl ConsoleLog {
    /// Mirror the console of whichever session `active` currently holds.
    /// `enabled` is the TTY decision — `false` yields a no-op mirror.
    pub fn new(active: ActiveSession, enabled: bool) -> Self {
        Self {
            active: enabled.then_some(active),
            inner: Mutex::new(Inner {
                open: None,
                stripper: AnsiStripper::default(),
            }),
        }
    }

    /// A permanently disabled mirror (tests, headless callers).
    pub fn disabled() -> Self {
        Self {
            active: None,
            inner: Mutex::new(Inner {
                open: None,
                stripper: AnsiStripper::default(),
            }),
        }
    }

    /// Append already-rendered console bytes (SGR stripped) to the active
    /// session's `.console` file. Silent no-op when disabled or on any IO
    /// error — the mirror must never disrupt the live session.
    pub fn append(&self, rendered: &str) {
        let Some(active) = &self.active else {
            return;
        };
        // Resolve the id before locking `inner` to keep the two session locks
        // strictly ordered.
        let id = active.id();
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());

        let stale = inner
            .open
            .as_ref()
            .is_none_or(|(open_id, _)| open_id != &id);
        if stale {
            match open_console_file(&id) {
                Ok(file) => inner.open = Some((id, file)),
                Err(_) => {
                    inner.open = None;
                    return;
                }
            }
        }

        let Inner { open, stripper } = &mut *inner;
        let mut bytes = Vec::with_capacity(rendered.len());
        stripper.strip_into(rendered, &mut bytes);
        if let Some((_, file)) = open {
            let _ = file.write_all(&bytes);
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

/// Streaming stripper for ANSI escape sequences (`ESC [ … final`, plus lone
/// two-byte `ESC x`). State carries across [`Self::strip_into`] calls so a
/// sequence split across chunk boundaries is still removed. Operates on bytes:
/// escape bytes are ASCII, so multibyte UTF-8 passes through untouched and the
/// output stays valid UTF-8.
#[derive(Default)]
struct AnsiStripper {
    state: StripState,
}

#[derive(Default, Clone, Copy)]
enum StripState {
    #[default]
    Normal,
    /// Saw `ESC`; awaiting `[` (CSI) or a final byte (other escape).
    Esc,
    /// Inside a CSI sequence; consume until the final byte `0x40..=0x7E`.
    Csi,
}

impl AnsiStripper {
    fn strip_into(&mut self, input: &str, out: &mut Vec<u8>) {
        for &b in input.as_bytes() {
            self.state = match self.state {
                StripState::Normal => {
                    if b == 0x1b {
                        StripState::Esc
                    } else {
                        out.push(b);
                        StripState::Normal
                    }
                }
                StripState::Esc => {
                    if b == b'[' {
                        StripState::Csi
                    } else {
                        // Lone two-byte escape (e.g. `ESC c`): drop this byte too.
                        StripState::Normal
                    }
                }
                StripState::Csi => {
                    if (0x40..=0x7e).contains(&b) {
                        StripState::Normal
                    } else {
                        StripState::Csi
                    }
                }
            };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strip(chunks: &[&str]) -> String {
        let mut s = AnsiStripper::default();
        let mut out = Vec::new();
        for c in chunks {
            s.strip_into(c, &mut out);
        }
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn strips_sgr_and_keeps_content() {
        assert_eq!(strip(&["\x1b[0;1;36mUSER\x1b[0m"]), "USER");
        assert_eq!(strip(&["plain text\n"]), "plain text\n");
        // Cursor-movement CSI (would only appear defensively) is removed too.
        assert_eq!(strip(&["a\x1b[2Kb\x1b[3Jc"]), "abc");
    }

    #[test]
    fn strips_sequence_split_across_chunks() {
        // The escape straddles the boundary — state must carry.
        assert_eq!(
            strip(&["dim \x1b[0", ";2mthought\x1b", "[0m done"]),
            "dim thought done"
        );
    }

    #[test]
    fn preserves_multibyte_utf8() {
        assert_eq!(strip(&["\x1b[1m你好 🚀 café\x1b[0m"]), "你好 🚀 café");
        // Split inside a multibyte char is fine: only ASCII escape bytes are dropped.
        assert_eq!(strip(&["\x1b[1m你", "好\x1b[0m"]), "你好");
    }

    #[test]
    fn lone_escape_drops_two_bytes() {
        assert_eq!(strip(&["a\x1bcb"]), "ab");
    }

    #[test]
    fn disabled_log_is_a_silent_noop() {
        // No panic, no file, nothing to observe.
        let log = ConsoleLog::disabled();
        log.append("\x1b[1mhello\x1b[0m");
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
