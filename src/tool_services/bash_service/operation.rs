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

    pub(super) async fn collect(
        &self,
        timeout_ms: u64,
        idle_ms: u64,
        max_bytes: usize,
        return_on_empty_idle: bool,
        ctx: HostDispatchContext,
    ) -> Result<SessionSnapshot, String> {
        *lock_unpoisoned(&self.last_used) = Instant::now();
        if ctx.cancel.is_cancelled() {
            return Err("cancelled".into());
        }
        Ok(collect_output(
            &self.shared,
            &self.id,
            self.owner,
            &self.cmdline,
            timeout_ms,
            idle_ms,
            max_bytes,
            return_on_empty_idle,
            &ctx.cancel,
            &ctx.background,
        )
        .await)
    }
}
