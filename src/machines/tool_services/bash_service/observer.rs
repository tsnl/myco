//! The observer surface: a person watching or driving a live session from
//! the web terminal, as opposed to the agent's own bounded interactions.
//! Reads come from the non-consuming scrollback and the screen model,
//! writes are gated on the keyboard lock, and none of it disturbs the
//! agent's drain buffer. The NDJSON host protocol mirrors this whole
//! surface, so a remote host's shells watch and drive exactly like local
//! ones.

use super::*;

impl BashService {
    /// One session as the shells rail lists it.
    pub fn shell_overviews(&self) -> Vec<ShellOverview> {
        let sessions = self.sessions();
        let mut out: Vec<ShellOverview> = sessions
            .iter()
            .map(|(id, s)| {
                let (running, exit_code) = s
                    .shared
                    .buffer
                    .lock()
                    .map(|b| (!b.exited, b.exit_code))
                    .unwrap_or((false, None));
                ShellOverview {
                    id: id.clone(),
                    cmdline: s.cmdline.clone(),
                    title: lock_unpoisoned(&s.title).clone(),
                    running,
                    exit_code,
                    lock: *lock_unpoisoned(&s.lock),
                    end_offset: lock_unpoisoned(&s.shared.scroll).end(),
                    created_secs: s.created_at.elapsed().as_secs(),
                    idle_secs: s
                        .last_used
                        .lock()
                        .map(|t| t.elapsed().as_secs())
                        .unwrap_or(0),
                    pty: s.shared.pty,
                }
            })
            .collect();
        out.sort_by(|a, b| a.id.cmp(&b.id));
        out
    }

    /// Scrollback from absolute offset `from`, non-consuming. A viewer that
    /// fell more than `max_bytes` behind is skipped forward — a terminal
    /// wants the present, and the returned `from` says the skip happened.
    pub fn shell_tail(&self, id: &str, from: u64, max_bytes: usize) -> Result<ShellTail, String> {
        let (shared, lock) = {
            let sessions = self.sessions();
            let session = sessions
                .get(id)
                .ok_or_else(|| format!("unknown session {id:?}"))?;
            (Arc::clone(&session.shared), *lock_unpoisoned(&session.lock))
        };
        let running = shared.buffer.lock().map(|b| !b.exited).unwrap_or(false);
        let scroll = lock_unpoisoned(&shared.scroll);
        let end = scroll.end();
        let floor = end.saturating_sub(max_bytes as u64);
        let (from, data) = scroll.tail(from.max(floor));
        Ok(ShellTail {
            from,
            end,
            data,
            running,
            lock,
        })
    }

    /// Move the keyboard between the user and the agent. Returns the
    /// *previous* state so a caller can tell a transition from an idempotent
    /// re-take (two clicks are not an error, and not worth announcing twice).
    pub fn shell_set_lock(&self, id: &str, lock: ShellLock) -> Result<ShellLock, String> {
        let sessions = self.sessions();
        let session = sessions
            .get(id)
            .ok_or_else(|| format!("unknown session {id:?}"))?;
        Ok(std::mem::replace(
            &mut *lock_unpoisoned(&session.lock),
            lock,
        ))
    }

    /// One session's fresh overview (for input/lock responses).
    pub fn shell_overview(&self, id: &str) -> Result<ShellOverview, String> {
        self.shell_overviews()
            .into_iter()
            .find(|s| s.id == id)
            .ok_or_else(|| format!("unknown session {id:?}"))
    }

    /// Wait until the session's observable output moves past `seen` (the
    /// scrollback end offset — every observed byte lands there first), the
    /// child exits, or `timeout` passes; returns the current end offset.
    /// The change-driven half of a live terminal: a screen pusher waits
    /// here instead of polling.
    pub async fn shell_wait_change(
        &self,
        id: &str,
        seen: u64,
        timeout: Duration,
    ) -> Result<u64, String> {
        let shared = {
            let sessions = self.sessions();
            let session = sessions
                .get(id)
                .ok_or_else(|| format!("unknown session {id:?}"))?;
            Arc::clone(&session.shared)
        };
        let deadline = Instant::now() + timeout;
        loop {
            // Register *before* checking, or a notify that fires between the
            // check and the await is lost and this sleeps out its whole
            // timeout. `enable` is the documented pattern for exactly that.
            let notified = shared.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let end = lock_unpoisoned(&shared.scroll).end();
            if end != seen {
                return Ok(end);
            }
            let now = Instant::now();
            if now >= deadline {
                return Ok(end);
            }
            let _ = tokio::time::timeout(deadline - now, notified).await;
        }
    }

    /// The session's rendered terminal screen, non-consuming (see
    /// [`ShellScreen`]). No lock gate: watching is always fine.
    pub fn shell_screen(&self, id: &str) -> Result<ShellScreen, String> {
        let shared = {
            let sessions = self.sessions();
            let session = sessions
                .get(id)
                .ok_or_else(|| format!("unknown session {id:?}"))?;
            Arc::clone(&session.shared)
        };
        Ok(render_screen(&shared))
    }

    /// Resize the session's terminal: the screen model always, and the pty
    /// window (TIOCSWINSZ → SIGWINCH to the child) when there is one.
    /// Requires the user to hold the keyboard — the agent sized the terminal
    /// it asked for, and a viewer must not reshape it under a program the
    /// agent is driving.
    pub fn shell_resize(&self, id: &str, cols: u16, rows: u16) -> Result<(), String> {
        if !(1..=500).contains(&cols) || !(1..=500).contains(&rows) {
            return Err(format!("unreasonable terminal size {cols}x{rows}"));
        }
        let (shared, stdin_slot) = {
            let sessions = self.sessions();
            let session = sessions
                .get(id)
                .ok_or_else(|| format!("unknown session {id:?}"))?;
            if *lock_unpoisoned(&session.lock) != ShellLock::User {
                return Err(format!(
                    "session {id:?} is assistant-locked: take the keyboard first"
                ));
            }
            (
                Arc::clone(&session.shared),
                lock_unpoisoned(&session.stdin).take(),
            )
        };
        // Stdin briefly out for a write: report busy rather than resize the
        // model without the pty — the viewer's next size check retries.
        let Some(stdin) = stdin_slot else {
            return Err(format!("session {id:?} input is busy; retrying"));
        };
        let result = match &stdin {
            SessionInput::Pty(w) => w.resize(cols, rows).map_err(|e| format!("pty resize: {e}")),
            SessionInput::Pipe(_) => Ok(()),
        };
        self.return_stdin(id, stdin);
        result?;
        lock_unpoisoned(&shared.screen).set_size(rows, cols);
        shared.notify.notify_waiters();
        Ok(())
    }

    /// Open a terminal on the user's behalf, owned by `owner` — the agent's
    /// id, so the session is fully addressable by its bash tool (write,
    /// read, signal, close) like any it started itself. Starts **user-held**:
    /// whoever opened it is typing; the head button hands it over. With no
    /// name given, the first free `term-N` is picked.
    pub async fn shell_start(
        &self,
        name: Option<&str>,
        owner: Uuid,
        command: Option<&str>,
        pty: bool,
        cols: Option<u16>,
        rows: Option<u16>,
    ) -> Result<ShellOverview, String> {
        let id = match name.map(str::trim).filter(|n| !n.is_empty()) {
            Some(n) => n.to_string(),
            None => {
                let sessions = self.sessions();
                (1..)
                    .map(|n| format!("term-{n}"))
                    .find(|c| !sessions.contains_key(c))
                    .expect("unbounded candidate range")
            }
        };
        let spec = SpawnSpec {
            command,
            cwd: None,
            pty,
            cols,
            rows,
            lock: ShellLock::User,
        };
        self.spawn_session(&id, owner, spec).await?;
        self.shell_overview(&id)
    }

    /// Set or clear a session's display name. Organization only — the id is
    /// still the address, so nothing the agent holds breaks.
    pub fn shell_rename(&self, id: &str, title: Option<&str>) -> Result<ShellOverview, String> {
        {
            let sessions = self.sessions();
            let session = sessions
                .get(id)
                .ok_or_else(|| format!("unknown session {id:?}"))?;
            *lock_unpoisoned(&session.title) = title
                .map(str::trim)
                .filter(|t| !t.is_empty())
                .map(String::from);
        }
        self.shell_overview(id)
    }

    /// A keystroke line from the user. Requires the user to hold the
    /// keyboard — accepting input while the agent is mid-`write` would
    /// interleave two writers' bytes into one stdin.
    pub async fn shell_user_write(&self, id: &str, data: &str) -> Result<(), String> {
        {
            let sessions = self.sessions();
            let session = sessions
                .get(id)
                .ok_or_else(|| format!("unknown session {id:?}"))?;
            if *lock_unpoisoned(&session.lock) != ShellLock::User {
                return Err(format!(
                    "session {id:?} is assistant-locked: take the keyboard first"
                ));
            }
        }
        self.write_to_session(id, data).await
    }
}
