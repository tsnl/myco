//! Retain the existing exec child and output readers as a shell session.

use super::*;

impl BashService {
    pub(super) fn background_exec(
        &self,
        command: &str,
        owner: Uuid,
        child: Child,
        shared: Arc<SessionShared>,
        max_bytes: usize,
    ) -> generative_model::ToolResult {
        let pid = child.id();
        {
            let mut buffer = lock_unpoisoned(&shared.buffer);
            let (stdout, stderr) = buffer.exec_capture.take().expect("foreground exec capture");
            buffer.stdout = render_capture(&stdout, EXEC_CAPTURE_CAP + 64 * 1024).into_bytes();
            buffer.stderr = render_capture(&stderr, EXEC_CAPTURE_CAP + 64 * 1024).into_bytes();
        }
        let id = format!("bg-{}", Uuid::new_v4().simple());
        // Promotion spawns no new process, so the admission limit for `start`
        // must not prevent retaining work that is already running.
        self.sessions().insert(
            id.clone(),
            Session {
                owner,
                cmdline: command.into(),
                stdin: Mutex::new(None),
                shared: shared.clone(),
                created_at: Instant::now(),
                last_used: Mutex::new(Instant::now()),
                pid,
            },
        );
        spawn_waiter(child, shared.clone());
        let mut result = take_snapshot(
            &shared,
            &id,
            owner,
            command,
            SnapshotStatus::Backgrounded,
            max_bytes,
        )
        .tool_result();
        result.content.push(generative_model::Content::Text {
            text:
                "The original exec had closed stdin; use read, signal, or close with this handle."
                    .into(),
        });
        result
    }
}
