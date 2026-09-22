//! Session workflows shared by interactive and scripted callers. Compaction
//! replaces context only at settled boundaries; live tools keep their owner.

use std::sync::Arc;

use chrono::{DateTime, Utc};

use crate::SessionRuntime;
use crate::agent::{Agent, AgentInteractionError, RunOutcome, StateError};
use crate::core::{Async, CancelToken, ModelInfo};
use crate::generative_model::{CatalogModel, Content, GenerativeModel, Message};
use crate::prompts;
use crate::session::{CompactOutcome, Session, SessionWriter, Thread};

use super::session_turn::{Submission, finish_turn, run_turn};
use super::{
    CompactWorkerError, SessionTurnOutcome, persist_session, run_compact_worker, wire_checkpoint,
};

pub trait Compactor: Send + Sync {
    fn compact(
        self: Arc<Self>,
        predecessor: Session,
        cancel: CancelToken,
    ) -> Async<Result<(Thread, CompactOutcome), CompactWorkerError>>;
}

pub struct ModelCompactor {
    pub model: CatalogModel,
    pub max_requests: usize,
}

impl Compactor for ModelCompactor {
    fn compact(
        self: Arc<Self>,
        predecessor: Session,
        cancel: CancelToken,
    ) -> Async<Result<(Thread, CompactOutcome), CompactWorkerError>> {
        Box::pin(async move {
            run_compact_worker(&predecessor, &self.model, self.max_requests, cancel).await
        })
    }
}

#[derive(Debug, Clone)]
pub enum WorkflowEvent {
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
    forked: bool,
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
            forked: false,
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
    pub fn set_forked(&mut self, forked: bool) {
        self.forked = forked;
    }

    pub fn set_compactor(&mut self, compactor: Arc<dyn Compactor>, auto_compact_at: Option<u64>) {
        self.workflow.compactor = Some(compactor);
        self.workflow.threshold = auto_compact_at.map(|tokens| tokens.max(1));
        self.workflow.auto_failed = false;
    }

    pub fn set_observer(&mut self, observer: Arc<dyn Fn(WorkflowEvent) + Send + Sync>) {
        self.workflow.observer = observer;
    }

    /// Change the model at an idle boundary and persist its identity before any
    /// more model work. An empty session defers the notice until its first input.
    pub async fn set_model(
        &mut self,
        model: Arc<dyn GenerativeModel>,
        info: ModelInfo,
    ) -> Result<(), AgentInteractionError> {
        if !self.agent.state().is_idle() {
            return Err(StateError::Busy.into());
        }
        let session = self.runtime.session().clone();
        let _writer = session.writer().await;
        if self.agent.checkpoint_failed() {
            self.agent.checkpoint()?;
        }
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

    pub async fn bind_runtime(
        &mut self,
        runtime: Arc<SessionRuntime>,
    ) -> Result<(), AgentInteractionError> {
        let session = runtime.session().clone();
        let _writer = session.writer().await;
        runtime.bind_agent(&mut self.agent)?;
        wire_checkpoint(&mut self.agent, runtime.session());
        self.runtime = runtime;
        self.workflow.auto_failed = false;
        self.workflow.resume_notice = true;
        self.forked = false;
        self.workflow
            .record_runtime(&mut self.agent, &self.runtime)
            .await?;
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
            forked: self.forked,
            accepted_at: Some(accepted_at),
        };
        let observer = self.workflow.observer.clone();
        let outcome = run_turn(
            &mut self.agent,
            &self.runtime,
            submission,
            &mut self.workflow,
            cancel,
            move |warning| observer(WorkflowEvent::Warning(warning.into())),
        )
        .await;
        if self.forked {
            self.forked = !self.agent.history().iter().any(|message| matches!(message,
                Message::UserMessage { content } if content.iter().any(|part| matches!(part,
                    Content::System { kind, data, .. } if kind == "session" && data["session_id"].as_str() == Some(self.runtime.session_id())))));
        }
        outcome
    }

    /// Continue a saved session without fabricating a human submission.
    pub async fn resume(&mut self, cancel: CancelToken) -> SessionTurnOutcome {
        self.workflow.resume_notice = true;
        let submission = Submission { input: vec![Content::System {
            kind: "continuation".into(),
            text: "Resume the work described in the saved context. Check the latest observations and runtime notices before continuing.".into(),
            data: serde_json::json!({"reason":"resume"}),
        }], forked: false, accepted_at: None };
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
            .drive(&mut self.agent, &self.runtime, &writer, cancel, start)
            .await;
        let observer = self.workflow.observer.clone();
        finish_turn(
            &mut self.agent,
            &self.runtime,
            &writer,
            result,
            move |warning| observer(WorkflowEvent::Warning(warning.into())),
        )
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
        let outcome = self
            .workflow
            .compact(&mut self.agent, &self.runtime, &writer, cancel, false)
            .await?;
        self.workflow.auto_failed = false;
        Ok(outcome)
    }
}

pub(super) struct Workflow {
    model_info: Option<ModelInfo>,
    resume_notice: bool,
    compactor: Option<Arc<dyn Compactor>>,
    threshold: Option<u64>,
    auto_failed: bool,
    compacted_after_completion: bool,
    awaiting_compacted_usage: bool,
    observer: Arc<dyn Fn(WorkflowEvent) + Send + Sync>,
}

impl Default for Workflow {
    fn default() -> Self {
        Self {
            model_info: None,
            resume_notice: false,
            compactor: None,
            threshold: None,
            auto_failed: false,
            compacted_after_completion: false,
            awaiting_compacted_usage: false,
            observer: Arc::new(|_| {}),
        }
    }
}

impl Workflow {
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
        self.record_runtime(agent, runtime).await?;
        if start {
            agent.start_run()?;
            self.compacted_after_completion = false;
            self.awaiting_compacted_usage = false;
        }
        loop {
            let result = agent.step(cancel.clone()).await;
            if let Err(error @ AgentInteractionError::Checkpoint(_)) = result {
                return Err(error);
            }
            self.record_runtime(agent, runtime).await?;
            let outcome = result?;
            let used = agent.last_usage().map(|usage| usage.context_tokens());
            if self.awaiting_compacted_usage && used.is_some() {
                self.awaiting_compacted_usage = false;
                if self
                    .threshold
                    .zip(used)
                    .is_some_and(|(threshold, used)| used >= threshold)
                {
                    self.auto_failed = true;
                    (self.observer)(WorkflowEvent::Warning("compaction did not reduce the prompt below its threshold; automatic compaction disabled until manual compaction or a session change".into()));
                }
            }
            let should_compact = !(self.auto_failed
                || cancel.is_cancelled()
                || !agent.state().can_replace_at_boundary()
                || outcome.is_some() && self.compacted_after_completion)
                && outcome.as_ref().is_none_or(|outcome| {
                    outcome.reason == crate::generative_model::TurnEndReason::EndTurn
                })
                && self
                    .threshold
                    .zip(used)
                    .is_some_and(|(threshold, used)| used >= threshold);
            if should_compact {
                self.compacted_after_completion |= outcome.is_some();
                match self
                    .compact(agent, runtime, writer, cancel.clone(), true)
                    .await
                {
                    Ok(_) => {
                        continue;
                    }
                    Err(AgentInteractionError::Compaction(reason)) => {
                        self.auto_failed = true;
                        (self.observer)(WorkflowEvent::Warning(format!(
                            "auto-compaction failed: {reason}; disabled until manual compaction or a session change"
                        )));
                    }
                    Err(AgentInteractionError::Cancelled) => {
                        agent.cancel_at_boundary()?;
                        return Err(AgentInteractionError::Cancelled);
                    }
                    Err(error) => return Err(error),
                }
            }
            if let Some(outcome) = outcome {
                return Ok(outcome);
            }
        }
    }

    async fn compact(
        &mut self,
        agent: &mut Agent,
        runtime: &Arc<SessionRuntime>,
        writer: &SessionWriter,
        cancel: CancelToken,
        automatic: bool,
    ) -> Result<CompactOutcome, AgentInteractionError> {
        if (automatic && !agent.state().can_replace_at_boundary())
            || (!automatic && !agent.state().is_idle())
        {
            return Err(StateError::Busy.into());
        }
        let compactor = self
            .compactor
            .clone()
            .ok_or_else(|| AgentInteractionError::Compaction("no compactor configured".into()))?;
        self.record_runtime(agent, runtime).await?;
        persist_session(agent, runtime.session(), true)
            .map_err(AgentInteractionError::Checkpoint)?;
        let predecessor = runtime.session().snapshot();
        (self.observer)(WorkflowEvent::Compacting {
            session_id: predecessor.id.clone(),
            thread_id: predecessor.active_thread().id.clone(),
            automatic,
        });
        let started = std::time::Instant::now();
        let mut work = compactor.compact(predecessor, cancel.clone());
        let mut progress = tokio::time::interval(std::time::Duration::from_secs(10));
        progress.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        progress.tick().await;
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
        let (mut successor, outcome) = result.map_err(|error| match error {
            CompactWorkerError::Cancelled => AgentInteractionError::Cancelled,
            CompactWorkerError::Failed(message) => AgentInteractionError::Compaction(message),
        })?;
        if automatic {
            successor.messages.push(Message::UserMessage {
                content: vec![Content::System {
                    kind: "continuation".into(),
                    text: prompts::COMPACTION_RESUMPTION.into(),
                    data: serde_json::json!({"reason":"auto_compaction"}),
                }],
            });
        }
        writer
            .commit_thread(successor)
            .map_err(AgentInteractionError::Checkpoint)?;
        runtime.install_compacted_context(agent, automatic)?;
        self.awaiting_compacted_usage = automatic;
        wire_checkpoint(agent, runtime.session());
        persist_session(agent, runtime.session(), true)
            .map_err(AgentInteractionError::Checkpoint)?;
        (self.observer)(WorkflowEvent::Compacted(outcome.clone()));
        Ok(outcome)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    };

    use super::*;
    use crate::agent::NullEventSink;
    use crate::generative_model::{GenerateOutput, TokenUsage, ToolUse, TurnEndReason};
    use crate::harness::Harness;
    use crate::session::{ActiveSession, compact_thread};
    use crate::test_support::{ScriptedModel, temp_home};
    use serde_json::json;

    #[derive(Default)]
    struct Summarizer {
        calls: AtomicUsize,
        sources: Mutex<Vec<Session>>,
        cancel: bool,
        break_store: Option<std::path::PathBuf>,
    }

    impl Compactor for Summarizer {
        fn compact(
            self: Arc<Self>,
            predecessor: Session,
            cancel: CancelToken,
        ) -> Async<Result<(Thread, CompactOutcome), CompactWorkerError>> {
            Box::pin(async move {
                self.calls.fetch_add(1, Ordering::SeqCst);
                crate::agent::validate_context(&predecessor.active_thread().messages).unwrap();
                self.sources.lock().unwrap().push(predecessor.clone());
                let compacted =
                    compact_thread(&predecessor, "Continue the task with the existing shell")
                        .unwrap();
                if self.cancel {
                    cancel.cancel();
                }
                if let Some(home) = &self.break_store {
                    std::fs::rename(home.join("session"), home.join("saved-store")).unwrap();
                    std::fs::write(home.join("session"), "disk unavailable").unwrap();
                }
                Ok(compacted)
            })
        }
    }

    fn output(
        used: u64,
        input: Option<serde_json::Value>,
        reason: TurnEndReason,
    ) -> GenerateOutput {
        GenerateOutput {
            content: vec![Content::Text {
                text: "progress".into(),
            }],
            tool_uses: input
                .into_iter()
                .map(|input| ToolUse {
                    name: "bash".into(),
                    input,
                })
                .collect(),
            turn_end_reason: reason,
            usage: Some(TokenUsage {
                input_tokens: used,
                output_tokens: 7,
                cached_input_tokens: 0,
            }),
        }
    }

    async fn setup(
        scripts: Vec<GenerateOutput>,
        compactor: Arc<Summarizer>,
    ) -> (SessionRunner, Arc<ScriptedModel>) {
        let session = ActiveSession::new(Session::new("scripted"));
        let runtime = SessionRuntime::new(Harness::local_with_services(vec![]), session);
        let model = ScriptedModel::new(scripts);
        let agent = Agent::new(model.clone(), runtime.clone(), Arc::new(NullEventSink));
        let mut runner = SessionRunner::new(agent, runtime).await.unwrap();
        runner.set_compactor(compactor, Some(80));
        (runner, model)
    }

    async fn submit(runner: &mut SessionRunner, cancel: CancelToken) -> SessionTurnOutcome {
        runner
            .submit(
                vec![Content::Text {
                    text: "scripted task".into(),
                }],
                Utc::now(),
                cancel,
            )
            .await
    }

    #[test]
    fn runtime_records_survive_compaction_and_report_model_changes_and_lost_handles_on_restart() {
        let home = temp_home("runner-lifecycle");
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            let file = home.path().join("observed.txt");
            std::fs::write(&file, "read before editing").unwrap();
            let mut start = output(90, Some(json!({"action":"start", "session_id":"live", "command":"bash --noprofile --norc", "idle_ms":10, "timeout_ms":1000})), TurnEndReason::ToolUse);
            start.tool_uses.push(ToolUse { name: "str_replace_based_edit_tool".into(), input: json!({"command":"view", "path":file}) });
            let (mut runner, _) = setup(vec![start, output(20, None, TurnEndReason::EndTurn)], Arc::new(Summarizer::default())).await;
            submit(&mut runner, CancelToken::new()).await.result.unwrap();
            let current = crate::RuntimeRecord::latest(runner.agent().history()).unwrap();
            let handles = current.resources.iter().find(|host| host.host == "local").unwrap().resources.as_ref().unwrap();
            assert_eq!(handles.len(), 2);
            assert!(handles.iter().any(|resource| resource.id == "live" && resource.details["process_exited"] == false));
            assert!(handles.iter().any(|resource| resource.id == file.to_string_lossy()));
            assert!(current.unavailable_after_restart.is_empty());
            let path = runner.runtime().session().snapshot().json_path();
            let saved = Session::load(&path).unwrap();
            assert_eq!(saved.threads().len(), 2);
            assert_eq!(crate::RuntimeRecord::latest(&saved.active_thread().messages).unwrap().runtime_id, current.runtime_id);

            let replacement = ScriptedModel::new(vec![]);
            let mut info = ModelInfo::named("replacement");
            info.effort = Some(crate::generative_model::Effort::Max);
            runner.set_model(replacement, info.clone()).await.unwrap();
            let changed = crate::RuntimeRecord::latest(&Session::load(&path).unwrap().active_thread().messages).unwrap();
            assert_eq!(changed.previous_model.unwrap().key, "scripted");
            assert_eq!(changed.model, info);
            assert_eq!(changed.runtime_id, current.runtime_id);
            assert!(!changed.resumed);
            drop(runner);

            let saved = Session::load(&path).unwrap();
            let times = saved.active_thread().user_turn_timestamps.clone();
            let session = ActiveSession::new(saved);
            let runtime = SessionRuntime::new(Harness::local_with_services(vec![]), session);
            let model = ScriptedModel::new(vec![output(20, None, TurnEndReason::EndTurn)]);
            let agent = Agent::new(model.clone(), runtime.clone(), Arc::new(NullEventSink));
            let mut resumed = SessionRunner::new(agent, runtime).await.unwrap();
            resumed.set_model(model, info).await.unwrap();
            resumed.resume(CancelToken::new()).await.result.unwrap();
            let recovered = crate::RuntimeRecord::latest(resumed.agent().history()).unwrap();
            assert_ne!(recovered.runtime_id, current.runtime_id);
            assert!(recovered.resumed);
            assert!(recovered.resources[0].resources.as_ref().unwrap().is_empty());
            assert_eq!(recovered.unavailable_after_restart[0].resources.as_ref().unwrap(), handles);
            assert_eq!(Session::load(&path).unwrap().active_thread().user_turn_timestamps, times);
        });
    }

    #[test]
    fn repairing_the_input_checkpoint_can_continue_without_resubmitting_the_task() {
        let home = temp_home("runner-input-repair");
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            let (mut runner, model) = setup(
                vec![output(20, None, TurnEndReason::EndTurn)],
                Arc::new(Summarizer::default()),
            )
            .await;
            std::fs::write(home.path().join("session"), "store unavailable").unwrap();
            assert!(matches!(
                submit(&mut runner, CancelToken::new()).await.result,
                Err(AgentInteractionError::Checkpoint(_))
            ));
            assert_eq!(model.remaining(), 1);
            std::fs::remove_file(home.path().join("session")).unwrap();
            runner
                .continue_run(CancelToken::new())
                .await
                .result
                .unwrap();
            let saved = Session::load(&runner.runtime().session().snapshot().json_path()).unwrap();
            assert_eq!(
                saved
                    .active_thread()
                    .messages
                    .iter()
                    .filter(|message| message.is_user_turn())
                    .count(),
                1
            );
            assert_eq!(saved.active_thread().user_turn_timestamps.len(), 1);
            assert_eq!(model.remaining(), 0);
        });
    }

    #[test]
    fn failed_model_change_keeps_the_original_model_history_and_usage() {
        let home = temp_home("model-change-save");
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            let (mut runner, old) = setup(
                vec![output(20, None, TurnEndReason::EndTurn); 2],
                Arc::new(Summarizer::default()),
            )
            .await;
            submit(&mut runner, CancelToken::new())
                .await
                .result
                .unwrap();
            let history = runner.agent().history().to_vec();
            let usage = runner.agent().last_usage();
            let replacement = ScriptedModel::new(vec![output(30, None, TurnEndReason::EndTurn)]);
            std::fs::rename(home.path().join("session"), home.path().join("saved")).unwrap();
            std::fs::write(home.path().join("session"), "store unavailable").unwrap();
            assert!(
                runner
                    .set_model(replacement.clone(), ModelInfo::named("replacement"))
                    .await
                    .is_err()
            );
            assert_eq!(runner.agent().history(), history);
            assert_eq!(runner.agent().last_usage(), usage);
            std::fs::remove_file(home.path().join("session")).unwrap();
            std::fs::rename(home.path().join("saved"), home.path().join("session")).unwrap();
            submit(&mut runner, CancelToken::new())
                .await
                .result
                .unwrap();
            assert_eq!(old.remaining(), 0);
            assert_eq!(replacement.remaining(), 1);
        });
    }

    #[test]
    fn queued_agents_refresh_the_same_thread_and_stale_live_retries_cannot_overwrite_it() {
        let home = temp_home("runner-stale-writer");
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            let (mut first, first_model) = setup(
                vec![output(20, None, TurnEndReason::EndTurn)],
                Arc::new(Summarizer::default()),
            )
            .await;
            let runtime = first.runtime().clone();
            let model = ScriptedModel::new(vec![output(20, None, TurnEndReason::EndTurn)]);
            let agent = Agent::new(model, runtime.clone(), Arc::new(NullEventSink));
            let mut second = SessionRunner::new(agent, runtime.clone()).await.unwrap();
            std::fs::write(home.path().join("session"), "store unavailable").unwrap();
            assert!(matches!(
                submit(&mut first, CancelToken::new()).await.result,
                Err(AgentInteractionError::Checkpoint(_))
            ));
            std::fs::remove_file(home.path().join("session")).unwrap();
            second
                .submit(
                    vec![Content::Text {
                        text: "newer work".into(),
                    }],
                    Utc::now(),
                    CancelToken::new(),
                )
                .await
                .result
                .unwrap();
            let saved = serde_json::to_value(runtime.session().snapshot()).unwrap();
            assert!(matches!(
                first.continue_run(CancelToken::new()).await.result,
                Err(AgentInteractionError::Checkpoint(_))
            ));
            assert_eq!(first_model.remaining(), 1);
            assert_eq!(
                serde_json::to_value(runtime.session().snapshot()).unwrap(),
                saved
            );

            let agent = Agent::new(
                ScriptedModel::new(vec![output(20, None, TurnEndReason::EndTurn)]),
                runtime.clone(),
                Arc::new(NullEventSink),
            );
            let mut third = SessionRunner::new(agent, runtime.clone()).await.unwrap();
            let agent = Agent::new(
                ScriptedModel::new(vec![output(20, None, TurnEndReason::EndTurn)]),
                runtime.clone(),
                Arc::new(NullEventSink),
            );
            let mut fourth = SessionRunner::new(agent, runtime.clone()).await.unwrap();
            third
                .submit(
                    vec![Content::Text {
                        text: "third".into(),
                    }],
                    Utc::now(),
                    CancelToken::new(),
                )
                .await
                .result
                .unwrap();
            fourth
                .submit(
                    vec![Content::Text {
                        text: "fourth".into(),
                    }],
                    Utc::now(),
                    CancelToken::new(),
                )
                .await
                .result
                .unwrap();
            let saved = Session::load(&runtime.session().snapshot().json_path()).unwrap();
            let human: Vec<_> = saved
                .active_thread()
                .messages
                .iter()
                .filter(|message| message.is_user_turn())
                .collect();
            assert_eq!(human.len(), 3);
            let text = serde_json::to_string(&human).unwrap();
            assert!(
                text.contains("newer work") && text.contains("third") && text.contains("fourth")
            );
        });
    }

    #[test]
    fn ineffective_compaction_stops_recompacting_and_manual_compaction_does_not_continue() {
        let _home = temp_home("runner-ineffective");
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            let compactor = Arc::new(Summarizer::default());
            let (mut runner, model) = setup(
                vec![
                    output(90, None, TurnEndReason::EndTurn),
                    output(90, None, TurnEndReason::EndTurn),
                ],
                compactor.clone(),
            )
            .await;
            let warnings = Arc::new(Mutex::new(Vec::new()));
            let observer = warnings.clone();
            runner.set_observer(Arc::new(move |event| {
                if let WorkflowEvent::Warning(message) = event {
                    observer.lock().unwrap().push(message);
                }
            }));
            submit(&mut runner, CancelToken::new())
                .await
                .result
                .unwrap();
            assert_eq!(model.remaining(), 0);
            assert_eq!(compactor.calls.load(Ordering::SeqCst), 1);
            assert!(warnings.lock().unwrap()[0].contains("did not reduce"));
            runner.compact(CancelToken::new()).await.unwrap();
            assert_eq!(compactor.calls.load(Ordering::SeqCst), 2);
            assert_eq!(model.remaining(), 0);
        });
    }

    #[test]
    fn one_scripted_run_compacts_repeatedly_between_tool_rounds_and_keeps_its_shell() {
        let _home = temp_home("runner-compaction");
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            let compactor = Arc::new(Summarizer::default());
            let (mut runner, model) = setup(vec![
                output(90, Some(json!({"action":"start", "session_id":"persistent", "command":"marker=42; export marker; exec bash --noprofile --norc", "idle_ms":10, "timeout_ms":1000})), TurnEndReason::ToolUse),
                output(20, Some(json!({"action":"write", "session_id":"persistent", "stdin":"echo marker-$marker\n", "idle_ms":20, "timeout_ms":1000})), TurnEndReason::ToolUse),
                output(90, Some(json!({"action":"write", "session_id":"persistent", "stdin":"echo next-$marker\n", "idle_ms":20, "timeout_ms":1000})), TurnEndReason::ToolUse),
                output(20, None, TurnEndReason::EndTurn),
            ], compactor.clone()).await;
            let outcome = submit(&mut runner, CancelToken::new()).await.result.unwrap();
            assert_eq!(compactor.calls.load(Ordering::SeqCst), 2);
            assert_eq!(model.remaining(), 0);
            assert_eq!(outcome.usage.unwrap().output_tokens, 28);
            assert_eq!(outcome.reason, TurnEndReason::EndTurn);
            assert_eq!(runner.runtime().running_tool_summaries().len(), 1);
            let saved = Session::load(&runner.runtime().session().snapshot().json_path()).unwrap();
            assert_eq!(saved.threads().len(), 3);
            assert_eq!(saved.active_thread().user_turn_timestamps.len(), 1);
            assert!(saved.active_thread().pending_operation.is_none());
            let history = serde_json::to_string(&saved.active_thread().messages).unwrap();
            assert!(history.contains("marker-42"), "{history}");
            assert!(history.contains("next-42"), "{history}");
            let sources = compactor.sources.lock().unwrap();
            assert_eq!(serde_json::to_value(&saved.threads()[0]).unwrap(), serde_json::to_value(sources[0].active_thread()).unwrap());
            assert_eq!(serde_json::to_value(&saved.threads()[1]).unwrap(), serde_json::to_value(sources[1].active_thread()).unwrap());
        });
    }

    #[test]
    fn compaction_preserves_the_run_truncation_cap() {
        let _home = temp_home("runner-truncation");
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            let compactor = Arc::new(Summarizer::default());
            let (mut runner, model) = setup(
                vec![
                    output(90, None, TurnEndReason::MaxTokens),
                    output(20, None, TurnEndReason::MaxTokens),
                    output(20, None, TurnEndReason::EndTurn),
                ],
                compactor.clone(),
            )
            .await;
            runner.agent_mut().set_max_truncated_resumes(1);
            let outcome = submit(&mut runner, CancelToken::new())
                .await
                .result
                .unwrap();
            assert_eq!(outcome.reason, TurnEndReason::MaxTokens);
            assert_eq!(model.remaining(), 1);
            assert_eq!(outcome.usage.unwrap().output_tokens, 14);
            assert_eq!(compactor.calls.load(Ordering::SeqCst), 1);
        });
    }

    #[test]
    fn auto_compaction_does_not_continue_a_refusal_or_exhausted_output_cap() {
        let _home = temp_home("runner-stop-reasons");
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            for reason in [
                TurnEndReason::MaxTokens,
                TurnEndReason::Other("refusal".into()),
                TurnEndReason::Other("unknown".into()),
            ] {
                let compactor = Arc::new(Summarizer::default());
                let (mut runner, model) =
                    setup(vec![output(90, None, reason.clone())], compactor.clone()).await;
                runner.agent_mut().set_max_truncated_resumes(0);
                let outcome = submit(&mut runner, CancelToken::new())
                    .await
                    .result
                    .unwrap();
                assert_eq!(outcome.reason, reason);
                assert_eq!(model.remaining(), 0);
                assert_eq!(compactor.calls.load(Ordering::SeqCst), 0);
            }
        });
    }

    #[test]
    fn cancellation_during_compaction_keeps_the_source_and_accepts_the_next_submission() {
        let _home = temp_home("runner-cancel");
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            let compactor = Arc::new(Summarizer {
                cancel: true,
                ..Default::default()
            });
            let (mut runner, model) = setup(
                vec![
                    output(
                        90,
                        Some(json!({"command":"printf observed"})),
                        TurnEndReason::ToolUse,
                    ),
                    output(20, None, TurnEndReason::EndTurn),
                ],
                compactor,
            )
            .await;
            assert!(matches!(
                submit(&mut runner, CancelToken::new()).await.result,
                Err(AgentInteractionError::Cancelled)
            ));
            assert_eq!(model.remaining(), 1);
            assert_eq!(runner.runtime().session().snapshot().threads().len(), 1);
            assert!(runner.agent().state().is_idle());
            submit(&mut runner, CancelToken::new())
                .await
                .result
                .unwrap();
        });
    }

    #[test]
    fn a_failed_compaction_commit_can_resume_live_work_without_repeating_a_tool() {
        let home = temp_home("runner-storage-repair");
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            let effect = home.path().join("effect");
            let compactor = Arc::new(Summarizer {
                break_store: Some(home.path().to_owned()),
                ..Default::default()
            });
            let (mut runner, model) = setup(
                vec![
                    output(
                        90,
                        Some(json!({"command":format!("printf x >> '{}'", effect.display())})),
                        TurnEndReason::ToolUse,
                    ),
                    output(20, None, TurnEndReason::EndTurn),
                ],
                compactor,
            )
            .await;
            assert!(matches!(
                submit(&mut runner, CancelToken::new()).await.result,
                Err(AgentInteractionError::Checkpoint(_))
            ));
            assert_eq!(model.remaining(), 1);
            std::fs::remove_file(home.path().join("session")).unwrap();
            std::fs::rename(home.path().join("saved-store"), home.path().join("session")).unwrap();
            let outcome = runner
                .continue_run(CancelToken::new())
                .await
                .result
                .unwrap();
            assert_eq!(outcome.usage.unwrap().output_tokens, 14);
            assert_eq!(std::fs::read_to_string(effect).unwrap(), "x");
            let saved = Session::load(&runner.runtime().session().snapshot().json_path()).unwrap();
            assert_eq!(saved.threads().len(), 1);
            assert!(saved.active_thread().pending_operation.is_none());
        });
    }
}
