//! Startup checks. Two kinds, deliberately apart:
//!
//! - [`fatal_startup_check`] stops the process (today: an oversized prelude —
//!   nothing shortens it to fit, so a prompt could not carry it honestly).
//! - [`prelude_warning`] is the one survivable problem worth a line on
//!   stderr: a prelude directory that exists but cannot be read, because a
//!   session then runs with an empty prelude and nothing else would say so.
//!
//! Everything else that used to be probed here (executables on PATH,
//! ssh-agent identities) now fails at the tool call that needs it, with the
//! failing program's own error — which is both less code and a message
//! closer to the actual problem.

use crate::prompts::{prelude, prelude_oversize};

/// The checks that stop startup, as a message to print before exiting, or
/// `None` when myco may run.
pub fn fatal_startup_check(max_prelude_bytes: usize) -> Option<String> {
    let over = prelude_oversize(max_prelude_bytes)?;
    let dir = prelude::dir()
        .map(|d| d.display().to_string())
        .unwrap_or_else(|_| "~/.myco/v2/workspace/prelude".into());
    Some(format!(
        "{}\n\
         refusing to start: agents would run on a prelude too big to carry, and trimming it \
         would hide the missing part from them.\n\
         fix by hand in {dir} — merge overlapping entries or delete dead ones (each is a \
         plain `*.md` file; `ls -S` lists the biggest first) — or raise max_prelude_bytes in \
         config.toml above {} bytes.",
        over.describe(),
        over.bytes,
    ))
}

/// A survivable startup problem worth one stderr line, or `None`.
pub fn prelude_warning() -> Option<String> {
    let failure = prelude::dir()
        .ok()
        .as_deref()
        .and_then(prelude::read_failure)?;
    Some(format!(
        "prelude directory unreadable: {failure}\n\
         every agent this session runs with an empty prelude; `prelude` action=list \
         will look empty too, so do not curate against it until this is fixed"
    ))
}
