//! Retain the existing exec child and output readers as a shell session.

use super::*;

#[derive(Clone, Copy)]
pub(super) enum BackgroundReason {
    User,
    AfterMillis(u64),
}

pub(super) struct Promotion {
    pub reason: BackgroundReason,
    pub deadline: Option<(tokio::time::Instant, u64)>,
}

pub(super) async fn until(deadline: Option<tokio::time::Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

pub(super) async fn after(millis: Option<u64>) -> BackgroundReason {
    match millis {
        Some(millis) => {
            tokio::time::sleep(Duration::from_millis(millis)).await;
            BackgroundReason::AfterMillis(millis)
        }
        None => std::future::pending().await,
    }
}

impl BashService {
    pub(super) fn background_exec(
        &self,
        command: &str,
        owner: Uuid,
        child: Child,
        shared: Arc<SessionShared>,
        max_bytes: usize,
        promotion: Promotion,
    ) -> generative_model::ToolResult {
        let process = ProcessOwner::retain(child, shared.clone());
        if let Some((deadline, timeout_ms)) = promotion.deadline {
            process.enforce_deadline(deadline, timeout_ms);
        }
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
                stdin: Arc::new(Mutex::new(None)),
                shared: shared.clone(),
                created_at: Instant::now(),
                last_used: Arc::new(Mutex::new(Instant::now())),
                process,
            },
        );
        let mut result = take_snapshot(
            &shared,
            &id,
            owner,
            command,
            SnapshotStatus::Backgrounded(promotion.reason),
            max_bytes,
        )
        .tool_result();
        result.content.push(generative_model::Content::Text {
            text:
                "The original exec had closed stdin; use read, signal, or close with this handle."
                    .into(),
        });
        if let Some((_, timeout_ms)) = promotion.deadline {
            result.content.push(generative_model::Content::Text {
                text: format!("The explicit timeout_ms={timeout_ms} remains a hard deadline from command start and will kill the process group."),
            });
        }
        result
    }
}
