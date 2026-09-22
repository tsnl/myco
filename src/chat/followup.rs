//! Human follow-ups join a settled model context only after they are durable.

use std::sync::Arc;

use chrono::{DateTime, Utc};

use crate::agent::{Agent, AgentInteractionError};
use crate::generative_model::{Content, Message};
use crate::session::ActiveSession;

/// Called between model requests, after any pending tool batch has settled.
/// Returns whether human input was accepted and the run should continue.
pub type FollowupHandler =
    Arc<dyn Fn(&mut Agent, &ActiveSession) -> Result<bool, AgentInteractionError> + Send + Sync>;

/// Persist before installing input so a failed checkpoint leaves it retryable.
/// The caller holds the session writer and acknowledges its queue only on success.
pub fn append_followup(
    agent: &mut Agent,
    session: &ActiveSession,
    mut content: Vec<Content>,
    accepted_at: DateTime<Utc>,
) -> Result<(), AgentInteractionError> {
    crate::core::image_store::ImageStore::for_profile()
        .and_then(|store| store.externalize(&mut content))
        .map_err(AgentInteractionError::Checkpoint)?;
    let mut history = agent.history().to_vec();
    let index = history.len();
    history.push(Message::UserMessage { content });
    let mut next = agent.state().clone();
    next.replace_at_boundary(history.clone(), agent.last_usage())?;
    let thread_id = agent.context().thread_id.as_deref().ok_or_else(|| {
        AgentInteractionError::Checkpoint("agent is not bound to a thread".into())
    })?;
    session
        .persist_agent_state(thread_id, &next, false, Some((index, accepted_at)))
        .map_err(AgentInteractionError::Checkpoint)?;
    agent.replace_at_boundary(history, agent.last_usage())?;
    super::wire_checkpoint(agent, session);
    Ok(())
}
