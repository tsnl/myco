//! Session turns stamp input, recover rejected context, and persist every completed run.

use std::sync::Arc;

use chrono::{DateTime, Utc};

use super::runner::Workflow;
use crate::SessionRuntime;
use crate::agent::{Agent, AgentInteractionError, RunOutcome};
use crate::core::CancelToken;
use crate::generative_model::{Content, Message, Recovery};
use crate::prompts;
use crate::session::{ActiveSession, SessionWriter};

pub struct SessionTurnOutcome {
    pub result: Result<RunOutcome, AgentInteractionError>,
    pub rewound: Option<Vec<Content>>,
}

/// Submit already-expanded input. State persistence failures stop execution;
/// nonfatal metadata/recovery failures reach `on_warning`.
/// The caller keeps ownership of the agent, session lock, and cancellation source.
pub async fn run_session_turn(
    agent: &mut Agent,
    runtime: &Arc<SessionRuntime>,
    input: Vec<Content>,
    forked: bool,
    cancel: CancelToken,
    accepted_at: DateTime<Utc>,
    on_warning: impl Fn(&str) + Send + Sync + 'static,
) -> SessionTurnOutcome {
    run_turn(
        agent,
        runtime,
        Submission {
            input,
            forked,
            accepted_at: Some(accepted_at),
        },
        &mut Workflow::default(),
        cancel,
        on_warning,
    )
    .await
}

pub(super) struct Submission {
    pub input: Vec<Content>,
    pub forked: bool,
    pub accepted_at: Option<DateTime<Utc>>,
}

pub(super) async fn run_turn(
    agent: &mut Agent,
    runtime: &Arc<SessionRuntime>,
    submission: Submission,
    workflow: &mut Workflow,
    cancel: CancelToken,
    on_warning: impl Fn(&str) + Send + Sync,
) -> SessionTurnOutcome {
    let Submission {
        mut input,
        forked,
        accepted_at,
    } = submission;
    let session = runtime.session();
    let writer = tokio::select! {
        biased;
        writer = session.writer() => writer,
        _ = cancel.cancelled() => return SessionTurnOutcome { result: Err(AgentInteractionError::Cancelled), rewound: None },
    };
    if let Err(error) = runtime.bind_agent(agent) {
        return SessionTurnOutcome {
            result: Err(error),
            rewound: None,
        };
    }
    let accepted = accepted_at.map(|time| (agent.history().len(), time));
    wire_checkpoint_at(agent, session, accepted);
    if let Err(error) = crate::core::image_store::ImageStore::for_profile()
        .and_then(|store| store.externalize(&mut input))
    {
        return SessionTurnOutcome {
            result: Err(AgentInteractionError::Checkpoint(error)),
            rewound: None,
        };
    }
    if accepted_at.is_some() {
        if let Err(error) = auto_title(session, &input) {
            on_warning(&format!("could not auto-title session: {error}"));
        }
        if needs_session_stamp(agent.history(), forked) {
            stamp_input(session, &mut input);
        }
    }
    if let Some(notice) = workflow.runtime_notice(agent, runtime).await {
        input.push(notice);
    }
    let result = match agent.append_input(Message::UserMessage { content: input }) {
        Ok(()) => workflow.drive(agent, runtime, &writer, cancel, true).await,
        Err(error) => Err(error),
    };
    finish_turn(agent, runtime, &writer, result, on_warning)
}

pub(super) fn finish_turn(
    agent: &mut Agent,
    runtime: &Arc<SessionRuntime>,
    writer: &SessionWriter,
    result: Result<RunOutcome, AgentInteractionError>,
    on_warning: impl Fn(&str),
) -> SessionTurnOutcome {
    let session = runtime.session();
    if let Err(error) = persist_session(agent, session, true) {
        return SessionTurnOutcome {
            result: Err(AgentInteractionError::Checkpoint(error)),
            rewound: None,
        };
    }
    let rewound = match rewind_rejected_input(agent, runtime, writer, &result) {
        Ok(rewound) => rewound,
        Err(error) => {
            on_warning(&format!(
                "could not recover rejected input; original history retained: {error}"
            ));
            None
        }
    };
    if rewound.is_some() {
        wire_checkpoint(agent, session);
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
    let text = session.with(|session| {
        prompts::thread_stamp(&session.id, &session.active_thread().id, session.created_at)
    });
    input.insert(
        0,
        Content::System {
            kind: "session".into(),
            text,
            data: session.with(|session| {
                serde_json::json!({
                    "session_id":session.id, "thread_id":session.active_thread().id,
                    "created_at":session.created_at,
                })
            }),
        },
    );
}

fn rewind_rejected_input(
    agent: &mut Agent,
    runtime: &Arc<SessionRuntime>,
    writer: &SessionWriter,
    result: &Result<RunOutcome, AgentInteractionError>,
) -> Result<Option<Vec<Content>>, String> {
    if !matches!(result, Err(error) if error.recovery() == Recovery::OmitLastMessage) {
        return Ok(None);
    }
    let Some(index) = agent.history().iter().rposition(Message::is_user_turn) else {
        return Ok(None);
    };
    let Message::UserMessage { content } = &agent.history()[index] else {
        unreachable!()
    };
    let dropped = content
        .iter()
        .filter(|part| !matches!(part, Content::System { .. }))
        .cloned()
        .collect();
    let original = runtime.session().snapshot();
    let mut successor = original.active_thread().clone();
    successor.id = uuid::Uuid::new_v4().as_simple().to_string();
    successor.created_at = chrono::Utc::now();
    successor.predecessor_id = Some(original.active_thread().id.clone());
    successor.messages = agent.history()[..index].to_vec();
    if let Some(part) = crate::core::latest_runtime_part(agent.history())
        && crate::core::latest_runtime_part(&successor.messages) != Some(part)
    {
        successor.messages.push(Message::UserMessage {
            content: vec![part.clone()],
        });
    }
    if agent.history()[index..]
        .iter()
        .any(|message| matches!(message, Message::ToolResults { .. }))
    {
        successor.messages.push(Message::UserMessage { content: vec![Content::System {
            kind: "recovery".into(),
            text: format!("The latest submission was removed from model context after a size rejection. Tools had already returned results in predecessor thread {} of session {}. Their external effects were not undone. Inspect those observations with session_history before repeating actions.", original.active_thread().id, original.id),
            data: serde_json::json!({"reason":"rejected_input", "predecessor_id":original.active_thread().id, "from_index":index}),
        }] });
    }
    successor.user_turn_timestamps.retain(|&key, _| key < index);
    successor.last_usage = None;
    successor.context_tokens_estimate = None;
    if let Some(Message::UserMessage { content }) = successor.messages.first_mut() {
        for part in content {
            if let Content::System { kind, text, data } = part
                && kind == "session"
            {
                *text = prompts::thread_stamp(&original.id, &successor.id, original.created_at);
                *data = serde_json::json!({
                    "session_id": original.id, "thread_id": successor.id,
                    "created_at": original.created_at,
                });
            }
        }
    }
    // Commit the repaired context before installing it; the predecessor retains
    // every observation, including actions taken during the rejected turn.
    writer.commit_thread(successor)?;
    runtime
        .bind_agent(agent)
        .map_err(|error| error.to_string())?;
    Ok(Some(dropped))
}

/// Persist every effect boundary. A failed save stops the agent before more work.
pub fn wire_checkpoint(agent: &mut Agent, session: &ActiveSession) {
    wire_checkpoint_at(agent, session, None);
}

fn wire_checkpoint_at(
    agent: &mut Agent,
    session: &ActiveSession,
    accepted: Option<(usize, DateTime<Utc>)>,
) {
    let thread_id = session.with(|session| session.active_thread().id.clone());
    let epoch = session.begin_checkpoints();
    let session = session.clone();
    agent.set_checkpoint(Some(Box::new(move |state| {
        if !session.checkpoint_is_current(epoch) {
            return Err(
                "agent checkpoint was superseded by another session writer; reload saved context"
                    .into(),
            );
        }
        session.persist_agent_state(&thread_id, state, false, accepted)
    })));
}

/// Save current context, including an empty rewind of an existing session.
/// A session that never accepted input does not create a file.
pub fn persist_session(agent: &Agent, session: &ActiveSession, force: bool) -> Result<(), String> {
    let history = agent.history();
    if history.is_empty() && !session.snapshot().json_path().exists() {
        return Ok(());
    }
    agent.checkpoint().map_err(|error| error.to_string())?;
    let thread_id = agent
        .context()
        .thread_id
        .clone()
        .unwrap_or_else(|| session.snapshot().active_thread().id.clone());
    session.persist_agent_state(&thread_id, agent.state(), force, None)
}

// Forks inherit a parent's stamp and need their own; resumes keep the existing one.
fn needs_session_stamp(history: &[Message], forked: bool) -> bool {
    !history.iter().any(Message::is_user_turn) || forked
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
            crate::test_support::tool_runtime(Harness::local_with_services(vec![])),
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
                &SessionRuntime::new(Harness::local_with_services(vec![]), session.clone()),
                vec![Content::Text {
                    text: "task".into(),
                }],
                false,
                cancel,
                chrono::Utc::now(),
                |warning| panic!("{warning}"),
            ))
    }

    fn saved_messages(session: &ActiveSession) -> serde_json::Value {
        let saved = Session::load(&session.snapshot().json_path()).unwrap();
        serde_json::to_value(&saved.active_thread().messages).unwrap()
    }

    #[test]
    fn failed_input_save_stops_generation_and_preserves_input_for_the_live_runtime() {
        let home = temp_home("input-save-failure");
        let session = ActiveSession::new(Session::new("test"));
        let runtime = SessionRuntime::new(Harness::local_with_services(vec![]), session.clone());
        let model = ScriptedModel::new(vec![GenerateOutput {
            content: vec![],
            tool_uses: vec![],
            turn_end_reason: TurnEndReason::EndTurn,
            usage: None,
        }]);
        let mut agent = agent(model.clone());
        std::fs::write(home.path().join("session"), "blocks writes").unwrap();
        let executor = tokio::runtime::Runtime::new().unwrap();
        let outcome = executor.block_on(run_session_turn(
            &mut agent,
            &runtime,
            vec![Content::Text {
                text: "first accepted input".into(),
            }],
            false,
            CancelToken::new(),
            Utc::now(),
            |_| {},
        ));
        assert!(matches!(
            outcome.result,
            Err(AgentInteractionError::Checkpoint(_))
        ));
        assert_eq!(model.remaining(), 1);
        assert!(session.snapshot().active_thread().messages.is_empty());
        assert!(session.snapshot().title.is_none());
        std::fs::remove_file(home.path().join("session")).unwrap();
        executor
            .block_on(run_session_turn(
                &mut agent,
                &runtime,
                vec![Content::Text {
                    text: "continue after storage repair".into(),
                }],
                false,
                CancelToken::new(),
                Utc::now(),
                |warning| panic!("{warning}"),
            ))
            .result
            .unwrap();
        let saved = Session::load(&session.snapshot().json_path()).unwrap();
        let messages = &saved.active_thread().messages;
        assert_eq!(messages.len(), 3);
        assert!(
            serde_json::to_string(&messages[0])
                .unwrap()
                .contains("first accepted input")
        );
        assert!(saved.active_thread().pending_operation.is_none());
    }

    #[test]
    fn user_turn_acceptance_times_survive_restart_and_context_forks() {
        let _home = temp_home("turn-timestamps");
        let session = ActiveSession::new(Session::new("test"));
        let mut agent = agent(
            ScriptedModel::new(vec![]).then_fail(GenerateError::ExecutionError("offline".into())),
        );
        let before = Utc::now();
        submit(&mut agent, &session, CancelToken::new());
        let after = Utc::now();
        let saved = Session::load(&session.snapshot().json_path()).unwrap();
        let time = saved.active_thread().user_turn_timestamps[&0];
        assert!(time >= before && time <= after);
        assert_eq!(
            saved
                .fork_child("test")
                .active_thread()
                .user_turn_timestamps[&0],
            time
        );
        let legacy = Session::from_json(include_bytes!(
            "../../tests/fixtures/session_v2_all_variants.json"
        ))
        .unwrap();
        assert!(legacy.active_thread().user_turn_timestamps.is_empty());
    }

    #[test]
    fn rejected_input_is_preserved_in_a_predecessor_before_context_is_rewound() {
        let _home = temp_home("chat-rejection");
        let mut document = Session::new("test");
        document.active_thread_mut().messages = vec![user("earlier"), assistant("answer")];
        let earlier_time = Utc::now() - chrono::Duration::minutes(5);
        document
            .active_thread_mut()
            .user_turn_timestamps
            .insert(0, earlier_time);
        let expected = serde_json::to_value(&document.active_thread().messages).unwrap();
        let mut agent = agent(
            ScriptedModel::new(vec![])
                .then_fail(GenerateError::RequestTooLargeError("oversized".into())),
        );
        agent
            .replace_context(document.active_thread().messages.clone(), None)
            .unwrap();
        let session = ActiveSession::new(document);
        wire_checkpoint(&mut agent, &session);
        let outcome = submit(&mut agent, &session, CancelToken::new());
        assert!(outcome.result.is_err());
        assert!(
            matches!(outcome.rewound.as_deref(), Some([Content::Text { text }]) if text == "task")
        );
        assert_eq!(
            serde_json::to_value(&agent.history()[..2]).unwrap(),
            expected
        );
        assert!(crate::RuntimeRecord::latest(agent.history()).is_some());
        assert_eq!(
            saved_messages(&session),
            serde_json::to_value(agent.history()).unwrap()
        );
        let saved = Session::load(&session.snapshot().json_path()).unwrap();
        assert_eq!(saved.threads().len(), 2);
        assert_eq!(saved.threads()[0].messages.len(), 3);
        assert_eq!(saved.threads()[0].user_turn_timestamps[&0], earlier_time);
        assert!(saved.threads()[0].user_turn_timestamps[&2] > earlier_time);
        assert_eq!(saved.active_thread().user_turn_timestamps.len(), 1);
        assert_eq!(saved.active_thread().user_turn_timestamps[&0], earlier_time);
        assert!(
            matches!(&saved.threads()[0].messages[2], Message::UserMessage { content }
            if matches!(&content[0], Content::Text { text } if text == "task"))
        );
        assert_eq!(
            saved.active_thread().predecessor_id.as_deref(),
            Some(saved.threads()[0].id.as_str())
        );
    }

    #[test]
    fn recovery_keeps_completed_tool_actions_without_reexecuting_them() {
        let home = temp_home("recovery-actions");
        let effect = home.path().join("effect");
        let model = ScriptedModel::new(vec![GenerateOutput {
            content: vec![],
            tool_uses: vec![crate::generative_model::ToolUse {
                name: "bash".into(),
                input: serde_json::json!({"command": format!("printf x >> '{}'", effect.display())}),
            }],
            turn_end_reason: TurnEndReason::ToolUse,
            usage: None,
        }]).then_fail(GenerateError::RequestTooLargeError("image dimensions".into()));
        let session = ActiveSession::new(Session::new("test"));
        let mut agent = agent(model);
        assert!(
            submit(&mut agent, &session, CancelToken::new())
                .rewound
                .is_some()
        );
        let saved = Session::load(&session.snapshot().json_path()).unwrap();
        assert!(matches!(
            saved.threads()[0].messages.last(),
            Some(Message::ToolResults { .. })
        ));
        assert!(
            !saved
                .active_thread()
                .messages
                .iter()
                .any(Message::is_user_turn)
        );
        assert!(
            serde_json::to_string(&saved.active_thread().messages)
                .unwrap()
                .contains("external effects were not undone")
        );
        assert!(saved.active_thread().user_turn_timestamps.is_empty());
        assert_eq!(saved.threads()[0].user_turn_timestamps.len(), 1);
        let original = serde_json::to_value(&saved.threads()[0]).unwrap();
        assert!(
            submit(&mut agent, &session, CancelToken::new())
                .rewound
                .is_some()
        );
        assert_eq!(std::fs::read_to_string(effect).unwrap(), "x");
        assert_eq!(
            serde_json::to_value(&session.snapshot().threads()[0]).unwrap(),
            original
        );
    }

    #[test]
    fn failed_recovery_commit_keeps_the_original_context_active() {
        let home = temp_home("recovery-save-failure");
        let mut document = Session::new("test");
        document.replace_context(vec![user("rejected")], None);
        let original_id = document.active_thread().id.clone();
        let session = ActiveSession::new(document);
        let runtime = SessionRuntime::new(Harness::local_with_services(vec![]), session.clone());
        let mut agent = agent(ScriptedModel::new(vec![]));
        runtime.bind_agent(&mut agent).unwrap();
        std::fs::write(home.path().join("session"), "blocks writes").unwrap();
        let executor = tokio::runtime::Runtime::new().unwrap();
        let writer = executor.block_on(session.writer());
        let result = Err(AgentInteractionError::GenerateError(
            GenerateError::RequestTooLargeError("too big".into()),
        ));
        assert!(rewind_rejected_input(&mut agent, &runtime, &writer, &result).is_err());
        assert_eq!(session.snapshot().active_thread().id, original_id);
        assert_eq!(agent.history().len(), 1);
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
            matches!(&saved.active_thread().messages[0], Message::UserMessage { content }
            if matches!(&content[0], Content::System { text, .. } if text.contains(&saved.id)))
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

    #[test]
    fn queued_turn_uses_the_successor_thread_after_compaction_finishes() {
        let _home = temp_home("thread-queued-turn");
        let executor = tokio::runtime::Runtime::new().unwrap();
        executor.block_on(async {
            let mut document = Session::new("m");
            document.replace_context(vec![user("earlier"), assistant("answer")], None);
            let session = ActiveSession::new(document);
            let writer = session.writer().await;
            let mut agent = agent(ScriptedModel::new(vec![GenerateOutput {
                content: vec![],
                tool_uses: vec![],
                turn_end_reason: TurnEndReason::EndTurn,
                usage: None,
            }]));
            {
                let runtime =
                    SessionRuntime::new(Harness::local_with_services(vec![]), session.clone());
                let run = run_session_turn(
                    &mut agent,
                    &runtime,
                    vec![],
                    false,
                    CancelToken::new(),
                    chrono::Utc::now(),
                    |warning| panic!("{warning}"),
                );
                let mut run = std::pin::pin!(run);
                assert!(futures::poll!(&mut run).is_pending());
                let (next, _) =
                    crate::session::compact_thread(&session.snapshot(), "compacted goal").unwrap();
                writer.commit_thread(next).unwrap();
                drop(writer);
                run.await.result.unwrap();
            }
            let saved = session.snapshot();
            assert_eq!(saved.threads().len(), 2);
            assert_eq!(
                agent.context().thread_id.as_deref(),
                Some(saved.active_thread().id.as_str())
            );
            assert!(
                matches!(&agent.history()[0], Message::UserMessage { content }
                if matches!(&content[1], Content::System { text, .. } if text.contains("compacted goal")))
            );
        });
    }

    #[test]
    fn cancellation_while_waiting_for_the_writer_does_not_submit_input() {
        let _home = temp_home("thread-queued-cancel");
        let executor = tokio::runtime::Runtime::new().unwrap();
        executor.block_on(async {
            let session = ActiveSession::new(Session::new("m"));
            let _writer = session.writer().await;
            let mut agent = agent(ScriptedModel::new(vec![]));
            let cancel = CancelToken::new();
            cancel.cancel();
            let outcome = run_session_turn(
                &mut agent,
                &SessionRuntime::new(Harness::local_with_services(vec![]), session.clone()),
                vec![],
                false,
                cancel,
                chrono::Utc::now(),
                |warning| panic!("{warning}"),
            )
            .await;
            assert!(matches!(
                outcome.result,
                Err(AgentInteractionError::Cancelled)
            ));
            assert!(session.snapshot().active_thread().messages.is_empty());
            assert!(!session.snapshot().json_path().exists());
        });
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
