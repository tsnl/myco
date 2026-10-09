//! Deterministic model/tool transitions. Effects carry identities so late or
//! duplicate completions cannot mutate context or dispatch another tool batch.

use myco_model::{
    Content, GenerateOutput, Message, TokenUsage, ToolResult, ToolUse, TurnEndReason,
    answer_content,
};

use crate::CONTINUE_PROMPT;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct OperationId(u64);

/// Durable intent. A tool intent without its result is an unknown external
/// effect after restart, even when dispatch may not actually have begun.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum PendingOperation {
    Generation { operation: OperationId },
    Tools { operation: OperationId },
}

#[derive(Debug, Clone)]
pub enum Effect {
    Generate {
        operation: OperationId,
    },
    ExecuteTools {
        operation: OperationId,
        calls: Vec<ToolUse>,
    },
    Finished {
        answer: Vec<Content>,
        reason: TurnEndReason,
    },
    Cancelled,
}

#[derive(Debug, Clone)]
enum Next {
    Generate,
    Finish {
        answer: Vec<Content>,
        reason: TurnEndReason,
    },
}

#[derive(Debug, Clone)]
enum Phase {
    Ready,
    GenerationFailed,
    Finished {
        answer: Vec<Content>,
        reason: TurnEndReason,
    },
    Cancelled,
    Generating(OperationId),
    Tools {
        operation: OperationId,
        count: usize,
        next: Next,
    },
}

#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum StateError {
    #[error("agent has an outstanding operation")]
    Busy,
    #[error("completion does not match the outstanding operation")]
    UnexpectedCompletion,
    #[error("invalid context: {0}")]
    InvalidContext(String),
    #[error("invalid model response: {0}")]
    InvalidResponse(String),
}

/// Context and control state, without a model, executor, clock, or event sink.
/// Every transition is synchronous; the caller interprets returned effects.
#[derive(Debug, Clone)]
pub struct AgentState {
    history: Vec<Message>,
    context_size: crate::context_size::ContextSize,
    last_usage: Option<TokenUsage>,
    phase: Phase,
    sequence: u64,
    executing: bool,
    run_usage: Option<TokenUsage>,
    truncations: u32,
    max_truncated_resumes: u32,
}

impl Default for AgentState {
    fn default() -> Self {
        Self {
            history: Vec::new(),
            context_size: Default::default(),
            last_usage: None,
            phase: Phase::Ready,
            sequence: 0,
            executing: false,
            run_usage: None,
            truncations: 0,
            max_truncated_resumes: crate::DEFAULT_MAX_TRUNCATED_RESUMES,
        }
    }
}

impl AgentState {
    pub fn history(&self) -> &[Message] {
        &self.history
    }

    pub fn last_usage(&self) -> Option<TokenUsage> {
        self.last_usage
    }

    pub(crate) fn context_tokens_estimate(&self) -> u64 {
        self.context_size.tokens()
    }

    pub(crate) fn restore_context_size_hint(&mut self, tokens: Option<u64>) {
        self.context_size.restore_hint(tokens);
    }

    fn push(&mut self, message: Message) {
        self.context_size.append(&message);
        self.history.push(message);
    }

    fn replace_history(&mut self, history: Vec<Message>, usage: Option<TokenUsage>) {
        if (usage.is_none() || usage == self.last_usage) && history.starts_with(&self.history) {
            for message in &history[self.history.len()..] {
                self.context_size.append(message);
            }
        } else {
            self.context_size = crate::context_size::ContextSize::restored(&history, usage);
        }
        self.history = history;
        self.last_usage = usage;
    }

    pub fn run_usage(&self) -> Option<TokenUsage> {
        self.run_usage
    }

    pub fn is_idle(&self) -> bool {
        matches!(
            self.phase,
            Phase::Ready | Phase::GenerationFailed | Phase::Finished { .. } | Phase::Cancelled
        )
    }

    pub fn pending_operation(&self) -> Option<PendingOperation> {
        match self.phase {
            Phase::Ready | Phase::GenerationFailed | Phase::Finished { .. } | Phase::Cancelled => {
                None
            }
            Phase::Generating(operation) => Some(PendingOperation::Generation { operation }),
            Phase::Tools { operation, .. } => Some(PendingOperation::Tools { operation }),
        }
    }

    /// A pending tool batch leaves a call/result boundary incomplete. It must
    /// settle before this history can be used as model input or forked.
    pub fn is_model_boundary(&self) -> bool {
        !matches!(self.phase, Phase::Tools { .. })
    }

    /// The next effect, or terminal outcome, without advancing the controller.
    pub fn effect(&self) -> Option<Effect> {
        match &self.phase {
            Phase::Ready | Phase::GenerationFailed => None,
            Phase::Generating(operation) => Some(Effect::Generate {
                operation: *operation,
            }),
            Phase::Tools { operation, .. } => {
                let Some(Message::AssistantMessage { tool_uses, .. }) = self.history.last() else {
                    unreachable!()
                };
                Some(Effect::ExecuteTools {
                    operation: *operation,
                    calls: tool_uses.clone(),
                })
            }
            Phase::Finished { answer, reason } => Some(Effect::Finished {
                answer: answer.clone(),
                reason: reason.clone(),
            }),
            Phase::Cancelled => Some(Effect::Cancelled),
        }
    }

    /// An interpreter must not dispatch a dropped future's operation again.
    pub fn begin_effect(&mut self, operation: OperationId) -> Result<(), StateError> {
        if self.executing {
            return Err(StateError::Busy);
        }
        match self.pending_operation() {
            Some(
                PendingOperation::Generation {
                    operation: expected,
                }
                | PendingOperation::Tools {
                    operation: expected,
                },
            ) if expected == operation => {
                self.executing = true;
                Ok(())
            }
            _ => Err(StateError::UnexpectedCompletion),
        }
    }

    pub fn can_replace_at_boundary(&self) -> bool {
        matches!(
            self.phase,
            Phase::Generating(_) | Phase::GenerationFailed | Phase::Finished { .. }
        ) && !self.executing
    }

    pub fn cancel_at_boundary(&mut self) -> Result<(), StateError> {
        if !self.can_replace_at_boundary() {
            return Err(StateError::Busy);
        }
        self.phase = Phase::Cancelled;
        Ok(())
    }

    /// Install derived context between generations while retaining run policy
    /// counters. The old operation identity can no longer complete this state.
    pub fn replace_at_boundary(
        &mut self,
        history: Vec<Message>,
        usage: Option<TokenUsage>,
    ) -> Result<(), StateError> {
        if !self.can_replace_at_boundary() {
            return Err(StateError::Busy);
        }
        validate_context(&history)?;
        self.replace_history(history, usage);
        self.generate();
        Ok(())
    }

    /// Explicitly abandon work whose future was dropped. Completed observations
    /// survive; an executing tool batch is reconciled as unknown, never replayed.
    /// Live intents that never began have known, unexecuted outcomes.
    pub fn recover_interrupted(&mut self) -> Result<(), StateError> {
        if self.executing {
            let history = recover_checkpoint(self.history.clone(), self.pending_operation())?;
            self.replace_history(history, self.last_usage);
        } else if let Phase::Tools {
            operation, count, ..
        } = self.phase
        {
            let results = (0..count).map(|_| ToolResult::err(
                "not executed: the run stopped before this tool batch began. Reconsider this action using the latest input before trying again.",
            )).collect();
            self.tools_completed(operation, results, true)?;
        }
        self.phase = Phase::Ready;
        self.executing = false;
        Ok(())
    }

    fn require_ready(&self) -> Result<(), StateError> {
        if self.is_idle() {
            Ok(())
        } else {
            Err(StateError::Busy)
        }
    }

    pub fn replace_context(
        &mut self,
        history: Vec<Message>,
        usage: Option<TokenUsage>,
    ) -> Result<(), StateError> {
        self.require_ready()?;
        validate_context(&history)?;
        self.replace_history(history, usage);
        self.phase = Phase::Ready;
        Ok(())
    }

    pub fn append_input(&mut self, message: Message) -> Result<(), StateError> {
        self.require_ready()?;
        if !matches!(message, Message::UserMessage { .. }) {
            return Err(StateError::InvalidContext(
                "input must be a user message".into(),
            ));
        }
        self.phase = Phase::Ready;
        self.push(message);
        Ok(())
    }

    /// Record runtime observations at a settled boundary without changing the
    /// run's result, usage, or truncation counters.
    pub fn append_system(&mut self, parts: Vec<Content>) -> Result<(), StateError> {
        if self.executing || !self.is_model_boundary() {
            return Err(StateError::Busy);
        }
        if parts.is_empty()
            || parts
                .iter()
                .any(|part| !matches!(part, Content::System { .. }))
        {
            return Err(StateError::InvalidContext(
                "runtime observations must be system parts".into(),
            ));
        }
        self.push(Message::UserMessage { content: parts });
        if matches!(self.phase, Phase::Generating(_)) {
            self.generate();
        }
        Ok(())
    }

    pub fn truncate_history(&mut self, index: usize) -> Result<Vec<Message>, StateError> {
        self.require_ready()?;
        let prefix = self.history.get(..index).ok_or_else(|| {
            StateError::InvalidContext("rewind index is beyond the history".into())
        })?;
        validate_context(prefix)?;
        let dropped = self.history.split_off(index);
        self.context_size = crate::context_size::ContextSize::restored(&self.history, None);
        self.last_usage = None;
        self.phase = Phase::Ready;
        Ok(dropped)
    }

    pub fn set_max_truncated_resumes(&mut self, resumes: u32) {
        self.max_truncated_resumes = resumes;
    }

    /// Retry a failed generation within its existing run budget. Appending new
    /// input first transitions to Ready and begins a fresh run instead.
    pub fn start(&mut self) -> Result<Effect, StateError> {
        self.require_ready()?;
        if self.history.is_empty() {
            return Err(StateError::InvalidContext(
                "cannot run an empty context".into(),
            ));
        }
        if !matches!(self.phase, Phase::GenerationFailed) {
            self.run_usage = None;
            self.truncations = 0;
        }
        Ok(self.generate())
    }

    fn operation(&mut self) -> OperationId {
        self.sequence = self
            .sequence
            .checked_add(1)
            .expect("operation sequence exhausted");
        OperationId(self.sequence)
    }

    fn generate(&mut self) -> Effect {
        let operation = self.operation();
        self.phase = Phase::Generating(operation);
        self.executing = false;
        Effect::Generate { operation }
    }

    /// Stop a failed or cancelled generation without adding a partial response.
    /// Context can be repaired at this boundary without resetting run counters.
    pub fn generation_failed(&mut self, operation: OperationId) -> Result<(), StateError> {
        if !matches!(self.phase, Phase::Generating(id) if id == operation) {
            return Err(StateError::UnexpectedCompletion);
        }
        self.phase = Phase::GenerationFailed;
        self.executing = false;
        Ok(())
    }

    pub(crate) fn append_generation_notice(
        &mut self,
        operation: OperationId,
        text: String,
    ) -> Result<(), StateError> {
        if !matches!(self.phase, Phase::Generating(id) if id == operation) {
            return Err(StateError::UnexpectedCompletion);
        }
        let notice = Content::System {
            kind: "generation_notice".into(),
            text,
            data: serde_json::Value::Null,
        };
        // Notices belong to the current input so user-turn indexes and tool
        // call/result pairing remain stable across rewind and compaction.
        match self.history.last_mut() {
            Some(Message::UserMessage { content }) => {
                self.context_size.append_part(&notice);
                content.push(notice);
            }
            Some(Message::ToolResults { tool_use_results }) if !tool_use_results.is_empty() => {
                self.context_size.append_part(&notice);
                tool_use_results.last_mut().unwrap().content.push(notice);
            }
            _ => self.push(Message::UserMessage {
                content: vec![notice],
            }),
        }
        Ok(())
    }

    pub fn generated(
        &mut self,
        operation: OperationId,
        output: GenerateOutput,
    ) -> Result<Effect, StateError> {
        if !matches!(self.phase, Phase::Generating(id) if id == operation) {
            return Err(StateError::UnexpectedCompletion);
        }
        if output
            .content
            .iter()
            .any(|part| matches!(part, Content::System { .. }))
        {
            self.generation_failed(operation)?;
            return Err(StateError::InvalidResponse(
                "model output contains a runtime-only system part".into(),
            ));
        }
        if output.turn_end_reason == TurnEndReason::ToolUse && output.tool_uses.is_empty() {
            self.generation_failed(operation)?;
            return Err(StateError::InvalidResponse(
                "turn ended in tool_use but streamed zero tool uses".into(),
            ));
        }
        self.executing = false;
        if let Some(usage) = output.usage {
            self.context_size.observe_input(usage);
            self.run_usage = Some(TokenUsage {
                output_tokens: self
                    .run_usage
                    .map_or(0, |previous| previous.output_tokens)
                    .saturating_add(usage.output_tokens),
                ..usage
            });
            self.last_usage = self.run_usage;
        }
        let answer = answer_content(&output.content);
        let reason = output.turn_end_reason;
        let calls = output.tool_uses;
        self.push(Message::AssistantMessage {
            content: output.content,
            tool_uses: calls.clone(),
            turn_end_reason: Some(reason.clone()),
        });
        self.phase = Phase::Ready;
        let resume = if reason == TurnEndReason::MaxTokens {
            self.truncations = self.truncations.saturating_add(1);
            self.truncations <= self.max_truncated_resumes
        } else {
            self.truncations = 0;
            false
        };
        let next = if reason == TurnEndReason::ToolUse || resume {
            Next::Generate
        } else {
            Next::Finish { answer, reason }
        };
        if calls.is_empty() {
            if resume {
                self.push(Message::UserMessage {
                    content: vec![Content::System {
                        kind: "continuation".into(),
                        text: CONTINUE_PROMPT.into(),
                        data: serde_json::json!({"reason":"max_tokens"}),
                    }],
                });
            }
            Ok(self.advance(next))
        } else {
            let operation = self.operation();
            self.phase = Phase::Tools {
                operation,
                count: calls.len(),
                next,
            };
            Ok(Effect::ExecuteTools { operation, calls })
        }
    }

    /// Results are in call order, including cancellation or unknown outcomes.
    /// Cancellation settles the entire batch and prevents another generation.
    pub fn tools_completed(
        &mut self,
        operation: OperationId,
        results: Vec<ToolResult>,
        cancelled: bool,
    ) -> Result<Effect, StateError> {
        let Phase::Tools {
            operation: expected,
            count,
            next,
        } = &self.phase
        else {
            return Err(StateError::UnexpectedCompletion);
        };
        if operation != *expected {
            return Err(StateError::UnexpectedCompletion);
        }
        if results.len() != *count {
            return Err(StateError::InvalidContext(format!(
                "expected {count} tool results, received {}",
                results.len(),
            )));
        }
        let next = next.clone();
        self.executing = false;
        self.push(Message::ToolResults {
            tool_use_results: results,
        });
        if cancelled {
            self.phase = Phase::Cancelled;
            Ok(Effect::Cancelled)
        } else {
            Ok(self.advance(next))
        }
    }

    fn advance(&mut self, next: Next) -> Effect {
        match next {
            Next::Generate => self.generate(),
            Next::Finish { answer, reason } => {
                self.phase = Phase::Finished {
                    answer: answer.clone(),
                    reason: reason.clone(),
                };
                Effect::Finished { answer, reason }
            }
        }
    }
}

/// Every tool call has exactly one result in the immediately following batch.
pub fn validate_context(history: &[Message]) -> Result<(), StateError> {
    let mut pending = 0;
    for (index, message) in history.iter().enumerate() {
        match message {
            Message::ToolResults { tool_use_results } => {
                if pending == 0 || pending != tool_use_results.len() {
                    return Err(StateError::InvalidContext(format!(
                        "message {index} has {} results for {pending} tool calls",
                        tool_use_results.len(),
                    )));
                }
                pending = 0;
            }
            _ if pending != 0 => {
                return Err(StateError::InvalidContext(format!(
                    "message {index} interrupts an unanswered tool batch",
                )));
            }
            Message::AssistantMessage { tool_uses, .. } => pending = tool_uses.len(),
            Message::UserMessage { .. } => {}
        }
    }
    if pending != 0 {
        return Err(StateError::InvalidContext(
            "history ends with unanswered tool calls".into(),
        ));
    }
    Ok(())
}

pub fn validate_checkpoint(
    history: &[Message],
    pending: Option<PendingOperation>,
) -> Result<(), StateError> {
    if matches!(pending, Some(PendingOperation::Tools { .. })) {
        match history.split_last() {
            Some((Message::AssistantMessage { tool_uses, .. }, prefix))
                if !tool_uses.is_empty() =>
            {
                validate_context(prefix)
            }
            _ => Err(StateError::InvalidContext(
                "pending tools require an unanswered assistant tool batch".into(),
            )),
        }
    } else {
        validate_context(history)
    }
}

/// Recover observations without reissuing any pending operation. The caller
/// must persist this context before allowing new effects.
pub fn recover_checkpoint(
    mut history: Vec<Message>,
    pending: Option<PendingOperation>,
) -> Result<Vec<Message>, StateError> {
    validate_checkpoint(&history, pending)?;
    let Some(pending) = pending else {
        return Ok(history);
    };
    let text = match pending {
        PendingOperation::Tools { .. } => {
            let Some(Message::AssistantMessage { tool_uses, .. }) = history.last() else {
                unreachable!()
            };
            let results = tool_uses.iter().map(|_| ToolResult::err(
                "execution interrupted before the outcome was saved; effects are unknown. Inspect external state before retrying this action.",
            )).collect();
            history.push(Message::ToolResults {
                tool_use_results: results,
            });
            "This runtime recovered an interrupted tool batch. Its results were not durably recorded. Calls may have executed; they have not been replayed. Inspect external state before repeating any action."
        }
        PendingOperation::Generation { .. } => {
            "A generation was pending when the previous runtime stopped. No completed response from that operation was recorded. Continue from the saved observations."
        }
    };
    history.push(Message::UserMessage {
        content: vec![Content::System {
            kind: "interrupted".into(),
            text: text.into(),
            data: serde_json::json!({"pending":pending}),
        }],
    });
    Ok(history)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{assistant, assistant_tool, tool_results, user};
    use serde_json::json;

    fn state() -> AgentState {
        let mut state = AgentState::default();
        state.append_input(user("task")).unwrap();
        state
    }

    fn generation(effect: Effect) -> OperationId {
        match effect {
            Effect::Generate { operation } => operation,
            other => panic!("expected generation, got {other:?}"),
        }
    }

    fn response(reason: TurnEndReason, calls: usize) -> GenerateOutput {
        GenerateOutput {
            content: vec![Content::Text {
                text: "answer".into(),
            }],
            tool_uses: (0..calls)
                .map(|index| ToolUse {
                    name: "test".into(),
                    input: json!({"index": index}),
                })
                .collect(),
            turn_end_reason: reason,
            usage: Some(TokenUsage {
                input_tokens: 100,
                output_tokens: 10,
                cached_input_tokens: 20,
            }),
        }
    }

    #[test]
    fn pure_transitions_replay_the_same_tool_loop_without_a_runtime() {
        let mut left = state();
        let mut right = left.clone();
        for machine in [&mut left, &mut right] {
            let op = generation(machine.start().unwrap());
            let Effect::ExecuteTools { operation, calls } = machine
                .generated(op, response(TurnEndReason::ToolUse, 2))
                .unwrap()
            else {
                panic!()
            };
            assert_eq!(calls.len(), 2);
            assert!(!machine.is_model_boundary());
            let next = machine
                .tools_completed(
                    operation,
                    vec![ToolResult::text("first"), ToolResult::err("second")],
                    false,
                )
                .unwrap();
            let op = generation(next);
            assert!(matches!(
                machine
                    .generated(op, response(TurnEndReason::EndTurn, 0))
                    .unwrap(),
                Effect::Finished {
                    reason: TurnEndReason::EndTurn,
                    ..
                }
            ));
            assert_eq!(machine.last_usage().unwrap().output_tokens, 20);
            validate_context(machine.history()).unwrap();
        }
        assert_eq!(format!("{left:?}"), format!("{right:?}"));
    }

    #[test]
    fn context_growth_survives_rebinding_missing_usage_and_model_changes() {
        let mut machine = state();
        let op = generation(machine.start().unwrap());
        machine
            .generated(op, response(TurnEndReason::EndTurn, 0))
            .unwrap();
        let measured = machine.context_tokens_estimate();
        machine
            .replace_context(machine.history().to_vec(), machine.last_usage())
            .unwrap();
        assert_eq!(machine.context_tokens_estimate(), measured);
        machine
            .append_input(Message::UserMessage {
                content: vec![Content::Text {
                    text: "x".repeat(3000),
                }],
            })
            .unwrap();
        let grown = machine.context_tokens_estimate();
        assert!(grown >= measured + 1000);
        let op = generation(machine.start().unwrap());
        let mut unmeasured = response(TurnEndReason::EndTurn, 0);
        unmeasured.usage = None;
        machine.generated(op, unmeasured).unwrap();
        assert!(machine.context_tokens_estimate() > grown);
        let before_change = machine.context_tokens_estimate();
        machine
            .replace_context(machine.history().to_vec(), None)
            .unwrap();
        assert_eq!(machine.context_tokens_estimate(), before_change);
        machine.truncate_history(1).unwrap();
        assert!(machine.context_tokens_estimate() < measured);
    }

    #[test]
    fn stale_completions_and_input_during_work_leave_state_unchanged() {
        let mut machine = state();
        let stale = generation(machine.start().unwrap());
        machine.generation_failed(stale).unwrap();
        let current = generation(machine.start().unwrap());
        let before = format!("{machine:?}");
        assert_eq!(
            machine
                .generated(stale, response(TurnEndReason::ToolUse, 1))
                .unwrap_err(),
            StateError::UnexpectedCompletion
        );
        assert_eq!(
            machine.append_input(user("interjection")),
            Err(StateError::Busy)
        );
        assert_eq!(
            machine.replace_context(vec![user("replacement")], None),
            Err(StateError::Busy)
        );
        assert!(matches!(machine.truncate_history(0), Err(StateError::Busy)));
        assert_eq!(
            machine.generation_failed(stale),
            Err(StateError::UnexpectedCompletion)
        );
        assert_eq!(format!("{machine:?}"), before);
        machine.generation_failed(current).unwrap();
    }

    #[test]
    fn tool_batch_requires_matching_count_and_can_only_complete_once() {
        let mut machine = state();
        let generation = generation(machine.start().unwrap());
        let Effect::ExecuteTools { operation, .. } = machine
            .generated(generation, response(TurnEndReason::ToolUse, 2))
            .unwrap()
        else {
            panic!()
        };
        let before = format!("{machine:?}");
        assert!(
            machine
                .tools_completed(operation, vec![ToolResult::text("partial")], false)
                .is_err()
        );
        assert_eq!(format!("{machine:?}"), before);
        assert!(matches!(
            machine
                .tools_completed(
                    operation,
                    vec![ToolResult::text("finished"), ToolResult::err("cancelled")],
                    true
                )
                .unwrap(),
            Effect::Cancelled
        ));
        validate_context(machine.history()).unwrap();
        assert_eq!(
            machine
                .tools_completed(operation, vec![], false)
                .unwrap_err(),
            StateError::UnexpectedCompletion
        );
        machine.append_input(user("next turn")).unwrap();
    }

    #[test]
    fn context_replacement_and_rewind_reject_split_or_orphaned_tool_batches() {
        let mut machine = state();
        let call = assistant_tool(None, "test", json!({}));
        for history in [
            vec![call.clone()],
            vec![tool_results(&["orphan"])],
            vec![call.clone(), user("interrupt"), tool_results(&["result"])],
            vec![call.clone(), tool_results(&["extra", "extra"])],
            vec![Message::ToolResults {
                tool_use_results: vec![],
            }],
        ] {
            assert!(machine.replace_context(history, None).is_err());
            assert_eq!(machine.history().len(), 1);
        }
        machine
            .replace_context(
                vec![user("task"), call, tool_results(&["ok"]), assistant("done")],
                None,
            )
            .unwrap();
        assert!(machine.truncate_history(2).is_err());
        assert!(machine.truncate_history(99).is_err());
        assert_eq!(machine.history().len(), 4);
        machine.truncate_history(1).unwrap();
    }

    #[test]
    fn truncation_cap_is_an_explicit_finish_reason_and_resets_for_a_new_run() {
        let mut machine = state();
        machine.set_max_truncated_resumes(1);
        let first = generation(machine.start().unwrap());
        let next = machine
            .generated(first, response(TurnEndReason::MaxTokens, 0))
            .unwrap();
        let second = generation(next);
        assert!(matches!(
            machine
                .generated(second, response(TurnEndReason::MaxTokens, 0))
                .unwrap(),
            Effect::Finished {
                reason: TurnEndReason::MaxTokens,
                ..
            }
        ));
        machine.append_input(user("new task")).unwrap();
        let next = generation(machine.start().unwrap());
        assert!(matches!(
            machine
                .generated(next, response(TurnEndReason::MaxTokens, 0))
                .unwrap(),
            Effect::Generate { .. }
        ));
    }

    #[test]
    fn restarting_failed_generation_preserves_usage_and_truncation_limits() {
        let mut machine = state();
        machine.set_max_truncated_resumes(1);
        let first = generation(machine.start().unwrap());
        let pending = generation(
            machine
                .generated(first, response(TurnEndReason::MaxTokens, 0))
                .unwrap(),
        );
        machine.generation_failed(pending).unwrap();
        let retried = generation(machine.start().unwrap());
        assert_eq!(machine.run_usage().unwrap().output_tokens, 10);
        assert!(matches!(
            machine
                .generated(retried, response(TurnEndReason::MaxTokens, 0))
                .unwrap(),
            Effect::Finished {
                reason: TurnEndReason::MaxTokens,
                ..
            }
        ));
        assert_eq!(machine.run_usage().unwrap().output_tokens, 20);

        machine.append_input(user("new task")).unwrap();
        let fresh = generation(machine.start().unwrap());
        assert!(machine.run_usage().is_none());
        assert!(matches!(
            machine
                .generated(fresh, response(TurnEndReason::MaxTokens, 0))
                .unwrap(),
            Effect::Generate { .. }
        ));
        assert_eq!(machine.run_usage().unwrap().output_tokens, 10);
    }

    #[test]
    fn invalid_outputs_leave_history_usage_and_truncation_budget_unchanged() {
        let mut machine = state();
        machine.set_max_truncated_resumes(1);
        let first = generation(machine.start().unwrap());
        let mut pending = generation(
            machine
                .generated(first, response(TurnEndReason::MaxTokens, 0))
                .unwrap(),
        );
        let history = serde_json::to_value(machine.history()).unwrap();
        let usage = machine.run_usage();
        let mut system_output = response(TurnEndReason::EndTurn, 0);
        system_output.content.push(Content::System {
            kind: "invalid".into(),
            text: "invalid".into(),
            data: serde_json::Value::Null,
        });
        for output in [response(TurnEndReason::ToolUse, 0), system_output] {
            machine.begin_effect(pending).unwrap();
            assert!(matches!(
                machine.generated(pending, output),
                Err(StateError::InvalidResponse(_))
            ));
            assert_eq!(serde_json::to_value(machine.history()).unwrap(), history);
            assert_eq!(machine.run_usage(), usage);
            assert_eq!(machine.last_usage(), usage);
            pending = generation(machine.start().unwrap());
        }
        assert!(matches!(
            machine
                .generated(pending, response(TurnEndReason::MaxTokens, 0))
                .unwrap(),
            Effect::Finished {
                reason: TurnEndReason::MaxTokens,
                ..
            }
        ));
    }
}
