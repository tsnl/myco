//! Session policy for explicitly enabled autonomous continuation.

use crate::agent::{Agent, AgentInteractionError, RunOutcome};
use crate::core::CancelToken;
use crate::generative_model::{Content, TurnEndReason};
use crate::session::ActiveSession;

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
