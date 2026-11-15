//! Session workflows shared by interactive and scripted callers. Compaction
//! replaces context only at settled boundaries; live tools keep their owner.

use std::sync::Arc;

use chrono::{DateTime, Utc};

use crate::SessionRuntime;
use crate::agent::{Agent, AgentInteractionError, RunOutcome};
use crate::core::{CancelToken, ModelInfo};
use crate::generative_model::{Content, GenerateError, GenerativeModel};
use crate::session::{CompactOutcome, SessionWriter};

use super::session_turn::{Submission, finish_turn, run_turn, settle_stopped_run};
use super::{SessionTurnOutcome, wire_checkpoint};

#[path = "compaction.rs"]
mod compaction;
use compaction::CompactionMode;
pub use compaction::{Compactor, ModelCompactor};

#[derive(Debug, Clone)]
pub enum WorkflowEvent {
    Retrying {
        error: String,
        delay: std::time::Duration,
    },
    Compacting {
        session_id: String,
        thread_id: String,
        automatic: bool,
    },
    Compacted(CompactOutcome),
    CompactionProgress {
        elapsed: std::time::Duration,
    },
    Warning(String),
}

pub struct SessionRunner {
    agent: Agent,
    runtime: Arc<SessionRuntime>,
    workflow: Workflow,
}

impl SessionRunner {
    pub async fn new(
        mut agent: Agent,
        runtime: Arc<SessionRuntime>,
    ) -> Result<Self, AgentInteractionError> {
        let session = runtime.session().clone();
        let _writer = session.writer().await;
        runtime.bind_agent(&mut agent)?;
        wire_checkpoint(&mut agent, runtime.session());
        Ok(Self {
            agent,
            runtime,
            workflow: Workflow {
                resume_notice: true,
                ..Default::default()
            },
        })
    }

    pub fn agent(&self) -> &Agent {
        &self.agent
    }
    pub fn agent_mut(&mut self) -> &mut Agent {
        &mut self.agent
    }
    pub fn runtime(&self) -> &Arc<SessionRuntime> {
        &self.runtime
    }
    pub fn set_compactor(&mut self, compactor: Arc<dyn Compactor>, auto_compact_at: Option<u64>) {
        self.workflow.compactor = Some(compactor);
        self.workflow.threshold = auto_compact_at.map(|tokens| tokens.max(1));
        self.workflow.auto_ineffective = false;
    }

    pub fn set_observer(&mut self, observer: Arc<dyn Fn(WorkflowEvent) + Send + Sync>) {
        self.workflow.observer = observer;
    }

    pub fn set_followup_handler(&mut self, handler: Option<super::FollowupHandler>) {
        self.workflow.followups = handler;
    }

    /// Change the model at an idle boundary and persist its identity before any
    /// more model work. An empty session defers the notice until its first input.
    pub async fn set_model(
        &mut self,
        model: Arc<dyn GenerativeModel>,
        info: ModelInfo,
    ) -> Result<(), AgentInteractionError> {
        let session = self.runtime.session().clone();
        let _writer = session.writer().await;
        settle_stopped_run(&mut self.agent)?;
        // Saved before the runtime notice: a later failure leaves the latest
        // runtime record naming the model that still runs.
        session
            .set_model_key(&info.key)
            .map_err(AgentInteractionError::Checkpoint)?;
        let mut next = self.agent.state().clone();
        if crate::RuntimeRecord::latest(self.agent.history()).is_some_and(|old| old.model != info) {
            next.replace_context(self.agent.history().to_vec(), None)?;
        }
        if !self.agent.history().is_empty() {
            if let Some(notice) = self
                .runtime
                .observe(next.history(), info.clone(), self.workflow.resume_notice)
                .await
            {
                next.append_system(vec![notice])?;
            }
            let thread = self.agent.context().thread_id.as_deref().ok_or_else(|| {
                AgentInteractionError::Checkpoint("agent is not bound to a thread".into())
            })?;
            session
                .persist_agent_state(thread, &next, false, None)
                .map_err(AgentInteractionError::Checkpoint)?;
        }
        self.agent
            .replace_context(next.history().to_vec(), next.last_usage())?;
        self.agent.set_model(model);
        self.workflow.model_info = Some(info);
        self.workflow.resume_notice = false;
        Ok(())
    }

    pub async fn submit(
        &mut self,
        input: Vec<Content>,
        accepted_at: DateTime<Utc>,
        cancel: CancelToken,
    ) -> SessionTurnOutcome {
        let submission = Submission {
            input,
            accepted_at: Some(accepted_at),
        };
        let observer = self.workflow.observer.clone();
        run_turn(
            &mut self.agent,
            &self.runtime,
            submission,
            &mut self.workflow,
            cancel,
            move |warning| observer(WorkflowEvent::Warning(warning.into())),
        )
        .await
    }

    /// Continue a saved session without fabricating a human submission.
    pub async fn resume(&mut self, cancel: CancelToken) -> SessionTurnOutcome {
        self.workflow.resume_notice = true;
        let submission = Submission { input: vec![Content::System {
            kind: "continuation".into(),
            text: "Resume the work described in the saved context. Check the latest observations and runtime notices before continuing.".into(),
            data: serde_json::json!({"reason":"resume"}),
        }], accepted_at: None };
        let observer = self.workflow.observer.clone();
        run_turn(
            &mut self.agent,
            &self.runtime,
            submission,
            &mut self.workflow,
            cancel,
            move |warning| observer(WorkflowEvent::Warning(warning.into())),
        )
        .await
    }

    /// Resume preserved live state after a failed checkpoint. No human input is
    /// invented, and no completed effect is repeated. A dropped tool future
    /// must first be reconciled with `agent_mut().recover_interrupted()`.
    pub async fn continue_run(&mut self, cancel: CancelToken) -> SessionTurnOutcome {
        let session = self.runtime.session().clone();
        let writer = tokio::select! {
            biased;
            _ = cancel.cancelled() => return SessionTurnOutcome { result: Err(AgentInteractionError::Cancelled), rewound: None },
            writer = session.writer() => writer,
        };
        let start = self.agent.state().effect().is_none();
        let result = self
            .workflow
            .drive(
                &mut self.agent,
                &self.runtime,
                &writer,
                cancel.clone(),
                start,
            )
            .await;
        let observer = self.workflow.observer.clone();
        finish_turn(
            &mut self.agent,
            &self.runtime,
            &writer,
            &self.workflow,
            &cancel,
            result,
            move |warning| observer(WorkflowEvent::Warning(warning.into())),
        )
        .await
    }

    /// Manual compaction ends at the new context; only automatic compaction continues work.
    pub async fn compact(
        &mut self,
        cancel: CancelToken,
    ) -> Result<CompactOutcome, AgentInteractionError> {
        let session = self.runtime.session().clone();
        let writer = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(AgentInteractionError::Cancelled),
            writer = session.writer() => writer,
        };
        settle_stopped_run(&mut self.agent)?;
        let outcome = self
            .workflow
            .compact(
                &mut self.agent,
                &self.runtime,
                &writer,
                cancel,
                CompactionMode::Manual,
            )
            .await?;
        self.workflow.auto_ineffective = false;
        Ok(outcome)
    }
}

pub(super) struct Workflow {
    model_info: Option<ModelInfo>,
    resume_notice: bool,
    compactor: Option<Arc<dyn Compactor>>,
    threshold: Option<u64>,
    auto_ineffective: bool,
    compacted_after_completion: bool,
    awaiting_compacted_usage: bool,
    skip_preflight: bool,
    recovering_size_error: bool,
    observer: Arc<dyn Fn(WorkflowEvent) + Send + Sync>,
    followups: Option<super::FollowupHandler>,
}

impl Default for Workflow {
    fn default() -> Self {
        Self {
            model_info: None,
            resume_notice: false,
            compactor: None,
            threshold: None,
            auto_ineffective: false,
            compacted_after_completion: false,
            awaiting_compacted_usage: false,
            skip_preflight: false,
            recovering_size_error: false,
            observer: Arc::new(|_| {}),
            followups: None,
        }
    }
}

impl Workflow {
    /// Retry a synchronous operation that retains its own uncommitted state.
    pub(super) async fn retry<T>(
        &self,
        runtime: &SessionRuntime,
        cancel: &CancelToken,
        mut operation: impl FnMut() -> Result<T, AgentInteractionError>,
    ) -> Result<T, AgentInteractionError> {
        let mut retry = super::autonomy::Retry::default();
        loop {
            match operation() {
                Ok(value) => return Ok(value),
                Err(error) => {
                    retry
                        .wait(error, runtime.session(), cancel, &*self.observer)
                        .await?
                }
            }
        }
    }

    /// Input/system updates mutate the live agent before checkpointing. Once
    /// applied, only retry the save; never append that input a second time.
    pub(super) async fn checkpointed_update(
        &self,
        agent: &mut Agent,
        runtime: &SessionRuntime,
        cancel: &CancelToken,
        mut update: impl FnMut(&mut Agent) -> Result<(), AgentInteractionError>,
    ) -> Result<(), AgentInteractionError> {
        let mut applied = false;
        self.retry(runtime, cancel, || {
            if applied {
                return agent.checkpoint();
            }
            let result = update(agent);
            applied = result.is_ok() || matches!(result, Err(AgentInteractionError::Checkpoint(_)));
            result
        })
        .await
    }

    pub(super) async fn runtime_notice(
        &mut self,
        agent: &Agent,
        runtime: &SessionRuntime,
    ) -> Option<Content> {
        let model = self.model_info.clone().unwrap_or_else(|| {
            ModelInfo::named(runtime.session().with(|session| session.model.clone()))
        });
        let notice = runtime
            .observe(agent.history(), model, self.resume_notice)
            .await;
        self.resume_notice = false;
        notice
    }

    async fn record_runtime(
        &mut self,
        agent: &mut Agent,
        runtime: &SessionRuntime,
    ) -> Result<(), AgentInteractionError> {
        if !agent.history().is_empty()
            && (agent.state().is_idle() || agent.state().can_replace_at_boundary())
            && let Some(notice) = self.runtime_notice(agent, runtime).await
        {
            agent.append_system(vec![notice])?;
        }
        Ok(())
    }

    pub(super) async fn drive(
        &mut self,
        agent: &mut Agent,
        runtime: &Arc<SessionRuntime>,
        writer: &SessionWriter,
        cancel: CancelToken,
        start: bool,
    ) -> Result<RunOutcome, AgentInteractionError> {
        if start {
            self.recovering_size_error = false;
            self.skip_preflight = false;
            self.compacted_after_completion = false;
            self.awaiting_compacted_usage = false;
            self.checkpointed_update(agent, runtime, &cancel, |agent| {
                super::autonomy::announce(agent, runtime.session())
            })
            .await?;
        }
        let mut start = start;
        let mut retry = super::autonomy::Retry::default();
        loop {
            match self
                .drive_until_error(agent, runtime, writer, cancel.clone(), start)
                .await
            {
                Ok(outcome) => return Ok(outcome),
                Err(error) => {
                    retry
                        .wait(error, runtime.session(), &cancel, &*self.observer)
                        .await?
                }
            }
            start = agent.state().effect().is_none();
        }
    }

    async fn drive_until_error(
        &mut self,
        agent: &mut Agent,
        runtime: &Arc<SessionRuntime>,
        writer: &SessionWriter,
        cancel: CancelToken,
        start: bool,
    ) -> Result<RunOutcome, AgentInteractionError> {
        agent.repair_checkpoint()?;
        self.record_runtime(agent, runtime).await?;
        if start {
            agent.start_run()?;
        }
        loop {
            self.deliver_followups(agent, runtime, &cancel)?;
            if matches!(
                agent.state().pending_operation(),
                Some(crate::agent::PendingOperation::Generation { .. })
            ) && !cancel.is_cancelled()
            {
                if let Err(error) = agent.prepare_generation(cancel.clone()).await {
                    if matches!(error, AgentInteractionError::Cancelled) {
                        agent.cancel_at_boundary()?;
                    }
                    return Err(error);
                }
                let reached = self.threshold_reached(Some(agent.context_tokens_estimate()));
                if std::mem::take(&mut self.skip_preflight) {
                    if reached {
                        self.auto_ineffective = true;
                        (self.observer)(WorkflowEvent::Warning("compaction did not reduce the estimated text prompt below its threshold; automatic compaction is suspended pending the next provider usage report. Manual compaction or a session change also resets it. Token estimates are approximate; request-size recovery remains available.".into()));
                    }
                } else if reached {
                    self.auto_compact(agent, runtime, writer, cancel.clone())
                        .await?;
                    continue;
                }
            }
            let result = if runtime.session().with(|session| session.auto_continue) {
                agent.step_once(cancel.clone()).await
            } else {
                agent.step(cancel.clone()).await
            };
            if let Err(error @ AgentInteractionError::Checkpoint(_)) = result {
                return Err(error);
            }
            self.record_runtime(agent, runtime).await?;
            let outcome = match result {
                Err(
                    error @ AgentInteractionError::GenerateError(
                        GenerateError::RequestTooLargeError(_),
                    ),
                ) => {
                    if cancel.is_cancelled() {
                        agent.cancel_at_boundary()?;
                        return Err(AgentInteractionError::Cancelled);
                    }
                    // A rejected compacted request must not loop through more
                    // summaries. Allow recovery again only after model progress.
                    if self.recovering_size_error || self.compactor.is_none() {
                        return Err(error);
                    }
                    self.recovering_size_error = true;
                    match self
                        .compact(
                            agent,
                            runtime,
                            writer,
                            cancel.clone(),
                            CompactionMode::RequestSize,
                        )
                        .await
                    {
                        Ok(_) => continue,
                        Err(AgentInteractionError::Compaction(reason)) => {
                            (self.observer)(WorkflowEvent::Warning(format!(
                                "request-size recovery failed: {reason}"
                            )));
                            return Err(error);
                        }
                        Err(AgentInteractionError::Cancelled) => {
                            agent.cancel_at_boundary()?;
                            return Err(AgentInteractionError::Cancelled);
                        }
                        Err(error) => return Err(error),
                    }
                }
                result => result?,
            };
            self.recovering_size_error = false;
            let used = agent.last_usage().map(|usage| usage.context_tokens());
            if self.awaiting_compacted_usage && used.is_some() {
                self.awaiting_compacted_usage = false;
                // A conservative successor estimate can suspend preflight
                // until this measurement establishes the actual input size.
                self.auto_ineffective = self
                    .threshold
                    .zip(used)
                    .is_some_and(|(threshold, used)| used >= threshold);
                if self.auto_ineffective {
                    (self.observer)(WorkflowEvent::Warning("compaction did not reduce the prompt below its threshold; automatic compaction disabled until manual compaction or a session change".into()));
                }
            }
            let should_compact = !(cancel.is_cancelled()
                || !agent.state().can_replace_at_boundary()
                || outcome.is_some() && self.compacted_after_completion)
                && outcome.as_ref().is_none_or(|outcome| {
                    outcome.reason == crate::generative_model::TurnEndReason::EndTurn
                })
                && self.threshold_reached(used);
            if should_compact {
                self.compacted_after_completion |= outcome.is_some();
                self.auto_compact(agent, runtime, writer, cancel.clone())
                    .await?;
                continue;
            }
            if let Some(outcome) = outcome {
                if self.deliver_followups(agent, runtime, &cancel)? {
                    continue;
                }
                if super::autonomy::continue_after(agent, runtime.session(), &outcome, &cancel)? {
                    self.compacted_after_completion = false;
                    continue;
                }
                return Ok(outcome);
            }
        }
    }

    fn threshold_reached(&self, used: Option<u64>) -> bool {
        !self.auto_ineffective
            && self
                .threshold
                .zip(used)
                .is_some_and(|(limit, used)| used >= limit)
    }

    fn deliver_followups(
        &self,
        agent: &mut Agent,
        runtime: &SessionRuntime,
        cancel: &CancelToken,
    ) -> Result<bool, AgentInteractionError> {
        if cancel.is_cancelled() || !agent.state().can_replace_at_boundary() {
            return Ok(false);
        }
        self.followups
            .as_ref()
            .map_or(Ok(false), |handler| handler(agent, runtime.session()))
    }
}

#[cfg(test)]
#[path = "compaction_retry_tests.rs"]
mod compaction_retry_tests;

#[cfg(test)]
#[path = "runner_tests.rs"]
mod tests;
