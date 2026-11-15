//! Compaction at settled boundaries: an injected [`Compactor`] writes the
//! successor, and the runner commits it under the session writer and installs
//! it without replacing live tools.

use std::sync::Arc;

use crate::SessionRuntime;
use crate::agent::{Agent, AgentInteractionError, StateError};
use crate::core::{Async, CancelToken};
use crate::generative_model::{CatalogModel, Content, Message};
use crate::prompts;
use crate::session::{CompactOutcome, Session, SessionWriter, Thread};

use super::super::compact_worker::run_compact_worker;
use super::super::{persist_session, wire_checkpoint};
use super::{Workflow, WorkflowEvent};

/// Builds a successor for the saved active thread of `predecessor`. The runner
/// commits it under the session writer and owns retries between calls.
pub trait Compactor: Send + Sync {
    /// `single_attempt` asks for one model attempt, leaving retry timing to an
    /// auto-continuing runner. `observer` receives recovery warnings.
    fn compact(
        self: Arc<Self>,
        predecessor: Session,
        single_attempt: bool,
        observer: Arc<dyn Fn(WorkflowEvent) + Send + Sync>,
        cancel: CancelToken,
    ) -> Async<Result<(Thread, CompactOutcome), AgentInteractionError>>;
}

pub struct ModelCompactor {
    pub model: CatalogModel,
    pub max_requests: usize,
}

impl Compactor for ModelCompactor {
    fn compact(
        self: Arc<Self>,
        predecessor: Session,
        single_attempt: bool,
        observer: Arc<dyn Fn(WorkflowEvent) + Send + Sync>,
        cancel: CancelToken,
    ) -> Async<Result<(Thread, CompactOutcome), AgentInteractionError>> {
        Box::pin(async move {
            run_compact_worker(
                &predecessor,
                &self.model,
                self.max_requests,
                single_attempt,
                observer,
                cancel,
                |model| model,
            )
            .await
        })
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum CompactionMode {
    Manual,
    Automatic,
    RequestSize,
}

impl Workflow {
    pub(super) async fn auto_compact(
        &mut self,
        agent: &mut Agent,
        runtime: &Arc<SessionRuntime>,
        writer: &SessionWriter,
        cancel: CancelToken,
    ) -> Result<(), AgentInteractionError> {
        match self
            .compact(agent, runtime, writer, cancel, CompactionMode::Automatic)
            .await
        {
            Ok(_) => Ok(()),
            Err(AgentInteractionError::Cancelled) => {
                agent.cancel_at_boundary()?;
                Err(AgentInteractionError::Cancelled)
            }
            Err(error) => Err(error),
        }
    }

    pub(super) async fn compact(
        &mut self,
        agent: &mut Agent,
        runtime: &Arc<SessionRuntime>,
        writer: &SessionWriter,
        cancel: CancelToken,
        mode: CompactionMode,
    ) -> Result<CompactOutcome, AgentInteractionError> {
        let automatic = mode != CompactionMode::Manual;
        if (automatic && !agent.state().can_replace_at_boundary())
            || (!automatic && !agent.state().is_idle())
        {
            return Err(StateError::Busy.into());
        }
        let compactor = self
            .compactor
            .clone()
            .ok_or_else(|| AgentInteractionError::Compaction("no compactor configured".into()))?;
        if !agent.history().is_empty()
            && let Some(notice) = self.runtime_notice(agent, runtime).await
        {
            if automatic {
                self.checkpointed_update(agent, runtime, &cancel, |agent| {
                    agent.append_system(vec![notice.clone()])
                })
                .await?;
            } else {
                agent.append_system(vec![notice])?;
            }
        }
        self.compact_retry(mode, runtime, &cancel, || {
            persist_session(agent, runtime.session(), true)
                .map_err(AgentInteractionError::Checkpoint)
        })
        .await?;
        let predecessor = runtime.session().snapshot();
        (self.observer)(WorkflowEvent::Compacting {
            session_id: predecessor.id.clone(),
            thread_id: predecessor.active_thread().id.clone(),
            automatic,
        });
        let (mut successor, outcome) = self
            .summarize(compactor, predecessor, runtime, &cancel, automatic)
            .await?;
        if mode == CompactionMode::RequestSize {
            omit_rejected_images(&mut successor, &outcome);
        }
        if automatic {
            successor.messages.push(Message::UserMessage {
                content: vec![Content::System {
                    kind: "continuation".into(),
                    text: prompts::COMPACTION_RESUMPTION.into(),
                    data: serde_json::json!({"reason":"auto_compaction"}),
                }],
            });
        }
        self.compact_retry(mode, runtime, &cancel, || {
            writer
                .commit_thread(successor.clone())
                .map_err(AgentInteractionError::Checkpoint)
        })
        .await?;
        self.compact_retry(mode, runtime, &cancel, || {
            runtime.install_compacted_context(agent, automatic)
        })
        .await?;
        self.awaiting_compacted_usage = automatic;
        self.skip_preflight = automatic;
        wire_checkpoint(agent, runtime.session());
        self.compact_retry(mode, runtime, &cancel, || {
            persist_session(agent, runtime.session(), true)
                .map_err(AgentInteractionError::Checkpoint)
        })
        .await?;
        (self.observer)(WorkflowEvent::Compacted(outcome.clone()));
        Ok(outcome)
    }

    async fn summarize(
        &self,
        compactor: Arc<dyn Compactor>,
        predecessor: Session,
        runtime: &SessionRuntime,
        cancel: &CancelToken,
        automatic: bool,
    ) -> Result<(Thread, CompactOutcome), AgentInteractionError> {
        let started = std::time::Instant::now();
        let mut progress = tokio::time::interval(std::time::Duration::from_secs(10));
        progress.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        progress.tick().await;
        let mut retry = super::super::autonomy::Retry::default();
        loop {
            // Explicit manual compaction retains the worker's configured retries.
            let single_attempt =
                automatic && runtime.session().with(|session| session.auto_continue);
            let mut work = compactor.clone().compact(
                predecessor.clone(),
                single_attempt,
                self.observer.clone(),
                cancel.clone(),
            );
            let result = loop {
                tokio::select! {
                    biased;
                    result = &mut work => break result,
                    _ = progress.tick() => (self.observer)(WorkflowEvent::CompactionProgress {
                        elapsed: started.elapsed(),
                    }),
                }
            };
            if cancel.is_cancelled() {
                return Err(AgentInteractionError::Cancelled);
            }
            match result {
                Ok(successor) => return Ok(successor),
                Err(error) if !automatic => return Err(error),
                Err(error) => {
                    retry
                        .wait(error, runtime.session(), cancel, &*self.observer)
                        .await?
                }
            }
        }
    }

    async fn compact_retry<T>(
        &self,
        mode: CompactionMode,
        runtime: &SessionRuntime,
        cancel: &CancelToken,
        mut operation: impl FnMut() -> Result<T, AgentInteractionError>,
    ) -> Result<T, AgentInteractionError> {
        if mode == CompactionMode::Manual {
            operation()
        } else {
            self.retry(runtime, cancel, operation).await
        }
    }
}

// Recent images survive ordinary compaction. A size rejection requires a
// text-only tail; the committed predecessor keeps the originals inspectable.
fn omit_rejected_images(thread: &mut Thread, outcome: &CompactOutcome) {
    let notice = format!(
        "[Image omitted to reduce request size. Use session_history to inspect the original in session {}, thread {} before relying on its contents.]",
        outcome.session_id, outcome.predecessor_id,
    );
    for message in &mut thread.messages {
        match message {
            Message::UserMessage { content } | Message::AssistantMessage { content, .. } => {
                replace_images_with_notice(content, &notice);
            }
            Message::ToolResults { tool_use_results } => {
                for result in tool_use_results {
                    replace_images_with_notice(&mut result.content, &notice);
                }
            }
        }
    }
}

fn replace_images_with_notice(parts: &mut [Content], notice: &str) {
    for part in parts {
        if matches!(part, Content::Image { .. }) {
            *part = Content::Text {
                text: notice.into(),
            };
        }
    }
}
