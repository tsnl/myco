use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::*;
use crate::agent::NullEventSink;
use crate::chat::SessionRunner;
use crate::generative_model::{GenerateError, GenerateOutput, Message, ToolUse};
use crate::session::Session;
use crate::test_support::{ScriptedModel, temp_home};
use crate::{Harness, SessionMetaTool, SessionRuntime};

fn response(reason: TurnEndReason, disable: bool) -> GenerateOutput {
    GenerateOutput {
        content: vec![Content::Text {
            text: "progress".into(),
        }],
        tool_uses: if disable {
            vec![ToolUse {
                name: "session_meta".into(),
                input: serde_json::json!({"action":"disable_auto_continue"}),
            }]
        } else {
            vec![]
        },
        turn_end_reason: reason,
        usage: None,
    }
}

async fn runner(scripts: Vec<GenerateOutput>) -> (SessionRunner, Arc<ScriptedModel>) {
    let active = ActiveSession::new(Session::new("test"));
    let harness =
        Harness::local_with_services(vec![Arc::new(SessionMetaTool::new(active.clone()))]);
    let runtime = SessionRuntime::new(harness, active);
    let model = ScriptedModel::new(scripts);
    let agent = Agent::new(model.clone(), runtime.clone(), Arc::new(NullEventSink));
    (SessionRunner::new(agent, runtime).await.unwrap(), model)
}

async fn submit(
    runner: &mut SessionRunner,
    cancel: CancelToken,
) -> crate::chat::SessionTurnOutcome {
    runner
        .submit(
            vec![Content::Text {
                text: "finish the task".into(),
            }],
            chrono::Utc::now(),
            cancel,
        )
        .await
}

#[test]
fn auto_continue_is_opt_in_and_agent_disable_is_durable() {
    let _home = temp_home("auto-continue-opt-in");
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let scripts = vec![
            response(TurnEndReason::EndTurn, false),
            response(TurnEndReason::ToolUse, true),
            response(TurnEndReason::EndTurn, false),
        ];
        let (mut off, off_model) = runner(scripts.clone()).await;
        submit(&mut off, CancelToken::new()).await.result.unwrap();
        assert_eq!(off_model.remaining(), 2);

        let (mut enabled, model) = runner(scripts).await;
        let session = enabled.runtime().session().clone();
        session.set_auto_continue(true).unwrap();
        submit(&mut enabled, CancelToken::new())
            .await
            .result
            .unwrap();
        assert_eq!(model.remaining(), 0);
        let saved = Session::load(&session.snapshot().json_path()).unwrap();
        assert!(!saved.auto_continue);
        assert_eq!(saved.active_thread().user_turn_timestamps.len(), 1);
        assert_eq!(
            saved
                .active_thread()
                .messages
                .iter()
                .filter(|message| message.is_user_turn())
                .count(),
            1
        );
        assert!(
            saved
                .active_thread()
                .messages
                .iter()
                .any(|message| matches!(message,
            Message::UserMessage { content } if content.iter().any(|part| matches!(part,
                Content::System { data, .. } if data["reason"] == "auto_continue"))))
        );
    });
}

#[test]
fn auto_continue_handles_output_caps_and_respects_cancellation() {
    let _home = temp_home("auto-continue-stops");
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let (mut capped, model) = runner(vec![
            response(TurnEndReason::MaxTokens, false),
            response(TurnEndReason::ToolUse, true),
            response(TurnEndReason::EndTurn, false),
        ])
        .await;
        capped.agent_mut().set_max_truncated_resumes(0);
        capped.runtime().session().set_auto_continue(true).unwrap();
        submit(&mut capped, CancelToken::new())
            .await
            .result
            .unwrap();
        assert_eq!(model.remaining(), 0);

        let (mut failed, _) = runner(vec![]).await;
        failed
            .agent_mut()
            .set_model(ScriptedModel::from_results(vec![Err(
                GenerateError::RefusalError("cannot continue".into()),
            )]));
        // The default mode retains its bounded error policy.
        assert!(
            submit(&mut failed, CancelToken::new())
                .await
                .result
                .is_err()
        );
        failed.runtime().session().set_auto_continue(true).unwrap();
        let cancel = CancelToken::new();
        cancel.cancel();
        assert!(matches!(
            submit(&mut failed, cancel).await.result,
            Err(AgentInteractionError::Cancelled)
        ));
    });
}

#[test]
fn persisted_mode_survives_restart_and_compaction_without_spreading_to_children() {
    let _home = temp_home("auto-continue-persistence");
    let active = ActiveSession::new(Session::new("test"));
    active
        .with_mut(|session| session.replace_context(vec![crate::test_support::user("task")], None));
    active.set_auto_continue(true).unwrap();
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let (successor, _) =
            crate::session::compact_thread(&active.snapshot(), "Continue task").unwrap();
        active.writer().await.commit_thread(successor).unwrap();
    });
    let saved = Session::load(&active.snapshot().json_path()).unwrap();
    assert!(saved.auto_continue);
    assert_eq!(saved.threads().len(), 2);
    assert!(!saved.fork_child("test").auto_continue);
    let mut old = serde_json::to_value(&saved).unwrap();
    old.as_object_mut().unwrap().remove("auto_continue");
    assert!(
        !serde_json::from_value::<Session>(old)
            .unwrap()
            .auto_continue
    );
}

#[test]
fn failed_mode_save_leaves_live_setting_unchanged() {
    let home = temp_home("auto-continue-failed-save");
    let active = ActiveSession::new(Session::new("test"));
    active.set_auto_continue(true).unwrap();
    std::fs::rename(
        home.path().join("session"),
        home.path().join("saved-session"),
    )
    .unwrap();
    std::fs::write(home.path().join("session"), "unavailable").unwrap();
    assert!(active.set_auto_continue(false).is_err());
    assert!(active.with(|session| session.auto_continue));
}

#[test]
fn disabling_during_a_generation_prevents_the_next_automatic_prompt() {
    let _home = temp_home("auto-continue-live-disable");
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let (mut runner, model) = runner(vec![response(TurnEndReason::EndTurn, false)]).await;
        let session = runner.runtime().session().clone();
        session.set_auto_continue(true).unwrap();
        let observed = session.clone();
        runner
            .agent_mut()
            .set_before_generation_notice(Some(Box::new(move |_, _| {
                observed.set_auto_continue(false).unwrap();
                Box::pin(async { None })
            })));
        submit(&mut runner, CancelToken::new())
            .await
            .result
            .unwrap();
        assert_eq!(model.remaining(), 0);
        assert!(!session.with(|session| session.auto_continue));
    });
}

//
// Automatic error recovery
//

fn paused_runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .start_paused(true)
        .build()
        .unwrap()
}

struct RecordedModel {
    model: Arc<ScriptedModel>,
    inputs: Mutex<Vec<Vec<Message>>>,
}

impl crate::generative_model::GenerativeModel for RecordedModel {
    fn generate(
        &self,
        input: &[Message],
    ) -> crate::core::AsyncStream<crate::generative_model::GenerationEvent> {
        self.inputs.lock().unwrap().push(input.to_vec());
        self.model.generate(input)
    }
}

#[test]
fn automatic_errors_retry_indefinitely_with_bounded_delays_and_the_same_input() {
    let _home = temp_home("auto-errors-backoff");
    paused_runtime().block_on(async {
        let (mut runner, _) = runner(vec![]).await;
        let mut responses = vec![Err(GenerateError::RefusalError("temporarily refused".into())); 7];
        responses.extend([
            Ok(response(TurnEndReason::ToolUse, true)),
            Ok(response(TurnEndReason::EndTurn, false)),
        ]);
        let model = Arc::new(RecordedModel {
            model: ScriptedModel::from_results(responses),
            inputs: Mutex::new(vec![]),
        });
        runner.agent_mut().set_model(model.clone());
        let session = runner.runtime().session().clone();
        session.set_auto_continue(true).unwrap();
        let delays = Arc::new(Mutex::new(vec![]));
        runner.set_observer(Arc::new({
            let delays = delays.clone();
            move |event| {
                if let WorkflowEvent::Retrying { error, delay } = event {
                    assert!(error.contains("temporarily refused"));
                    delays.lock().unwrap().push(delay.as_secs());
                }
            }
        }));
        let began = tokio::time::Instant::now();
        let outcome = submit(&mut runner, CancelToken::new()).await;
        outcome.result.unwrap();
        assert!(outcome.rewound.is_none());
        assert_eq!(*delays.lock().unwrap(), [1, 2, 4, 5, 5, 5, 5]);
        assert_eq!(began.elapsed(), Duration::from_secs(27));
        let inputs = model.inputs.lock().unwrap();
        assert_eq!(inputs.len(), 9);
        assert!(inputs[..8].iter().all(|input| input == &inputs[0]));
        let saved = Session::load(&session.snapshot().json_path()).unwrap();
        assert_eq!(saved.active_thread().user_turn_timestamps.len(), 1);
        assert_eq!(
            runner
                .agent()
                .history()
                .iter()
                .filter(|message| message.is_user_turn())
                .count(),
            1
        );
        assert_eq!(
            runner
                .agent()
                .history()
                .iter()
                .filter(|message| matches!(message,
            Message::UserMessage { content } if content.iter().any(|part| matches!(part,
                Content::System { data, .. } if data["reason"] == "auto_continue_enabled"))))
                .count(),
            1
        );
    });
}

#[test]
fn cancellation_or_live_disable_interrupts_retry_without_another_request() {
    let _home = temp_home("auto-errors-stop");
    paused_runtime().block_on(async {
        for disable in [false, true] {
            let (mut runner, _) = runner(vec![]).await;
            let model = ScriptedModel::from_results(vec![
                Err(GenerateError::RefusalError("unavailable".into())),
                Ok(response(TurnEndReason::EndTurn, false)),
            ]);
            runner.agent_mut().set_model(model.clone());
            let session = runner.runtime().session().clone();
            session.set_auto_continue(true).unwrap();
            let cancel = CancelToken::new();
            runner.set_observer(Arc::new({
                let cancel = cancel.clone();
                move |event| {
                    if matches!(event, WorkflowEvent::Retrying { .. }) {
                        if disable { session.set_auto_continue(false).unwrap(); }
                        else { cancel.cancel(); }
                    }
                }
            }));
            let began = tokio::time::Instant::now();
            let outcome = submit(&mut runner, cancel).await;
            assert!(matches!(outcome.result,
                Err(AgentInteractionError::GenerateError(GenerateError::RefusalError(_))) if disable)
                || matches!(outcome.result, Err(AgentInteractionError::Cancelled)) && !disable);
            assert!(began.elapsed() < Duration::from_secs(1));
            assert_eq!(model.remaining(), 1);
        }
    });
}

#[test]
fn failed_input_saves_retry_without_duplicating_input_or_acceptance_time() {
    let home = temp_home("auto-errors-input-save");
    paused_runtime().block_on(async {
        let (mut runner, model) = runner(vec![
            response(TurnEndReason::ToolUse, true),
            response(TurnEndReason::EndTurn, false),
        ])
        .await;
        let session = runner.runtime().session().clone();
        session.set_auto_continue(true).unwrap();
        let path = home.path().to_owned();
        std::fs::rename(path.join("session"), path.join("saved-session")).unwrap();
        std::fs::write(path.join("session"), "storage unavailable").unwrap();
        let delays = Arc::new(Mutex::new(vec![]));
        runner.set_observer(Arc::new({
            let delays = delays.clone();
            let model = model.clone();
            move |event| {
                if let WorkflowEvent::Retrying { delay, .. } = event {
                    assert_eq!(model.remaining(), 2, "model must wait for input durability");
                    let mut delays = delays.lock().unwrap();
                    delays.push(delay.as_secs());
                    if delays.len() == 3 {
                        std::fs::remove_file(path.join("session")).unwrap();
                        std::fs::rename(path.join("saved-session"), path.join("session")).unwrap();
                    }
                }
            }
        }));
        let accepted = chrono::Utc::now();
        runner
            .submit(
                vec![Content::Text {
                    text: "keep this exact task".into(),
                }],
                accepted,
                CancelToken::new(),
            )
            .await
            .result
            .unwrap();
        assert_eq!(*delays.lock().unwrap(), [1, 2, 4]);
        let saved = Session::load(&session.snapshot().json_path()).unwrap();
        assert_eq!(
            saved
                .active_thread()
                .user_turn_timestamps
                .values()
                .copied()
                .collect::<Vec<_>>(),
            [accepted]
        );
        assert_eq!(
            saved
                .active_thread()
                .messages
                .iter()
                .filter(|message| message.is_user_turn())
                .count(),
            1
        );
        assert_eq!(model.remaining(), 0);
    });
}

#[test]
fn cancellation_keeps_failed_save_visible_and_retains_live_tool_observations() {
    let home = temp_home("auto-cancel-unsaved-observations");
    paused_runtime().block_on(async {
        for (automatic, cancel_before_wait) in [(false, true), (true, true), (true, false)] {
            let (mut runner, model) = runner(vec![response(TurnEndReason::EndTurn, false)]).await;
            let session = runner.runtime().session().clone();
            session.set_auto_continue(automatic).unwrap();
            let observations = vec![
                crate::test_support::user("original task"),
                Message::AssistantMessage {
                    content: vec![],
                    tool_uses: vec![ToolUse {
                        name: "effect".into(),
                        input: serde_json::json!({}),
                    }],
                    turn_end_reason: Some(TurnEndReason::ToolUse),
                },
                Message::ToolResults {
                    tool_use_results: vec![crate::generative_model::ToolResult::text(
                        "completed once",
                    )],
                },
            ];
            runner
                .agent_mut()
                .replace_context(observations.clone(), None)
                .unwrap();
            let store = home.path().join("session");
            let backup = home.path().join("saved-session");
            std::fs::rename(&store, &backup).unwrap();
            std::fs::write(&store, "storage unavailable").unwrap();
            let error = runner.agent().checkpoint().unwrap_err();
            assert!(matches!(error, AgentInteractionError::Checkpoint(_)));
            let expected_error = error.to_string();
            let cancel = CancelToken::new();
            if cancel_before_wait {
                cancel.cancel();
            }
            let notices = Mutex::new(0);
            let began = tokio::time::Instant::now();
            let stopped = Retry::default()
                .wait(error, &session, &cancel, &|event| {
                    assert!(matches!(event, WorkflowEvent::Retrying { .. }));
                    *notices.lock().unwrap() += 1;
                    cancel.cancel();
                })
                .await;
            assert!(
                matches!(stopped, Err(AgentInteractionError::Checkpoint(_))),
                "{stopped:?}"
            );
            assert_eq!(stopped.unwrap_err().to_string(), expected_error);
            assert_eq!(began.elapsed(), Duration::ZERO);
            assert_eq!(*notices.lock().unwrap(), usize::from(!cancel_before_wait));
            assert!(runner.agent().checkpoint_failed());
            assert_eq!(runner.agent().history(), observations);
            assert_eq!(
                model.remaining(),
                1,
                "cancellation must not dispatch another request"
            );

            std::fs::remove_file(&store).unwrap();
            std::fs::rename(&backup, &store).unwrap();
            runner.agent().checkpoint().unwrap();
            assert!(!runner.agent().checkpoint_failed());
            let saved = Session::load(&session.snapshot().json_path()).unwrap();
            assert_eq!(saved.active_thread().messages, observations);
        }
    });
}
