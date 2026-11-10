//! Frontend-independent session workflows, context recovery, and compaction.

use crate::agent::{Agent, AgentInteractionError};
use crate::core::CancelToken;
use crate::generative_model::{Content, Message};

mod session_turn;

#[cfg(test)]
mod recovery_tests;
pub use session_turn::{SessionTurnOutcome, persist_session, run_session_turn, wire_checkpoint};

mod followup;
pub use followup::{FollowupHandler, append_followup};

mod autonomy;

mod runner;
pub use runner::{Compactor, ModelCompactor, SessionRunner, WorkflowEvent};

mod auxiliary;
mod compact_worker;
pub use compact_worker::{
    CompactWorkerError, compact_subagent_prompt, run_compact_worker, run_compact_worker_with_model,
};

pub async fn interact(
    agent: &mut Agent,
    user_input: Vec<Content>,
    cancel: CancelToken,
) -> Result<Vec<Content>, AgentInteractionError> {
    agent.append_input(Message::UserMessage {
        content: user_input,
    })?;
    agent.run(cancel).await
}
