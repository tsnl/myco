//! Chat operations over a separately owned agent: user turns, rewind, and compaction.

use crate::agent::{Agent, AgentInteractionError};
use crate::core::CancelToken;
use crate::generative_model::{Content, Message};

mod compact_worker;
pub use compact_worker::{CompactWorkerError, compact_subagent_prompt, run_compact_worker};

pub async fn interact(
    agent: &mut Agent,
    user_input: Vec<Content>,
    cancel: CancelToken,
) -> Result<Vec<Content>, AgentInteractionError> {
    agent.append_input(Message::UserMessage {
        content: user_input,
    });
    agent.run(cancel).await
}

/// Remove the latest user turn and its descendants after a size rejection.
/// The earlier context remains a well-formed prefix; usage is invalidated.
pub fn rewind_last_user_turn(agent: &mut Agent) -> Option<Vec<Content>> {
    let index = agent
        .history()
        .iter()
        .rposition(|message| matches!(message, Message::UserMessage { .. }))?;
    match agent.truncate_history(index).remove(0) {
        Message::UserMessage { content } => Some(content),
        _ => unreachable!("index identifies a user message"),
    }
}
