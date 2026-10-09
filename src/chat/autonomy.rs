//! Session policy for explicitly enabled autonomous continuation.

use crate::agent::{Agent, AgentInteractionError, RunOutcome};
use crate::core::CancelToken;
use crate::generative_model::{Content, TurnEndReason};
use crate::session::ActiveSession;
use std::time::Duration;

use super::WorkflowEvent;

//
// Error retries
//

#[derive(Default)]
pub(super) struct Retry {
    failures: u32,
}

impl Retry {
    pub(super) async fn wait(
        &mut self,
        error: AgentInteractionError,
        session: &ActiveSession,
        cancel: &CancelToken,
        observer: &(dyn Fn(WorkflowEvent) + Send + Sync),
    ) -> Result<(), AgentInteractionError> {
        if cancel.is_cancelled() || matches!(error, AgentInteractionError::Cancelled) {
            return Err(cancellation_error(error));
        }
        if !session.with(|session| session.auto_continue) {
            return Err(error);
        }
        let delay = Duration::from_secs((1 << self.failures.min(3)).min(5));
        self.failures = self.failures.saturating_add(1);
        observer(WorkflowEvent::Retrying {
            error: error.to_string(),
            delay,
        });
        let deadline = tokio::time::Instant::now() + delay;
        let mut changes = tokio::time::interval(Duration::from_millis(100));
        loop {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => return Err(cancellation_error(error)),
                _ = tokio::time::sleep_until(deadline) => {
                    return if session.with(|session| session.auto_continue) { Ok(()) } else { Err(error) };
                }
                _ = changes.tick() => {
                    if !session.with(|session| session.auto_continue) { return Err(error); }
                }
            }
        }
    }
}

fn cancellation_error(error: AgentInteractionError) -> AgentInteractionError {
    // Stop waiting immediately, but keep an unsaved-observation failure visible.
    match error {
        AgentInteractionError::Checkpoint(_) => error,
        _ => AgentInteractionError::Cancelled,
    }
}

const INSTRUCTION: &str = "Auto-continue is enabled for this session. Continue the user's active task autonomously. \
    When the task is complete, or progress requires user input, call session_meta action=disable_auto_continue \
    and explain the outcome. Do not invent new work after completing the task. Cancellation stops the current run.";

#[cfg(test)]
#[path = "autonomy_tests.rs"]
mod tests;

fn notice(reason: &str) -> Content {
    Content::System {
        kind: "continuation".into(),
        text: INSTRUCTION.into(),
        data: serde_json::json!({"reason": reason}),
    }
}

pub(super) fn announce(
    agent: &mut Agent,
    session: &ActiveSession,
) -> Result<(), AgentInteractionError> {
    if session.with(|session| session.auto_continue) {
        agent.append_system(vec![notice("auto_continue_enabled")])?;
    }
    Ok(())
}

pub(super) fn continue_after(
    agent: &mut Agent,
    session: &ActiveSession,
    outcome: &RunOutcome,
    cancel: &CancelToken,
) -> Result<bool, AgentInteractionError> {
    if cancel.is_cancelled()
        || !matches!(
            outcome.reason,
            TurnEndReason::EndTurn | TurnEndReason::MaxTokens
        )
        || !session.with(|session| session.auto_continue)
    {
        return Ok(false);
    }
    super::followup::append_at_boundary(agent, session, vec![notice("auto_continue")], None)?;
    Ok(true)
}
