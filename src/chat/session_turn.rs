//! Session turns stamp input, recover rejected context, and persist every completed run.

use crate::agent::{Agent, AgentInteractionError};
use crate::core::CancelToken;
use crate::generative_model::{Content, Message, Recovery};
use crate::prompts;
use crate::session::ActiveSession;

pub struct SessionTurnOutcome {
    pub result: Result<Vec<Content>, AgentInteractionError>,
    pub rewound: Option<Vec<Content>>,
}

/// Submit already-expanded input. Nonfatal persistence failures reach `on_warning`.
/// The caller keeps ownership of the agent, session lock, and cancellation source.
pub async fn run_session_turn(
    agent: &mut Agent,
    session: &ActiveSession,
    mut input: Vec<Content>,
    forked: bool,
    cancel: CancelToken,
    on_warning: impl Fn(&str),
) -> SessionTurnOutcome {
    if let Err(error) = auto_title(session, &input) {
        on_warning(&format!("could not auto-title session: {error}"));
    }
    if needs_session_stamp(agent.history(), forked) {
        stamp_input(session, &mut input);
    }
    let result = super::interact(agent, input, cancel).await;
    let rewound = rewind_rejected_input(agent, &result);
    if let Err(error) = persist_session(agent, session, true) {
        on_warning(&format!("could not save session: {error}"));
    }
    SessionTurnOutcome { result, rewound }
}

fn auto_title(session: &ActiveSession, input: &[Content]) -> Result<(), String> {
    if let Some(text) = input.iter().find_map(|content| match content {
        Content::Text { text } => Some(text),
        _ => None,
    }) {
        session.maybe_auto_title_from_user_text(text)?;
    }
    Ok(())
}

fn stamp_input(session: &ActiveSession, input: &mut Vec<Content>) {
    let text = session.with(|session| prompts::session_stamp(&session.id, session.created_at));
    input.insert(0, Content::Text { text });
}

fn rewind_rejected_input(
    agent: &mut Agent,
    result: &Result<Vec<Content>, AgentInteractionError>,
) -> Option<Vec<Content>> {
    match result {
        Err(error) if error.recovery() == Recovery::OmitLastMessage => {
            super::rewind_last_user_turn(agent)
        }
        _ => None,
    }
}

/// Save well-formed history boundaries; warnings do not interrupt a run.
pub fn wire_checkpoint(
    agent: &mut Agent,
    session: &ActiveSession,
    on_warning: impl Fn(&str) + Send + Sync + 'static,
) {
    let session = session.clone();
    agent.set_checkpoint(Box::new(move |messages, usage| {
        if let Err(error) = session.persist_messages(messages, usage, false) {
            on_warning(&format!("mid-turn session save failed: {error}"));
        }
    }));
}

/// Save current context, including an empty rewind of an existing session.
/// A session that never accepted input does not create a file.
pub fn persist_session(agent: &Agent, session: &ActiveSession, force: bool) -> Result<(), String> {
    let history = agent.history();
    if history.is_empty() && !session.snapshot().json_path().exists() {
        return Ok(());
    }
    session.persist_messages(history, agent.last_usage(), force)
}

// Forks inherit a parent's stamp and need their own; resumes keep the existing one.
fn needs_session_stamp(history: &[Message], forked: bool) -> bool {
    history.is_empty() || forked
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::agent::NullEventSink;
    use crate::generative_model::{GenerateError, GenerateOutput, TurnEndReason};
    use crate::harness::Harness;
    use crate::session::Session;
    use crate::test_support::{ScriptedModel, assistant, temp_home, user};

    fn agent(model: Arc<ScriptedModel>) -> Agent {
        Agent::new(
            model,
            Harness::local_with_services(vec![]),
            Arc::new(NullEventSink),
        )
    }

    fn submit(
        agent: &mut Agent,
        session: &ActiveSession,
        cancel: CancelToken,
    ) -> SessionTurnOutcome {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(run_session_turn(
                agent,
                session,
                vec![Content::Text {
                    text: "task".into(),
                }],
                false,
                cancel,
                |warning| panic!("{warning}"),
            ))
    }

    fn saved_messages(session: &ActiveSession) -> serde_json::Value {
        let saved = Session::load(&session.snapshot().json_path()).unwrap();
        serde_json::to_value(saved.messages).unwrap()
    }

    #[test]
    fn rejected_input_is_rewound_before_the_session_is_saved() {
        let _home = temp_home("chat-rejection");
        let mut document = Session::new("test");
        document.messages = vec![user("earlier"), assistant("answer")];
        let expected = serde_json::to_value(&document.messages).unwrap();
        let mut agent = agent(
            ScriptedModel::new(vec![])
                .then_fail(GenerateError::RequestTooLargeError("oversized".into())),
        );
        agent.replace_context(document.messages.clone(), None);
        let session = ActiveSession::new(document);
        wire_checkpoint(&mut agent, &session, |warning| panic!("{warning}"));
        let outcome = submit(&mut agent, &session, CancelToken::new());
        assert!(outcome.result.is_err());
        assert!(
            matches!(outcome.rewound.as_deref(), Some([Content::Text { text }]) if text == "task")
        );
        assert_eq!(saved_messages(&session), expected);
        assert_eq!(serde_json::to_value(agent.history()).unwrap(), expected);
    }

    #[test]
    fn cancellation_keeps_the_stamped_input_and_title_on_disk() {
        let _home = temp_home("chat-cancel");
        let session = ActiveSession::new(Session::new("test"));
        let mut agent = agent(ScriptedModel::new(vec![]));
        let cancel = CancelToken::new();
        cancel.cancel();
        let outcome = submit(&mut agent, &session, cancel);
        assert!(matches!(
            outcome.result,
            Err(AgentInteractionError::Cancelled)
        ));
        assert!(outcome.rewound.is_none());
        let saved = Session::load(&session.snapshot().json_path()).unwrap();
        assert_eq!(saved.title.as_deref(), Some("task"));
        assert!(
            matches!(&saved.messages[0], Message::UserMessage { content }
            if matches!(&content[0], Content::Text { text } if text.contains(&saved.id)))
        );
        assert_eq!(
            saved_messages(&session),
            serde_json::to_value(agent.history()).unwrap()
        );
    }

    #[test]
    fn successful_and_failed_turns_both_persist_the_current_history() {
        let _home = temp_home("chat-completion");
        let session = ActiveSession::new(Session::new("test"));
        let mut agent = agent(
            ScriptedModel::new(vec![GenerateOutput {
                content: vec![Content::Text {
                    text: "answer".into(),
                }],
                tool_uses: vec![],
                turn_end_reason: TurnEndReason::EndTurn,
                usage: None,
            }])
            .then_fail(GenerateError::ExecutionError("unavailable".into())),
        );
        for succeeds in [true, false] {
            let outcome = submit(&mut agent, &session, CancelToken::new());
            assert_eq!(outcome.result.is_ok(), succeeds);
            assert!(outcome.rewound.is_none());
            assert_eq!(
                saved_messages(&session),
                serde_json::to_value(agent.history()).unwrap()
            );
        }
        assert_eq!(agent.history().len(), 3);
    }

    /// A session stamps its id on the first message of its own conversation:
    /// once on a fresh session, again on the first message a context fork adds
    /// (the inherited one names the parent), never on a resumed turn.
    #[test]
    fn session_stamp_covers_fresh_and_forked_runs_only() {
        let seeded = [Message::UserMessage {
            content: vec![Content::Text {
                text: "inherited".into(),
            }],
        }];
        assert!(needs_session_stamp(&[], /*forked*/ false));
        assert!(needs_session_stamp(&seeded, /*forked*/ true));
        assert!(!needs_session_stamp(&seeded, /*forked*/ false));
    }
}
