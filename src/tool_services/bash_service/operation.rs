//! An authorized operation stays bound to one session instance across awaits.

use super::*;
use std::sync::Weak;

pub(super) struct SessionOperation {
    id: String,
    owner: Uuid,
    cmdline: String,
    stdin: Arc<Mutex<Option<ChildStdin>>>,
    shared: Arc<SessionShared>,
    last_used: Arc<Mutex<Instant>>,
    // Reads and writes must not postpone process cleanup after close/reap.
    process: Weak<ProcessOwner>,
}

impl Session {
    pub(super) fn operation(&self, id: &str) -> SessionOperation {
        SessionOperation {
            id: id.into(),
            owner: self.owner,
            cmdline: self.cmdline.clone(),
            stdin: self.stdin.clone(),
            shared: self.shared.clone(),
            last_used: self.last_used.clone(),
            process: Arc::downgrade(&self.process),
        }
    }
}

impl SessionOperation {
    pub(super) async fn write(&self, data: &str) -> Result<(), String> {
        self.shared.generation.fetch_add(1, Ordering::SeqCst);
        let stdin = lock_unpoisoned(&self.stdin).take();
        let Some(mut stdin) = stdin else {
            return Err(if lock_unpoisoned(&self.shared.buffer).exited {
                format!(
                    "session {:?} has exited; close it and start a new one",
                    self.id
                )
            } else {
                format!("session {:?} stdin is closed", self.id)
            });
        };
        let result = tokio::time::timeout(Duration::from_millis(STDIN_WRITE_TIMEOUT_MS), async {
            stdin.write_all(data.as_bytes()).await?;
            stdin.flush().await
        })
        .await;
        // A replacement with the same name owns a different stdin slot.
        *lock_unpoisoned(&self.stdin) = Some(stdin);
        result
            .map_err(|_| {
                format!("stdin write timed out after {STDIN_WRITE_TIMEOUT_MS}ms (child may not be reading stdin)")
            })?
            .map_err(|error| format!("stdin write failed: {error}"))?;
        *lock_unpoisoned(&self.last_used) = Instant::now();
        self.shared.notify.notify_waiters();
        Ok(())
    }

    pub(super) fn signal(&self, signal: SignalKind) -> Result<(), String> {
        let process = self
            .process
            .upgrade()
            .ok_or_else(|| format!("session {:?} has been closed", self.id))?;
        if lock_unpoisoned(&self.shared.buffer).exited {
            return Err(format!(
                "session {:?} has already exited; use close to stop remaining descendants",
                self.id
            ));
        }
        self.shared.generation.fetch_add(1, Ordering::SeqCst);
        process.signal(signal.as_libc()).map_err(|error| {
            format!(
                "could not send {} to session {:?}: {error}",
                signal.name(),
                self.id
            )
        })
    }

    /// Collect a snapshot for a tool call; refuses one already cancelled.
    pub(super) async fn collect(
        &self,
        wait: Wait,
        return_on_empty_idle: bool,
        ctx: &HostDispatchContext,
    ) -> Result<SessionSnapshot, String> {
        *lock_unpoisoned(&self.last_used) = Instant::now();
        if ctx.cancel.is_cancelled() {
            return Err("cancelled".into());
        }
        Ok(self
            .collect_output(wait, return_on_empty_idle, &ctx.cancel, &ctx.background)
            .await)
    }

    /// Drain the shared buffer until idle / timeout / exit / byte cap / cancel.
    ///
    /// Bytes returned are removed from the buffer so subsequent reads only see new data.
    /// On cancel, returns whatever is buffered with `TimedOut` status (session stays live).
    pub(super) async fn collect_output(
        &self,
        wait: Wait,
        return_on_empty_idle: bool,
        cancel: &CancelToken,
        background: &CancelToken,
    ) -> SessionSnapshot {
        let shared = &self.shared;
        let deadline = Instant::now() + Duration::from_millis(wait.timeout_ms.max(1));
        let idle = Duration::from_millis(wait.idle_ms.max(1));
        let max_bytes = wait.max_bytes.max(1);

        let mut last_activity = Instant::now();
        let (mut last_total, already_pending) = {
            let b = lock_unpoisoned(&shared.buffer);
            (
                b.total_bytes,
                !b.stdout.is_empty() || !b.stderr.is_empty() || b.is_finished(),
            )
        };

        // Track generation so a concurrent write resets idle.
        let mut seen_gen = shared.generation.load(Ordering::SeqCst);

        let status;

        loop {
            if cancel.is_cancelled() {
                status = SnapshotStatus::TimedOut;
                break;
            }

            let (total, finished, pending_len) = {
                let b = lock_unpoisoned(&shared.buffer);
                (
                    b.total_bytes,
                    b.is_finished(),
                    b.stdout.len().saturating_add(b.stderr.len()),
                )
            };

            if background.is_cancelled() {
                status = SnapshotStatus::Backgrounded(BackgroundReason::User);
                break;
            }
            if total > last_total {
                last_total = total;
                last_activity = Instant::now();
            }

            let current_gen = shared.generation.load(Ordering::SeqCst);
            if current_gen != seen_gen {
                seen_gen = current_gen;
                last_activity = Instant::now();
            }

            if pending_len >= max_bytes {
                status = SnapshotStatus::Truncated;
                break;
            }
            if finished {
                status = SnapshotStatus::Exited;
                break;
            }
            // Idle with data (or we entered with pending and it settled).
            if last_activity.elapsed() >= idle && pending_len > 0 {
                status = SnapshotStatus::Running;
                break;
            }
            // A plain start returns once the child has been quiet even if it has
            // emitted nothing. Reads and writes long-poll for their first byte.
            if last_activity.elapsed() >= idle
                && pending_len == 0
                && (already_pending || return_on_empty_idle)
            {
                // Entered with pending that was drained by a concurrent collector, or exited
                // flag raced; treat as settled running/empty.
                status = SnapshotStatus::Running;
                break;
            }
            if Instant::now() >= deadline {
                status = SnapshotStatus::TimedOut;
                break;
            }

            let remaining = deadline.saturating_duration_since(Instant::now());
            let slice = remaining.min(Duration::from_millis(50)).min(idle);
            tokio::select! {
                biased;
                _ = cancel.cancelled() => {
                    status = SnapshotStatus::TimedOut;
                    break;
                }
                _ = background.cancelled() => {
                    status = SnapshotStatus::Backgrounded(BackgroundReason::User);
                    break;
                }
                _ = tokio::time::timeout(slice, shared.notify.notified()) => {}
            }
        }

        self.take_snapshot(status, max_bytes)
    }

    /// Drain up to `max_bytes` from the buffer (stdout first, then stderr).
    pub(super) fn take_snapshot(
        &self,
        mut status: SnapshotStatus,
        max_bytes: usize,
    ) -> SessionSnapshot {
        let mut b = lock_unpoisoned(&self.shared.buffer);

        let take_stdout = b.stdout.len().min(max_bytes);
        let stdout: Vec<u8> = b.stdout.drain(..take_stdout).collect();
        let take_stderr = b.stderr.len().min(max_bytes - take_stdout);
        let stderr: Vec<u8> = b.stderr.drain(..take_stderr).collect();

        if !b.stdout.is_empty() || !b.stderr.is_empty() {
            if !matches!(status, SnapshotStatus::Backgrounded(_)) {
                status = SnapshotStatus::Truncated;
            }
        } else if b.is_finished() {
            status = SnapshotStatus::Exited;
        }

        let stdout = String::from_utf8_lossy(&stdout).into_owned();
        let stderr = String::from_utf8_lossy(&stderr).into_owned();
        SessionSnapshot {
            instance_id: self.shared.instance_id.clone(),
            session_id: self.id.clone(),
            owner: self.owner,
            cmdline: self.cmdline.clone(),
            status,
            exit_code: b.exit_code,
            exit_signal: b.exit_signal,
            background_output: b.exited && !b.is_finished(),
            exec_timeout_ms: b.exec_timeout_ms,
            bytes_returned: stdout.len().saturating_add(stderr.len()),
            stdout,
            stderr,
            bytes_dropped: std::mem::take(&mut b.dropped_bytes),
        }
    }
}
