//! Drive model context through generation and tool calls at replayable boundaries.
//! Callers supply tool execution, event sinks, context, and persistence callbacks.
//!
//! # Embedding
//!
//! Construct [`Agent`] with a [`GenerativeModel`], [`ToolExecutor`], and [`EventSink`].
//! Supply input with [`Agent::append_input`] or install history with
//! [`Agent::replace_context`], then await [`Agent::run`]. The crate does not read
//! Myco configuration or create sessions, prompts, hosts, or terminal output.
//!
//! # Ownership and persistence
//!
//! The executor owns live resources independently of agent history. Calls within
//! a round run concurrently; results are recorded in call order. Checkpoints
//! persist pending effects and their settled observations. Pending tool batches
//! are durable intent, not replayable model context; recover them before reuse.
//!
//! Cancel a clone of the run's [`CancelToken`] and await the run future to let
//! cleanup finish. Aborting the task bypasses that cooperative completion path.
//! [`EventSink`] provides live observations, not a complete durable event log.
//!
//! See the [agent guide](https://tsnl.github.io/myco/developers/agents.html) and
//! run `cargo run -p myco-agent --example headless` for an offline model/tool round.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use futures::future;
use myco_model::{
    self as generative_model, Content, GenerateError, GenerationFailure, GenerativeModel, Message,
    Recovery, RetryPolicy, TokenUsage, ToolResult, ToolUse, TurnEndReason,
};
use uuid::Uuid;

mod context_size;
mod generation;
mod state;
pub use state::{
    AgentState, Effect, OperationId, PendingOperation, StateError, recover_checkpoint,
    validate_checkpoint, validate_context,
};

pub use tokio_util::sync::CancellationToken as CancelToken;
pub type Async<T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send>>;
pub const DEFAULT_MAX_TRUNCATED_RESUMES: u32 = 3;

/// Capabilities available to this execution. Ownership and routing belong to the caller.
pub trait ToolExecutor: Send + Sync {
    /// Schemas to pass into the model's configuration. Changing the executor
    /// does not update schemas on a model that was already constructed.
    fn tool_specs(&self) -> Vec<generative_model::ToolSpec>;
    /// Execute a call, validating its input and returning failures as tool results.
    /// Calls in a round may overlap. Observe cancellation and clean up owned work.
    /// `background` requests an early result while preserving ongoing work,
    /// when the tool supports it. It must never be treated as cancellation.
    fn dispatch(
        self: Arc<Self>,
        tool: ToolUse,
        cancel: CancelToken,
        background: CancelToken,
    ) -> Async<ToolResult>;
}

//
// Event sink — live observability for agent / tool activity
//

/// Event attribution for frontends and evaluators. Session and thread are
/// present when bound; depth lets displays distinguish nested workers.
#[derive(Debug, Clone)]
pub struct TraceContext {
    /// Stable id for this agent instance (root or subagent).
    pub agent_id: Uuid,
    /// Nesting depth: root agent is 0; each nested agent is parent depth + 1.
    pub depth: usize,
    pub session_id: Option<String>,
    pub thread_id: Option<String>,
}

impl Default for TraceContext {
    fn default() -> Self {
        Self {
            agent_id: Uuid::nil(),
            depth: 0,
            session_id: None,
            thread_id: None,
        }
    }
}

impl TraceContext {
    pub fn root() -> Self {
        Self {
            agent_id: Uuid::new_v4(),
            depth: 0,
            session_id: None,
            thread_id: None,
        }
    }
}

/// Live events emitted by the agent runtime.
///
/// All ongoing work is attributed via [`TraceContext::agent_id`].
#[derive(Debug, Clone)]
pub enum AgentEvent {
    /// Begin a provisional response. A retry replaces the failed attempt's draft.
    GenerationStarted {
        context: TraceContext,
    },
    /// The complete response validated. Its tool calls have not executed yet.
    GenerationFinished {
        context: TraceContext,
    },
    /// A validated response was installed in history and its configured
    /// checkpoint succeeded. Its tool calls have not executed yet. Emitted
    /// only when a checkpoint callback is installed, including when a later
    /// checkpoint repairs an earlier save failure.
    GenerationCommitted {
        context: TraceContext,
    },
    Failure {
        failure: GenerationFailure,
        attempt: u32,
        max_attempts: u32,
        retry_in: Option<std::time::Duration>,
        /// Remaining elapsed recovery budget, if configured and recovery started.
        recovery_remaining: Option<std::time::Duration>,
        context: TraceContext,
    },
    /// Incremental assistant text (for streaming UX).
    TextDelta {
        text: String,
        context: TraceContext,
    },
    /// Incremental thinking *summary* text (streamed for UI; also stored in history).
    ThinkingDelta {
        text: String,
        context: TraceContext,
    },
    ToolStarted {
        call_id: Uuid,
        tool_use: ToolUse,
        background: CancelToken,
        context: TraceContext,
    },
    ToolFinished {
        call_id: Uuid,
        tool_use: ToolUse,
        result: ToolResult,
        context: TraceContext,
    },
    TurnFinished {
        context: TraceContext,
    },
}

/// Consumer of [`AgentEvent`]s (CLI, TUI, metrics, …).
pub trait EventSink: Send + Sync {
    /// Observe an event synchronously. Avoid blocking the execution task.
    /// `GenerationFinished` validates a draft; `GenerationCommitted` also confirms
    /// its history checkpoint. On a retrying failure, discard or clearly separate
    /// that draft before displaying the next attempt.
    fn emit(&self, event: AgentEvent);
}

/// No-op sink for tests and headless runs.
#[derive(Debug, Default)]
pub struct NullEventSink;

impl EventSink for NullEventSink {
    fn emit(&self, _event: AgentEvent) {}
}

//
// Agent
//

/// Persist context and pending intent before effects begin, and completed
/// observations before the next effect. Failure stops execution. A pending tool
/// batch is a durable observation, not a valid model input or context fork.
pub type Checkpoint = Box<dyn Fn(&AgentState) -> Result<(), String> + Send + Sync>;

/// Supply a pending runtime notice before a generation step. The returned text
/// is appended as internal system content to the latest user input or tool result
/// and checkpointed before generation; this callback does not reload the system
/// prompt. Retries reuse the same input. Consume the notice only when the future
/// completes, since cancellation can drop the future.
pub type BeforeGenerationNotice =
    Box<dyn Fn(&TraceContext, &[Message]) -> Async<Option<String>> + Send + Sync>;

/// How long a cancelled tool dispatch may keep running to do its own
/// cleanup (process-group kill, buffer drain) before the agent abandons it
/// and records an unknown outcome.
const CANCEL_TOOL_GRACE: std::time::Duration = std::time::Duration::from_secs(2);

/// The user turn sent to resume a reply that `max_tokens` cut off mid-text.
///
/// A plain "Continue" invites the model to acknowledge the instruction or start
/// the thought over; naming the requirement keeps the seam invisible in the
/// finished answer.
pub const CONTINUE_PROMPT: &str = "Continue from exactly where you stopped. Do not repeat anything you have already written, \
     and do not acknowledge this message.";

/// The user turn sent after `max_tokens` cut off a reply that held only thinking.
///
/// Providers receive no thinking from history, so such a turn reaches the next
/// request as nothing: "continue where you stopped" makes the model redo the
/// same reasoning until it hits the same cap. Name the loss and ask for visible
/// progress early so the next truncation keeps something.
pub const LOST_THINKING_PROMPT: &str = "Your previous response reached the output limit while still thinking, \
     before any visible text or tool call, and that thinking is not available to you. Keep thinking brief: \
     write your conclusions so far as visible text or make the next tool call early in this response. \
     Do not acknowledge this message.";

/// Whether `message` is a `max_tokens` stop that produced no answer text and no
/// tool call — its only output was thinking, which providers never receive back.
pub fn lost_thinking(message: &Message) -> bool {
    matches!(message, Message::AssistantMessage {
        content,
        tool_uses,
        turn_end_reason: Some(TurnEndReason::MaxTokens),
    } if tool_uses.is_empty() && generative_model::answer_content(content).is_empty())
}

/// A run ending at an output cap is distinct from a completed model turn.
#[derive(Debug, Clone)]
pub struct RunOutcome {
    pub answer: Vec<Content>,
    pub reason: TurnEndReason,
    pub usage: Option<TokenUsage>,
}

/// A headless model/tool loop over caller-supplied context and capabilities.
///
/// Resource lifetime belongs to [`ToolExecutor`]; persistence belongs to the
/// caller. The agent enforces no token limit and performs no automatic
/// compaction.
pub struct Agent {
    retry_policy: RetryPolicy,
    model: Arc<dyn GenerativeModel>,
    tools: Arc<dyn ToolExecutor>,
    sink: Arc<dyn EventSink>,
    context: TraceContext,
    state: AgentState,
    checkpoint: Option<Checkpoint>,
    before_generation_notice: Option<BeforeGenerationNotice>,
    prepared_generation: Option<OperationId>,
    notice_pending_checkpoint: Option<OperationId>,
    checkpoint_failed: AtomicBool,
    /// A failed save must not discard the validated response's commit notification.
    pending_generation_commit: Mutex<Option<TraceContext>>,
}

impl Agent {
    pub fn new(
        model: Arc<dyn GenerativeModel>,
        tools: Arc<dyn ToolExecutor>,
        sink: Arc<dyn EventSink>,
    ) -> Self {
        Self::with_context(model, tools, sink, TraceContext::root())
    }

    pub fn with_context(
        model: Arc<dyn GenerativeModel>,
        tools: Arc<dyn ToolExecutor>,
        sink: Arc<dyn EventSink>,
        context: TraceContext,
    ) -> Self {
        Self {
            retry_policy: RetryPolicy::default(),
            model,
            tools,
            sink,
            context,
            state: AgentState::default(),
            checkpoint: None,
            before_generation_notice: None,
            prepared_generation: None,
            notice_pending_checkpoint: None,
            checkpoint_failed: AtomicBool::new(false),
            pending_generation_commit: Mutex::new(None),
        }
    }

    /// Replace dispatch capabilities without rebuilding the model's tool catalog.
    /// Keep both in sync when adding or removing advertised tools.
    pub fn set_tools(&mut self, tools: Arc<dyn ToolExecutor>) {
        self.tools = tools;
    }

    pub fn set_context(&mut self, context: TraceContext) {
        self.context = context;
    }

    /// Install the durable effect checkpoint (see [`Checkpoint`]).
    pub fn set_checkpoint(&mut self, checkpoint: Option<Checkpoint>) {
        if checkpoint.is_none() {
            self.pending_generation_commit.get_mut().unwrap().take();
        }
        self.checkpoint = checkpoint;
    }

    pub fn set_before_generation_notice(&mut self, notice: Option<BeforeGenerationNotice>) {
        self.before_generation_notice = notice;
    }

    fn emit_checkpoint(&self) -> Result<(), AgentInteractionError> {
        let result = self
            .checkpoint
            .as_ref()
            .map_or(Ok(()), |checkpoint| checkpoint(&self.state));
        self.checkpoint_failed
            .store(result.is_err(), Ordering::Relaxed);
        result.map_err(AgentInteractionError::Checkpoint)?;
        if self.checkpoint.is_some() {
            let pending = self.pending_generation_commit.lock().unwrap().take();
            if let Some(context) = pending {
                self.sink.emit(AgentEvent::GenerationCommitted { context });
            }
        }
        Ok(())
    }

    pub fn checkpoint_failed(&self) -> bool {
        self.checkpoint_failed.load(Ordering::Relaxed)
    }

    /// Retry persistence without advancing or replaying an effect.
    pub fn checkpoint(&self) -> Result<(), AgentInteractionError> {
        self.emit_checkpoint()
    }

    /// [`Self::checkpoint`] only if the last save failed, so unsaved
    /// observations persist before the agent is rebound or resumed.
    pub fn repair_checkpoint(&self) -> Result<(), AgentInteractionError> {
        if self.checkpoint_failed() {
            self.emit_checkpoint()?;
        }
        Ok(())
    }

    pub fn history(&self) -> &[Message] {
        self.state.history()
    }

    /// Replace model context and its usage estimate without changing live tool state.
    pub fn replace_context(
        &mut self,
        history: Vec<Message>,
        usage: Option<TokenUsage>,
    ) -> Result<(), StateError> {
        self.state.replace_context(history, usage)?;
        self.pending_generation_commit.get_mut().unwrap().take();
        Ok(())
    }

    pub fn state(&self) -> &AgentState {
        &self.state
    }

    pub fn set_retry_policy(&mut self, retry_policy: RetryPolicy) {
        self.retry_policy = retry_policy;
    }

    /// Swap the generative model (e.g. mid-session `/effort` rebuild). History is kept.
    pub fn set_model(&mut self, model: Arc<dyn GenerativeModel>) {
        self.model = model;
    }

    /// Append input at a well-formed context boundary and checkpoint it.
    pub fn append_input(&mut self, message: Message) -> Result<(), AgentInteractionError> {
        self.state.append_input(message)?;
        self.emit_checkpoint()?;
        Ok(())
    }

    pub fn append_system(&mut self, parts: Vec<Content>) -> Result<(), AgentInteractionError> {
        self.state.append_system(parts)?;
        self.emit_checkpoint()
    }

    pub fn truncate_history(
        &mut self,
        index: usize,
    ) -> Result<Vec<Message>, AgentInteractionError> {
        let dropped = self.state.truncate_history(index)?;
        self.pending_generation_commit.get_mut().unwrap().take();
        self.emit_checkpoint()?;
        Ok(dropped)
    }

    /// Set how many consecutive `max_tokens` truncations one turn resumes
    /// through (the active model's `max_truncated_resumes`; `0` never resumes).
    pub fn set_max_truncated_resumes(&mut self, resumes: u32) {
        self.state.set_max_truncated_resumes(resumes);
    }

    /// Last observed prompt/context token usage (from the provider), if any.
    pub fn last_usage(&self) -> Option<TokenUsage> {
        self.state.last_usage()
    }

    /// Approximate current text/tool prompt size, including growth since the
    /// last measured input. This is not provider-reported usage or a fit guarantee.
    pub fn context_tokens_estimate(&self) -> u64 {
        self.state.context_tokens_estimate()
    }

    /// Preserve an older model's sizing hint when its usage cannot be restored.
    pub fn restore_context_size_hint(&mut self, tokens: Option<u64>) {
        self.state.restore_context_size_hint(tokens);
    }

    pub fn context(&self) -> &TraceContext {
        &self.context
    }

    /// Drive the existing model context to completion. The caller supplies input separately.
    /// The supplied tool executor determines the lifetime of live resources.
    ///
    /// Returns answer content from the final generation; intervening responses
    /// and tool rounds remain in [`Self::history`]. Emits [`AgentEvent::TurnFinished`]
    /// on success, error, or cooperative cancellation. Checkpoints persist both
    /// pending effects and the final settled state.
    pub async fn run(
        &mut self,
        cancel: CancelToken,
    ) -> Result<Vec<Content>, AgentInteractionError> {
        self.run_with_outcome(cancel)
            .await
            .map(|outcome| outcome.answer)
    }

    /// Drive context and retain the stop reason and measured usage for evaluators.
    pub async fn run_with_outcome(
        &mut self,
        cancel: CancelToken,
    ) -> Result<RunOutcome, AgentInteractionError> {
        if let Err(error) = self.start_run() {
            self.finish_output();
            return Err(error);
        }
        self.continue_run(cancel).await
    }

    pub fn start_run(&mut self) -> Result<(), AgentInteractionError> {
        self.state.start()?;
        Ok(())
    }

    /// Continue after a checkpoint failure without repeating completed effects.
    /// A dropped in-flight effect requires explicit `recover_interrupted` first.
    pub async fn continue_run(
        &mut self,
        cancel: CancelToken,
    ) -> Result<RunOutcome, AgentInteractionError> {
        loop {
            if let Some(outcome) = self.step(cancel.clone()).await? {
                return Ok(outcome);
            }
        }
    }

    pub fn replace_at_boundary(
        &mut self,
        history: Vec<Message>,
        usage: Option<TokenUsage>,
    ) -> Result<(), StateError> {
        self.state.replace_at_boundary(history, usage)?;
        self.pending_generation_commit.get_mut().unwrap().take();
        Ok(())
    }

    pub fn recover_interrupted(&mut self) -> Result<(), AgentInteractionError> {
        self.state.recover_interrupted()?;
        self.emit_checkpoint()
    }

    pub fn cancel_at_boundary(&mut self) -> Result<(), AgentInteractionError> {
        self.state.cancel_at_boundary()?;
        self.finish_output();
        self.emit_checkpoint()
    }

    /// Execute one generation or tool batch and checkpoint its outcome. `None`
    /// yields to the caller at the next effect, allowing compaction or inspection.
    pub async fn step(
        &mut self,
        cancel: CancelToken,
    ) -> Result<Option<RunOutcome>, AgentInteractionError> {
        self.step_with_policy(cancel, self.retry_policy).await
    }

    /// Execute one effect with at most one provider attempt. A caller that owns
    /// recovery timing can retry failed generation without nesting backoff loops.
    /// Tool dispatch, cancellation, and checkpoint boundaries match [`Self::step`].
    pub async fn step_once(
        &mut self,
        cancel: CancelToken,
    ) -> Result<Option<RunOutcome>, AgentInteractionError> {
        let policy = RetryPolicy {
            max_attempts: 1,
            ..self.retry_policy
        };
        self.step_with_policy(cancel, policy).await
    }

    async fn step_with_policy(
        &mut self,
        cancel: CancelToken,
        policy: RetryPolicy,
    ) -> Result<Option<RunOutcome>, AgentInteractionError> {
        let result = self.step_effect(cancel, policy).await;
        let result = match self.emit_checkpoint() {
            Ok(()) => result,
            Err(error) => Err(error),
        };
        if !matches!(result, Ok(None)) {
            self.finish_output();
        }
        result
    }

    fn finish_output(&self) {
        self.sink.emit(AgentEvent::TurnFinished {
            context: self.context.clone(),
        });
    }

    async fn step_effect(
        &mut self,
        cancel: CancelToken,
        policy: RetryPolicy,
    ) -> Result<Option<RunOutcome>, AgentInteractionError> {
        self.emit_checkpoint()?;
        let effect = self
            .state
            .effect()
            .ok_or(StateError::UnexpectedCompletion)?;
        let next = match effect {
            Effect::Generate { operation } => {
                let output = match async {
                    self.prepare_generation(cancel.clone()).await?;
                    self.state.begin_effect(operation)?;
                    generation::generate(self, cancel, policy).await
                }
                .await
                {
                    Ok(output) => output,
                    Err(error) => {
                        self.state.generation_failed(operation)?;
                        return Err(error);
                    }
                };
                let next = self.state.generated(operation, output)?;
                if self.checkpoint.is_some() {
                    *self.pending_generation_commit.get_mut().unwrap() = Some(self.context.clone());
                }
                next
            }
            Effect::ExecuteTools { operation, calls } => {
                self.state.begin_effect(operation)?;
                let results = future::join_all(
                    calls
                        .into_iter()
                        .map(|call| self.dispatch_tool_use(call, cancel.clone())),
                )
                .await;
                self.state
                    .tools_completed(operation, results, cancel.is_cancelled())?
            }
            terminal => terminal,
        };
        match next {
            Effect::Finished { answer, reason } => Ok(Some(RunOutcome {
                answer,
                reason,
                usage: self.state.run_usage(),
            })),
            Effect::Cancelled => Err(AgentInteractionError::Cancelled),
            _ => Ok(None),
        }
    }

    /// Append and checkpoint dynamic context before a caller sizes the next
    /// request. Idempotent for this operation; context replacement invalidates it.
    pub async fn prepare_generation(
        &mut self,
        cancel: CancelToken,
    ) -> Result<(), AgentInteractionError> {
        let Some(PendingOperation::Generation { operation }) = self.state.pending_operation()
        else {
            return Ok(());
        };
        if !self.state.can_replace_at_boundary() {
            return Err(StateError::Busy.into());
        }
        if self.prepared_generation == Some(operation) {
            return Ok(());
        }
        self.emit_checkpoint()?;
        if self.notice_pending_checkpoint != Some(operation) {
            self.append_pending_notice(operation, &cancel).await?;
        }
        self.emit_checkpoint()?;
        self.notice_pending_checkpoint = None;
        self.prepared_generation = Some(operation);
        Ok(())
    }

    async fn append_pending_notice(
        &mut self,
        operation: OperationId,
        cancel: &CancelToken,
    ) -> Result<(), AgentInteractionError> {
        let Some(poll_notice) = &self.before_generation_notice else {
            return Ok(());
        };
        let note = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(AgentInteractionError::Cancelled),
            note = poll_notice(&self.context, self.state.history()) => note,
        };
        if let Some(text) = note {
            self.state.append_generation_notice(operation, text)?;
            self.notice_pending_checkpoint = Some(operation);
        }
        Ok(())
    }

    async fn dispatch_tool_use(&self, tool_use: ToolUse, cancel: CancelToken) -> ToolResult {
        if cancel.is_cancelled() {
            return ToolResult::err("cancelled before dispatch");
        }
        let call_id = Uuid::new_v4();
        let background = CancelToken::new();
        self.sink.emit(AgentEvent::ToolStarted {
            call_id,
            tool_use: tool_use.clone(),
            background: background.clone(),
            context: self.context.clone(),
        });

        let work = self
            .tools
            .clone()
            .dispatch(tool_use.clone(), cancel.clone(), background);

        // Race cancel vs tool — but on cancel, give the dispatch a short grace
        // window instead of dropping it immediately. Cancel-aware tools use it
        // to run their own cleanup and return an honest partial result: bash
        // kills the exec's whole process group (kill_on_drop alone SIGKILLs
        // only the leader, orphaning grandchildren), drains its capture tasks,
        // and a mid-write bash session gets its taken ChildStdin back instead
        // of having it dropped (which would close the session's stdin for
        // good). Tools that ignore cancel are abandoned when the grace
        // expires; for subprocess hosts that only abandons this waiter —
        // the pipe demuxes by correlation id, so siblings are unaffected.
        let mut work = std::pin::pin!(work);
        let result = tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                match tokio::time::timeout(CANCEL_TOOL_GRACE, &mut work).await {
                    Ok(result) => result.with_status("cancel requested; partial result recorded"),
                    Err(_) => ToolResult::err("cancel requested; tool did not acknowledge before the deadline; effects are unknown").with_status("cancel requested; effects unknown"),
                }
            }
            result = &mut work => result,
        };
        self.sink.emit(AgentEvent::ToolFinished {
            call_id,
            tool_use,
            result: result.clone(),
            context: self.context.clone(),
        });
        result
    }
}

#[derive(thiserror::Error, Debug)]
pub enum AgentInteractionError {
    #[error("Error during generation: {0}")]
    GenerateError(#[from] generative_model::GenerateError),
    /// In-flight turn aborted (e.g. Ctrl-C). History is left well-formed when tools
    /// had already started (unacknowledged cancellations record unknown effects).
    #[error("cancelled")]
    Cancelled,
    #[error("{0}")]
    State(StateError),
    #[error("could not persist agent state: {0}")]
    Checkpoint(String),
    #[error("compaction failed: {0}")]
    Compaction(String),
}

impl From<StateError> for AgentInteractionError {
    fn from(error: StateError) -> Self {
        match error {
            StateError::InvalidResponse(message) => {
                Self::GenerateError(GenerateError::MalformedResponseError(message))
            }
            error => Self::State(error),
        }
    }
}

impl AgentInteractionError {
    /// Whether the failed turn can be resubmitted as-is, or the last user
    /// message has to be rewound out of history first
    /// by the caller before starting another run.
    pub fn recovery(&self) -> Recovery {
        match self {
            AgentInteractionError::GenerateError(e) => e.recovery(),
            // History is well-formed after a cancel; the same turn can be re-sent.
            AgentInteractionError::Cancelled => Recovery::Retry,
            AgentInteractionError::State(_)
            | AgentInteractionError::Checkpoint(_)
            | AgentInteractionError::Compaction(_) => Recovery::Stop,
        }
    }
}

#[cfg(test)]
mod test_support;

#[cfg(test)]
mod recovery_tests;

#[cfg(test)]
mod tests;
