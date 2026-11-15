//! Session turns stamp input, recover rejected context, and persist every completed run.

use std::sync::Arc;

use chrono::{DateTime, Utc};

use super::runner::Workflow;
use crate::SessionRuntime;
use crate::agent::{Agent, AgentInteractionError, RunOutcome};
use crate::core::CancelToken;
use crate::generative_model::{Content, Message, Recovery};
use crate::prompts;
use crate::session::{ActiveSession, Session, SessionWriter};

pub struct SessionTurnOutcome {
    pub result: Result<RunOutcome, AgentInteractionError>,
    pub rewound: Option<Vec<Content>>,
}

pub(super) struct Submission {
    pub input: Vec<Content>,
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
        accepted_at,
    } = submission;
    let session = runtime.session();
    let writer = tokio::select! {
        biased;
        writer = session.writer() => writer,
        _ = cancel.cancelled() => return SessionTurnOutcome { result: Err(AgentInteractionError::Cancelled), rewound: None },
    };
    if let Err(error) = workflow
        .retry(runtime, &cancel, || {
            settle_stopped_run(agent).and_then(|()| runtime.bind_agent(agent))
        })
        .await
    {
        return SessionTurnOutcome {
            result: Err(error),
            rewound: None,
        };
    }
    let accepted = accepted_at.map(|time| (agent.history().len(), time));
    wire_checkpoint_at(agent, session, accepted);
    if let Err(error) = workflow
        .retry(runtime, &cancel, || {
            crate::core::image_store::ImageStore::for_profile()
                .and_then(|store| store.externalize(&mut input))
                .map_err(AgentInteractionError::Checkpoint)
        })
        .await
    {
        return SessionTurnOutcome {
            result: Err(error),
            rewound: None,
        };
    }
    if accepted_at.is_some() {
        if let Err(error) = auto_title(session, &input) {
            on_warning(&format!("could not auto-title session: {error}"));
        }
        if let Some(stamp) = session.with(|session| {
            needs_session_stamp(agent.history(), session).then(|| {
                prompts::thread_stamp_part(
                    &session.id,
                    &session.active_thread().id,
                    session.created_at,
                )
            })
        }) {
            input.insert(0, stamp);
        }
    }
    if let Some(notice) = workflow.runtime_notice(agent, runtime).await {
        input.push(notice);
    }
    let result = match workflow
        .checkpointed_update(agent, runtime, &cancel, |agent| {
            agent.append_input(Message::UserMessage {
                content: input.clone(),
            })
        })
        .await
    {
        Ok(()) => {
            workflow
                .drive(agent, runtime, &writer, cancel.clone(), true)
                .await
        }
        Err(error) => Err(error),
    };
    finish_turn(
        agent, runtime, &writer, workflow, &cancel, result, on_warning,
    )
    .await
}

/// A new user action supersedes a stopped run, including one paused by a failed
/// save. Settle its intent before rebinding; never reload over unsaved results.
/// The caller holds the session writer and owns the stopped interpreter.
pub(super) fn settle_stopped_run(agent: &mut Agent) -> Result<(), AgentInteractionError> {
    if !agent.state().is_idle() {
        agent.recover_interrupted()?;
    } else {
        agent.repair_checkpoint()?;
    }
    Ok(())
}

pub(super) async fn finish_turn(
    agent: &mut Agent,
    runtime: &Arc<SessionRuntime>,
    writer: &SessionWriter,
    workflow: &Workflow,
    cancel: &CancelToken,
    result: Result<RunOutcome, AgentInteractionError>,
    on_warning: impl Fn(&str),
) -> SessionTurnOutcome {
    let session = runtime.session();
    if let Err(error) = workflow
        .retry(runtime, cancel, || {
            persist_session(agent, session, true).map_err(AgentInteractionError::Checkpoint)
        })
        .await
    {
        return SessionTurnOutcome {
            result: Err(error),
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
            if matches!(part, Content::System { kind, .. } if kind == "session") {
                *part =
                    prompts::thread_stamp_part(&original.id, &successor.id, original.created_at);
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
    if history.is_empty() && !session.with(|session| session.json_path().exists()) {
        return Ok(());
    }
    agent.checkpoint().map_err(|error| error.to_string())?;
    let thread_id = agent
        .context()
        .thread_id
        .clone()
        .unwrap_or_else(|| session.with(|session| session.active_thread().id.clone()));
    session.persist_agent_state(&thread_id, agent.state(), force, None)
}

/// A session stamps its first user turn. A child that has not stamped its own
/// identity yet (a fork inherits its parent's stamp) stamps the next one.
/// Resumes keep the existing stamp, including one upgraded from a legacy file.
fn needs_session_stamp(history: &[Message], session: &Session) -> bool {
    !history.iter().any(Message::is_user_turn)
        || session.parent_session_id.is_some() && !prompts::has_session_stamp(history, &session.id)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::agent::NullEventSink;
    use crate::chat::{SessionRunner, WorkflowEvent};
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

    fn runner(
        executor: &tokio::runtime::Runtime,
        model: Arc<ScriptedModel>,
        session: &ActiveSession,
    ) -> SessionRunner {
        let runtime = SessionRuntime::new(Harness::local_with_services(vec![]), session.clone());
        let mut runner = executor
            .block_on(SessionRunner::new(agent(model), runtime))
            .unwrap();
        runner.set_observer(Arc::new(|event| {
            if let WorkflowEvent::Warning(warning) = event {
                panic!("{warning}");
            }
        }));
        runner
    }

    fn submit_text(
        executor: &tokio::runtime::Runtime,
        runner: &mut SessionRunner,
        text: &str,
        cancel: CancelToken,
    ) -> SessionTurnOutcome {
        executor.block_on(runner.submit(
            vec![Content::Text { text: text.into() }],
            Utc::now(),
            cancel,
        ))
    }

    fn submit(
        executor: &tokio::runtime::Runtime,
        runner: &mut SessionRunner,
        cancel: CancelToken,
    ) -> SessionTurnOutcome {
        submit_text(executor, runner, "task", cancel)
    }

    fn saved_messages(session: &ActiveSession) -> serde_json::Value {
        let saved = Session::load(&session.snapshot().json_path()).unwrap();
        serde_json::to_value(&saved.active_thread().messages).unwrap()
    }

    #[test]
    fn failed_input_save_stops_generation_and_preserves_input_for_the_live_runtime() {
        let home = temp_home("input-save-failure");
        let session = ActiveSession::new(Session::new("test"));
        let model = ScriptedModel::new(vec![GenerateOutput {
            content: vec![],
            tool_uses: vec![],
            turn_end_reason: TurnEndReason::EndTurn,
            usage: None,
        }]);
        let executor = tokio::runtime::Runtime::new().unwrap();
        let mut runner = runner(&executor, model.clone(), &session);
        runner.set_observer(Arc::new(|_| {}));
        std::fs::write(home.path().join("session"), "blocks writes").unwrap();
        let outcome = submit_text(
            &executor,
            &mut runner,
            "first accepted input",
            CancelToken::new(),
        );
        assert!(matches!(
            outcome.result,
            Err(AgentInteractionError::Checkpoint(_))
        ));
        assert_eq!(model.remaining(), 1);
        assert!(session.snapshot().active_thread().messages.is_empty());
        assert!(session.snapshot().title.is_none());
        std::fs::remove_file(home.path().join("session")).unwrap();
        submit_text(
            &executor,
            &mut runner,
            "continue after storage repair",
            CancelToken::new(),
        )
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
        let executor = tokio::runtime::Runtime::new().unwrap();
        let mut runner = runner(
            &executor,
            ScriptedModel::new(vec![]).then_fail(GenerateError::ExecutionError("offline".into())),
            &session,
        );
        let before = Utc::now();
        submit(&executor, &mut runner, CancelToken::new());
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
        let session = ActiveSession::new(document);
        let executor = tokio::runtime::Runtime::new().unwrap();
        let mut runner = runner(
            &executor,
            ScriptedModel::new(vec![])
                .then_fail(GenerateError::RequestTooLargeError("oversized".into())),
            &session,
        );
        let outcome = submit(&executor, &mut runner, CancelToken::new());
        let agent = runner.agent();
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
                thought_signature: None,
            }],
            turn_end_reason: TurnEndReason::ToolUse,
            usage: None,
        }]).then_fail(GenerateError::RequestTooLargeError("image dimensions".into()));
        let session = ActiveSession::new(Session::new("test"));
        let executor = tokio::runtime::Runtime::new().unwrap();
        let mut runner = runner(&executor, model, &session);
        assert!(
            submit(&executor, &mut runner, CancelToken::new())
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
            submit(&executor, &mut runner, CancelToken::new())
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
        let executor = tokio::runtime::Runtime::new().unwrap();
        let mut runner = runner(&executor, ScriptedModel::new(vec![]), &session);
        let cancel = CancelToken::new();
        cancel.cancel();
        let outcome = submit(&executor, &mut runner, cancel);
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
            serde_json::to_value(runner.agent().history()).unwrap()
        );
    }

    #[test]
    fn successful_and_failed_turns_both_persist_the_current_history() {
        let _home = temp_home("chat-completion");
        let session = ActiveSession::new(Session::new("test"));
        let executor = tokio::runtime::Runtime::new().unwrap();
        let mut runner = runner(
            &executor,
            ScriptedModel::new(vec![GenerateOutput {
                content: vec![Content::Text {
                    text: "answer".into(),
                }],
                tool_uses: vec![],
                turn_end_reason: TurnEndReason::EndTurn,
                usage: None,
            }])
            .then_fail(GenerateError::ExecutionError("unavailable".into())),
            &session,
        );
        for succeeds in [true, false] {
            let outcome = submit(&executor, &mut runner, CancelToken::new());
            assert_eq!(outcome.result.is_ok(), succeeds);
            assert!(outcome.rewound.is_none());
            assert_eq!(
                saved_messages(&session),
                serde_json::to_value(runner.agent().history()).unwrap()
            );
        }
        assert_eq!(runner.agent().history().len(), 3);
    }

    #[test]
    fn queued_turn_uses_the_successor_thread_after_compaction_finishes() {
        let _home = temp_home("thread-queued-turn");
        let executor = tokio::runtime::Runtime::new().unwrap();
        let mut document = Session::new("m");
        document.replace_context(vec![user("earlier"), assistant("answer")], None);
        let session = ActiveSession::new(document);
        let model = ScriptedModel::new(vec![GenerateOutput {
            content: vec![],
            tool_uses: vec![],
            turn_end_reason: TurnEndReason::EndTurn,
            usage: None,
        }]);
        let mut runner = runner(&executor, model, &session);
        executor.block_on(async {
            let writer = session.writer().await;
            {
                let run = runner.submit(vec![], Utc::now(), CancelToken::new());
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
                runner.agent().context().thread_id.as_deref(),
                Some(saved.active_thread().id.as_str())
            );
            assert!(
                matches!(&runner.agent().history()[0], Message::UserMessage { content }
                if matches!(&content[1], Content::System { text, .. } if text.contains("compacted goal")))
            );
        });
    }

    #[test]
    fn cancellation_while_waiting_for_the_writer_does_not_submit_input() {
        let _home = temp_home("thread-queued-cancel");
        let executor = tokio::runtime::Runtime::new().unwrap();
        let session = ActiveSession::new(Session::new("m"));
        let mut runner = runner(&executor, ScriptedModel::new(vec![]), &session);
        executor.block_on(async {
            let _writer = session.writer().await;
            let cancel = CancelToken::new();
            cancel.cancel();
            let outcome = runner.submit(vec![], Utc::now(), cancel).await;
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
    /// (the inherited one names the parent), never on a resumed turn. A legacy
    /// stamp names no session, so only a child restamps after one.
    #[test]
    fn session_stamp_covers_fresh_and_forked_runs_only() {
        let parent = Session::new("test");
        let stamped = |stamp: Content| {
            vec![Message::UserMessage {
                content: vec![
                    stamp,
                    Content::Text {
                        text: "task".into(),
                    },
                ],
            }]
        };
        let own = |session: &Session| {
            stamped(prompts::thread_stamp_part(
                &session.id,
                &session.active_thread().id,
                session.created_at,
            ))
        };
        let legacy = stamped(Content::System {
            kind: "session".into(),
            text: "# Session".into(),
            data: serde_json::json!({"legacy": true}),
        });
        let child = parent.fork_child("test");

        assert!(needs_session_stamp(&[], &parent));
        assert!(needs_session_stamp(&[], &child));
        assert!(!needs_session_stamp(&own(&parent), &parent));
        assert!(!needs_session_stamp(&legacy, &parent));
        assert!(needs_session_stamp(&own(&parent), &child));
        assert!(needs_session_stamp(&legacy, &child));
        let mut forked = own(&parent);
        forked.extend(own(&child));
        assert!(!needs_session_stamp(&forked, &child));
    }
}
