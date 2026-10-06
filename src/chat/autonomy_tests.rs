use std::sync::Arc;

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
fn auto_continue_handles_output_caps_but_stops_on_provider_errors_and_cancellation() {
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
        failed.runtime().session().set_auto_continue(true).unwrap();
        assert!(
            submit(&mut failed, CancelToken::new())
                .await
                .result
                .is_err()
        );
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
