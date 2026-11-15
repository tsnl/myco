//! Runner workflows over scripted models: request-size recovery, automatic
//! compaction, model changes, follow-ups, and storage failures.

use std::sync::{
    Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

use super::*;
use crate::agent::NullEventSink;
use crate::core::Async;
use crate::generative_model::{
    GenerateError, GenerateOutput, Message, TokenUsage, ToolUse, TurnEndReason,
};
use crate::harness::Harness;
use crate::prompts;
use crate::session::{ActiveSession, Session, Thread, compact_thread};
use crate::test_support::{ScriptedModel, temp_home};
use serde_json::json;

#[derive(Default)]
struct Summarizer {
    calls: AtomicUsize,
    sources: Mutex<Vec<Session>>,
    cancel: AtomicBool,
    failure: Option<&'static str>,
    transient_failure: AtomicBool,
    break_store: Option<std::path::PathBuf>,
}

impl Compactor for Summarizer {
    fn compact(
        self: Arc<Self>,
        predecessor: Session,
        _single_attempt: bool,
        _observer: Arc<dyn Fn(WorkflowEvent) + Send + Sync>,
        cancel: CancelToken,
    ) -> Async<Result<(Thread, CompactOutcome), AgentInteractionError>> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            crate::agent::validate_context(&predecessor.active_thread().messages).unwrap();
            self.sources.lock().unwrap().push(predecessor.clone());
            if self.transient_failure.swap(false, Ordering::SeqCst) {
                return Err(AgentInteractionError::Compaction("temporary outage".into()));
            }
            if let Some(reason) = self.failure {
                return Err(AgentInteractionError::Compaction(reason.into()));
            }
            let compacted =
                compact_thread(&predecessor, "Continue the task with the existing shell").unwrap();
            if self.cancel.swap(false, Ordering::SeqCst) {
                cancel.cancel();
            }
            if let Some(home) = &self.break_store
                && self.calls.load(Ordering::SeqCst) == 1
            {
                std::fs::rename(home.join("session"), home.join("saved-store")).unwrap();
                std::fs::write(home.join("session"), "disk unavailable").unwrap();
            }
            Ok(compacted)
        })
    }
}

fn output(used: u64, input: Option<serde_json::Value>, reason: TurnEndReason) -> GenerateOutput {
    GenerateOutput {
        content: vec![Content::Text {
            text: "progress".into(),
        }],
        tool_uses: input
            .into_iter()
            .map(|input| ToolUse {
                name: "bash".into(),
                input,

                thought_signature: None,
            })
            .collect(),
        turn_end_reason: reason,
        usage: Some(TokenUsage {
            // Keep synthetic measurements above normal runtime framing.
            input_tokens: used * 1000,
            output_tokens: 7,
            cached_input_tokens: 0,
        }),
    }
}

async fn setup(
    scripts: Vec<GenerateOutput>,
    compactor: Arc<Summarizer>,
) -> (SessionRunner, Arc<ScriptedModel>) {
    setup_results(scripts.into_iter().map(Ok).collect(), compactor).await
}

async fn setup_results(
    scripts: Vec<Result<GenerateOutput, GenerateError>>,
    compactor: Arc<Summarizer>,
) -> (SessionRunner, Arc<ScriptedModel>) {
    let session = ActiveSession::new(Session::new("scripted"));
    let runtime = SessionRuntime::new(Harness::local_with_services(vec![]), session);
    let model = ScriptedModel::from_results(scripts);
    let agent = Agent::new(model.clone(), runtime.clone(), Arc::new(NullEventSink));
    let mut runner = SessionRunner::new(agent, runtime).await.unwrap();
    runner.set_compactor(compactor, Some(80_000));
    (runner, model)
}

async fn submit(runner: &mut SessionRunner, cancel: CancelToken) -> SessionTurnOutcome {
    runner
        .submit(
            vec![Content::Text {
                text: "scripted task".into(),
            }],
            Utc::now(),
            cancel,
        )
        .await
}

#[test]
fn oversized_requests_compact_and_continue_without_repeating_tools() {
    let home = temp_home("runner-request-size");
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let effect = home.path().join("effect");
        let compactor = Arc::new(Summarizer::default());
        let (mut runner, model) = setup_results(
            vec![
                Ok(output(
                    20,
                    Some(json!({"command":format!("printf x >> '{}'", effect.display())})),
                    TurnEndReason::ToolUse,
                )),
                Err(GenerateError::RequestTooLargeError("HTTP 413".into())),
                Ok(output(20, None, TurnEndReason::EndTurn)),
            ],
            compactor.clone(),
        )
        .await;
        runner.set_compactor(compactor.clone(), None);
        let result = submit(&mut runner, CancelToken::new()).await;
        let outcome = result.result.unwrap();
        assert!(result.rewound.is_none());
        assert_eq!(outcome.reason, TurnEndReason::EndTurn);
        assert_eq!(outcome.usage.unwrap().output_tokens, 14);
        assert_eq!(model.remaining(), 0);
        assert_eq!(compactor.calls.load(Ordering::SeqCst), 1);
        assert_eq!(std::fs::read_to_string(effect).unwrap(), "x");
        let saved = Session::load(&runner.runtime().session().snapshot().json_path()).unwrap();
        assert_eq!(saved.threads().len(), 2);
        assert!(
            saved.threads()[0]
                .messages
                .iter()
                .any(|message| matches!(message, Message::ToolResults { .. }))
        );
        assert_eq!(
            serde_json::to_value(&saved.threads()[0]).unwrap(),
            serde_json::to_value(compactor.sources.lock().unwrap()[0].active_thread()).unwrap()
        );
        assert!(saved.active_thread().pending_operation.is_none());
        crate::agent::validate_context(&saved.active_thread().messages).unwrap();
    });
}

#[test]
fn oversized_initial_input_recovers_without_usage_and_keeps_original_images() {
    let _home = temp_home("runner-initial-request-size");
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let compactor = Arc::new(Summarizer::default());
        let (mut runner, model) = setup_results(
            vec![
                Err(GenerateError::RequestTooLargeError(
                    "local request byte cap".into(),
                )),
                Ok(output(20, None, TurnEndReason::EndTurn)),
            ],
            compactor.clone(),
        )
        .await;
        runner.set_compactor(compactor.clone(), None);
        let outcome = runner
            .submit(
                vec![Content::Image {
                    source: "data:image/png;base64,aW1hZ2U=".into(),
                }],
                Utc::now(),
                CancelToken::new(),
            )
            .await;
        outcome.result.unwrap();
        assert!(outcome.rewound.is_none());
        assert_eq!(model.remaining(), 0);
        assert_eq!(compactor.calls.load(Ordering::SeqCst), 1);
        let saved = Session::load(&runner.runtime().session().snapshot().json_path()).unwrap();
        let original = saved.threads()[0]
            .messages
            .iter()
            .flat_map(Message::content)
            .find_map(|part| match part {
                Content::Image { source } => Some(source),
                _ => None,
            })
            .unwrap();
        assert_eq!(
            crate::core::image_store::ImageStore::for_profile()
                .unwrap()
                .resolve(original)
                .unwrap(),
            "data:image/png;base64,aW1hZ2U="
        );
        assert!(
            !saved
                .active_thread()
                .messages
                .iter()
                .flat_map(Message::content)
                .any(|part| matches!(part, Content::Image { .. }))
        );
        assert!(
            serde_json::to_string(&saved.active_thread().messages)
                .unwrap()
                .contains("Image omitted to reduce request size")
        );
        assert_eq!(saved.active_thread().user_turn_timestamps.len(), 1);
    });
}

#[test]
fn repeated_size_rejections_stop_compacting_and_accept_the_next_submission() {
    let _home = temp_home("runner-repeated-request-size");
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let compactor = Arc::new(Summarizer::default());
        let (mut runner, model) = setup_results(
            vec![
                Err(GenerateError::RequestTooLargeError("HTTP 413".into())),
                Err(GenerateError::RequestTooLargeError("HTTP 413 again".into())),
                Ok(output(20, None, TurnEndReason::EndTurn)),
            ],
            compactor.clone(),
        )
        .await;
        let result = submit(&mut runner, CancelToken::new()).await;
        assert!(matches!(
            result.result,
            Err(AgentInteractionError::GenerateError(
                GenerateError::RequestTooLargeError(_)
            ))
        ));
        assert_eq!(compactor.calls.load(Ordering::SeqCst), 1);
        assert_eq!(model.remaining(), 1);
        assert!(runner.agent().state().is_idle());
        submit(&mut runner, CancelToken::new())
            .await
            .result
            .unwrap();
    });
}

#[test]
fn size_recovery_can_run_again_after_successful_model_progress() {
    let _home = temp_home("runner-request-size-progress");
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let compactor = Arc::new(Summarizer::default());
        let (mut runner, model) = setup_results(
            vec![
                Err(GenerateError::RequestTooLargeError("HTTP 413".into())),
                Ok(output(
                    20,
                    Some(json!({"command":"printf observed"})),
                    TurnEndReason::ToolUse,
                )),
                Err(GenerateError::RequestTooLargeError(
                    "HTTP 413 after more work".into(),
                )),
                Ok(output(20, None, TurnEndReason::EndTurn)),
            ],
            compactor.clone(),
        )
        .await;
        submit(&mut runner, CancelToken::new())
            .await
            .result
            .unwrap();
        assert_eq!(compactor.calls.load(Ordering::SeqCst), 2);
        assert_eq!(model.remaining(), 0);
    });
}

#[test]
fn size_recovery_preserves_the_run_truncation_cap() {
    let _home = temp_home("runner-request-size-truncation");
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let compactor = Arc::new(Summarizer::default());
        let (mut runner, model) = setup_results(
            vec![
                Ok(output(20, None, TurnEndReason::MaxTokens)),
                Err(GenerateError::RequestTooLargeError("HTTP 413".into())),
                Ok(output(20, None, TurnEndReason::MaxTokens)),
                Ok(output(20, None, TurnEndReason::EndTurn)),
            ],
            compactor.clone(),
        )
        .await;
        runner.agent_mut().set_max_truncated_resumes(1);
        let outcome = submit(&mut runner, CancelToken::new())
            .await
            .result
            .unwrap();
        assert_eq!(outcome.reason, TurnEndReason::MaxTokens);
        assert_eq!(outcome.usage.unwrap().output_tokens, 14);
        assert_eq!(compactor.calls.load(Ordering::SeqCst), 1);
        assert_eq!(model.remaining(), 1);
    });
}

#[test]
fn failed_size_recovery_preserves_tool_observations_and_remains_usable() {
    let _home = temp_home("runner-request-size-failures");
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        for cancelled in [true, false] {
            let compactor = Arc::new(Summarizer {
                cancel: AtomicBool::new(cancelled),
                failure: (!cancelled).then_some("summary unavailable"),
                ..Default::default()
            });
            let (mut runner, model) = setup_results(
                vec![
                    Ok(output(
                        20,
                        Some(json!({"command":"printf observed"})),
                        TurnEndReason::ToolUse,
                    )),
                    Err(GenerateError::RequestTooLargeError("HTTP 413".into())),
                    Ok(output(20, None, TurnEndReason::EndTurn)),
                ],
                compactor.clone(),
            )
            .await;
            let result = submit(&mut runner, CancelToken::new()).await;
            if cancelled {
                assert!(matches!(
                    result.result,
                    Err(AgentInteractionError::Cancelled)
                ));
                assert!(result.rewound.is_none());
            } else {
                assert!(matches!(
                    result.result,
                    Err(AgentInteractionError::GenerateError(
                        GenerateError::RequestTooLargeError(_)
                    ))
                ));
                assert!(result.rewound.is_some());
            }
            assert_eq!(compactor.calls.load(Ordering::SeqCst), 1);
            assert_eq!(model.remaining(), 1);
            let saved = Session::load(&runner.runtime().session().snapshot().json_path()).unwrap();
            assert!(
                saved.threads()[0]
                    .messages
                    .iter()
                    .any(|message| matches!(message, Message::ToolResults { .. }))
            );
            assert!(runner.agent().state().is_idle());
            submit(&mut runner, CancelToken::new())
                .await
                .result
                .unwrap();
        }
    });
}

#[test]
fn followups_join_settled_tool_results_and_survive_automatic_compaction() {
    let _home = temp_home("runner-followups");
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        for compact in [false, true] {
            let compactor = Arc::new(Summarizer::default());
            let (mut runner, model) = setup(
                vec![
                    output(if compact { 90 } else { 20 }, Some(json!({"command":"printf observed"})), TurnEndReason::ToolUse),
                    output(20, None, TurnEndReason::EndTurn),
                ],
                compactor.clone(),
            ).await;
            let delivered = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let accepted_at: DateTime<Utc> = "2026-09-22T12:00:00Z".parse().unwrap();
            runner.set_followup_handler(Some(Arc::new({
                let delivered = delivered.clone();
                move |agent, session| {
                    if delivered.load(Ordering::SeqCst) || !agent.history().iter().any(|message| matches!(message, Message::ToolResults { .. })) {
                        return Ok(false);
                    }
                    for text in ["first follow-up", "second follow-up"] {
                        super::super::append_followup(agent, session, vec![Content::Text { text: text.into() }], accepted_at)?;
                        let saved = Session::load(&session.snapshot().json_path()).unwrap();
                        assert_eq!(saved.active_thread().messages, agent.history());
                    }
                    delivered.store(true, Ordering::SeqCst);
                    Ok(true)
                }
            })));
            submit(&mut runner, CancelToken::new()).await.result.unwrap();
            assert!(delivered.load(Ordering::SeqCst));
            assert_eq!(model.remaining(), 0);
            assert_eq!(compactor.calls.load(Ordering::SeqCst), usize::from(compact));
            let saved = Session::load(&runner.runtime().session().snapshot().json_path()).unwrap();
            let thread = saved.active_thread();
            let followups: Vec<_> = thread.messages.iter().enumerate().filter_map(|(index, message)| {
                match message {
                    Message::UserMessage { content } if matches!(&content[..], [Content::Text { text }] if text.ends_with("follow-up")) => Some(index),
                    _ => None,
                }
            }).collect();
            assert_eq!(followups.len(), 2);
            for index in followups {
                assert_eq!(thread.user_turn_timestamps[&index], accepted_at);
            }
            assert!(thread.pending_operation.is_none());
        }
    });
}

#[test]
fn a_followup_checkpoint_failure_preserves_input_for_retry_without_model_work() {
    let home = temp_home("runner-followup-checkpoint");
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let (mut runner, model) = setup(
            vec![output(20, None, TurnEndReason::EndTurn)],
            Arc::new(Summarizer::default()),
        )
        .await;
        let attempts = Arc::new(AtomicUsize::new(0));
        let queued = Arc::new(Mutex::new(Some(vec![Content::Text {
            text: "keep this follow-up".into(),
        }])));
        runner.set_followup_handler(Some(Arc::new({
            let path = home.path().to_owned();
            let attempts = attempts.clone();
            let queued = queued.clone();
            move |agent, session| {
                let mut queued = queued.lock().unwrap();
                let Some(input) = queued.as_ref() else {
                    return Ok(false);
                };
                if attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                    std::fs::rename(path.join("session"), path.join("saved-store")).unwrap();
                    std::fs::write(path.join("session"), "unavailable").unwrap();
                }
                super::super::append_followup(agent, session, input.clone(), Utc::now())?;
                queued.take();
                Ok(true)
            }
        })));
        assert!(matches!(
            submit(&mut runner, CancelToken::new()).await.result,
            Err(AgentInteractionError::Checkpoint(_))
        ));
        assert!(queued.lock().unwrap().is_some());
        assert_eq!(model.remaining(), 1);
        assert!(
            !serde_json::to_string(runner.agent().history())
                .unwrap()
                .contains("keep this follow-up")
        );
        std::fs::remove_file(home.path().join("session")).unwrap();
        std::fs::rename(home.path().join("saved-store"), home.path().join("session")).unwrap();
        runner
            .continue_run(CancelToken::new())
            .await
            .result
            .unwrap();
        assert!(queued.lock().unwrap().is_none());
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
        assert_eq!(model.remaining(), 0);
        assert_eq!(
            serde_json::to_string(runner.agent().history())
                .unwrap()
                .matches("keep this follow-up")
                .count(),
            1
        );
    });
}

#[test]
fn model_changes_check_the_new_threshold_before_submission_even_after_restart() {
    let _home = temp_home("runner-model-window");
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        for restart in [false, true] {
            for threshold in [80, 90, 200] {
                let compactor = Arc::new(Summarizer::default());
                let (mut runner, _) = setup(
                    vec![output(90, None, TurnEndReason::EndTurn)],
                    compactor.clone(),
                )
                .await;
                runner.set_compactor(compactor.clone(), Some(200_000));
                submit(&mut runner, CancelToken::new())
                    .await
                    .result
                    .unwrap();
                let replacement =
                    ScriptedModel::new(vec![output(20, None, TurnEndReason::EndTurn)]);
                runner
                    .set_model(replacement.clone(), ModelInfo::named("replacement"))
                    .await
                    .unwrap();
                assert!(runner.agent().last_usage().is_none());
                assert_eq!(
                    compactor.calls.load(Ordering::SeqCst),
                    0,
                    "model selection must wait for a submission"
                );
                if restart {
                    let saved =
                        Session::load(&runner.runtime().session().snapshot().json_path()).unwrap();
                    drop(runner);
                    let runtime = SessionRuntime::new(
                        Harness::local_with_services(vec![]),
                        ActiveSession::new(saved),
                    );
                    let agent = Agent::new(
                        replacement.clone(),
                        runtime.clone(),
                        Arc::new(NullEventSink),
                    );
                    runner = SessionRunner::new(agent, runtime).await.unwrap();
                }
                runner.set_compactor(compactor.clone(), Some(threshold * 1000));
                submit(&mut runner, CancelToken::new())
                    .await
                    .result
                    .unwrap();
                let count = usize::from(threshold <= 90);
                assert_eq!(
                    compactor.calls.load(Ordering::SeqCst),
                    count,
                    "restart={restart}, threshold={threshold}"
                );
                assert_eq!(replacement.remaining(), 0);
                let saved = runner.runtime().session().snapshot();
                assert_eq!(saved.threads().len(), 1 + count);
                assert_eq!(saved.active_thread().user_turn_timestamps.len(), 2);
                if count > 0 {
                    let sources = compactor.sources.lock().unwrap();
                    assert_eq!(
                        sources[0]
                            .active_thread()
                            .messages
                            .iter()
                            .filter(|message| matches!(message, Message::AssistantMessage { .. }))
                            .count(),
                        1,
                        "the new model must not receive an oversized request before compaction"
                    );
                    assert_eq!(
                        sources[0]
                            .active_thread()
                            .messages
                            .iter()
                            .filter(|message| message.is_user_turn())
                            .count(),
                        2,
                        "compaction must include the newly accepted input"
                    );
                }
            }
        }
    });
}

#[test]
fn preflight_counts_new_input_without_usage_and_growth_after_a_measurement() {
    let _home = temp_home("preflight-new-input");
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        for prior_usage in [false, true] {
            let compactor = Arc::new(Summarizer::default());
            let mut final_output = output(2, None, TurnEndReason::EndTurn);
            final_output.usage = None;
            let scripts = if prior_usage {
                vec![output(2, None, TurnEndReason::EndTurn), final_output]
            } else {
                vec![final_output]
            };
            let (mut runner, model) = setup(scripts, compactor.clone()).await;
            runner.set_compactor(compactor.clone(), Some(5_000));
            if prior_usage {
                submit(&mut runner, CancelToken::new())
                    .await
                    .result
                    .unwrap();
            }
            runner
                .submit(
                    vec![Content::Text {
                        text: "new input".repeat(2500),
                    }],
                    Utc::now(),
                    CancelToken::new(),
                )
                .await
                .result
                .unwrap();
            assert_eq!(model.remaining(), 0);
            assert_eq!(compactor.calls.load(Ordering::SeqCst), 1);
            let sources = compactor.sources.lock().unwrap();
            let source = &sources[0].active_thread().messages;
            assert_eq!(
                source
                    .iter()
                    .filter(|message| matches!(message, Message::AssistantMessage { .. }))
                    .count(),
                usize::from(prior_usage)
            );
            assert!(
                source
                    .iter()
                    .flat_map(Message::content)
                    .any(|part| matches!(part, Content::Text { text } if text.len() > 20_000))
            );
        }
    });
}

#[test]
fn preflight_waits_for_tool_results_then_counts_large_arguments_and_output() {
    let _home = temp_home("preflight-tools");
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        for command in [
            format!("true #{}", "x".repeat(18_000)),
            "printf '%18000s' x".into(),
        ] {
            let compactor = Arc::new(Summarizer::default());
            let mut first = output(
                2,
                Some(json!({"command":command, "max_bytes":20_000})),
                TurnEndReason::ToolUse,
            );
            first.usage = None;
            let (mut runner, model) = setup(
                vec![first, output(2, None, TurnEndReason::EndTurn)],
                compactor.clone(),
            )
            .await;
            runner.set_compactor(compactor.clone(), Some(5_000));
            submit(&mut runner, CancelToken::new())
                .await
                .result
                .unwrap();
            assert_eq!(model.remaining(), 0);
            assert_eq!(compactor.calls.load(Ordering::SeqCst), 1);
            let sources = compactor.sources.lock().unwrap();
            let source = &sources[0].active_thread().messages;
            assert_eq!(
                source
                    .iter()
                    .filter(|message| matches!(message, Message::AssistantMessage { .. }))
                    .count(),
                1
            );
            assert!(
                source
                    .iter()
                    .any(|message| matches!(message, Message::ToolResults { .. }))
            );
            crate::agent::validate_context(source).unwrap();
        }
    });
}

#[test]
fn changed_prelude_is_prepared_before_sizing_without_recompacting_the_same_successor() {
    let _home = temp_home("preflight-prelude");
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let compactor = Arc::new(Summarizer::default());
        let (mut runner, model) = setup(vec![output(2, None, TurnEndReason::EndTurn)], compactor.clone()).await;
        runner.set_compactor(compactor.clone(), Some(5_000));
        let sent = Arc::new(AtomicBool::new(false));
        runner.agent_mut().set_before_generation_notice(Some(Box::new(move |_, _| {
            let first = !sent.swap(true, Ordering::SeqCst);
            Box::pin(async move { first.then(|| "prelude".repeat(3000)) })
        })));
        submit(&mut runner, CancelToken::new()).await.result.unwrap();
        assert_eq!(model.remaining(), 0);
        assert_eq!(compactor.calls.load(Ordering::SeqCst), 1);
        let sources = compactor.sources.lock().unwrap();
        assert!(sources[0].active_thread().messages.iter().flat_map(Message::content).any(|part| matches!(part, Content::System { kind, text, .. } if kind == "generation_notice" && text.len() > 20_000)));
    });
}

#[test]
fn cancelling_preflight_notice_settles_the_saved_intent_without_model_work() {
    let _home = temp_home("preflight-cancel-notice");
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let compactor = Arc::new(Summarizer::default());
        let (mut runner, model) = setup(
            vec![output(2, None, TurnEndReason::EndTurn)],
            compactor.clone(),
        )
        .await;
        let polling = CancelToken::new();
        let started = polling.clone();
        runner
            .agent_mut()
            .set_before_generation_notice(Some(Box::new(move |_, _| {
                started.cancel();
                Box::pin(std::future::pending())
            })));
        let cancel = CancelToken::new();
        let abort = cancel.clone();
        tokio::spawn(async move {
            polling.cancelled().await;
            abort.cancel();
        });
        assert!(matches!(
            submit(&mut runner, cancel).await.result,
            Err(AgentInteractionError::Cancelled)
        ));
        assert!(runner.agent().state().is_idle());
        assert_eq!(model.remaining(), 1);
        assert_eq!(compactor.calls.load(Ordering::SeqCst), 0);
        let saved = Session::load(&runner.runtime().session().snapshot().json_path()).unwrap();
        assert!(saved.active_thread().pending_operation.is_none());
    });
}

#[test]
fn measured_successor_usage_reenables_preflight_after_a_conservative_estimate() {
    let _home = temp_home("preflight-measured-successor");
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let compactor = Arc::new(Summarizer::default());
        let (mut runner, model) = setup(
            vec![output(2, None, TurnEndReason::EndTurn); 2],
            compactor.clone(),
        )
        .await;
        runner.set_compactor(compactor.clone(), Some(5_000));
        for (size, expected_compactions) in [(24_000, 1), (12_000, 2)] {
            runner
                .submit(
                    vec![Content::Text {
                        text: "x".repeat(size),
                    }],
                    Utc::now(),
                    CancelToken::new(),
                )
                .await
                .result
                .unwrap();
            assert_eq!(compactor.calls.load(Ordering::SeqCst), expected_compactions);
            assert!(!runner.workflow.auto_ineffective);
            assert!(runner.agent().context_tokens_estimate() < 5_000);
        }
        assert_eq!(model.remaining(), 0);
    });
}

#[test]
fn manual_compaction_clears_the_estimate_left_by_a_model_change() {
    let _home = temp_home("runner-estimate-reset");
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let compactor = Arc::new(Summarizer::default());
        let (mut runner, _) = setup(
            vec![output(90, None, TurnEndReason::EndTurn)],
            compactor.clone(),
        )
        .await;
        runner.set_compactor(compactor.clone(), Some(200_000));
        submit(&mut runner, CancelToken::new())
            .await
            .result
            .unwrap();
        runner
            .set_model(
                ScriptedModel::new(vec![output(20, None, TurnEndReason::EndTurn)]),
                ModelInfo::named("replacement"),
            )
            .await
            .unwrap();
        assert_eq!(
            runner
                .runtime()
                .session()
                .snapshot()
                .active_thread()
                .context_tokens_estimate,
            Some(90_000)
        );
        runner.set_compactor(compactor.clone(), Some(80_000));
        runner.compact(CancelToken::new()).await.unwrap();
        assert!(
            runner
                .runtime()
                .session()
                .snapshot()
                .active_thread()
                .context_tokens_estimate
                .is_none()
        );
        submit(&mut runner, CancelToken::new())
            .await
            .result
            .unwrap();
        assert_eq!(compactor.calls.load(Ordering::SeqCst), 1);
    });
}

#[test]
fn runtime_records_survive_compaction_and_report_model_changes_and_lost_handles_on_restart() {
    let home = temp_home("runner-lifecycle");
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let file = home.path().join("observed.txt");
        std::fs::write(&file, "read before editing").unwrap();
        let mut start = output(90, Some(json!({"action":"start", "session_id":"live", "command":"bash --noprofile --norc", "idle_ms":10, "timeout_ms":1000})), TurnEndReason::ToolUse);
        start.tool_uses.push(ToolUse { name: "str_replace_based_edit_tool".into(), input: json!({"command":"view", "path":file}), thought_signature: None });
        let (mut runner, _) = setup(vec![start, output(20, None, TurnEndReason::EndTurn)], Arc::new(Summarizer::default())).await;
        submit(&mut runner, CancelToken::new()).await.result.unwrap();
        let current = crate::RuntimeRecord::latest(runner.agent().history()).unwrap();
        let handles = current.resources.iter().find(|host| host.host == "local").unwrap().resources.as_ref().unwrap();
        assert_eq!(handles.len(), 2);
        assert!(handles.iter().any(|resource| resource.id == "live" && resource.details["process_exited"] == false));
        assert!(handles.iter().any(|resource| resource.id == file.to_string_lossy()));
        assert!(current.unavailable_after_restart.is_empty());
        let path = runner.runtime().session().snapshot().json_path();
        let saved = Session::load(&path).unwrap();
        assert_eq!(saved.threads().len(), 2);
        assert_eq!(crate::RuntimeRecord::latest(&saved.active_thread().messages).unwrap().runtime_id, current.runtime_id);

        let replacement = ScriptedModel::new(vec![]);
        let mut info = ModelInfo::named("replacement");
        info.effort = Some(crate::generative_model::Effort::Max);
        runner.set_model(replacement, info.clone()).await.unwrap();
        let changed = crate::RuntimeRecord::latest(&Session::load(&path).unwrap().active_thread().messages).unwrap();
        assert_eq!(changed.previous_model.unwrap().key, "scripted");
        assert_eq!(changed.model, info);
        assert_eq!(changed.runtime_id, current.runtime_id);
        assert!(!changed.resumed);
        drop(runner);

        let saved = Session::load(&path).unwrap();
        let times = saved.active_thread().user_turn_timestamps.clone();
        let session = ActiveSession::new(saved);
        let runtime = SessionRuntime::new(Harness::local_with_services(vec![]), session);
        let model = ScriptedModel::new(vec![output(20, None, TurnEndReason::EndTurn)]);
        let agent = Agent::new(model.clone(), runtime.clone(), Arc::new(NullEventSink));
        let mut resumed = SessionRunner::new(agent, runtime).await.unwrap();
        resumed.set_model(model, info).await.unwrap();
        resumed.resume(CancelToken::new()).await.result.unwrap();
        let recovered = crate::RuntimeRecord::latest(resumed.agent().history()).unwrap();
        assert_ne!(recovered.runtime_id, current.runtime_id);
        assert!(recovered.resumed);
        assert!(recovered.resources[0].resources.as_ref().unwrap().is_empty());
        assert_eq!(recovered.unavailable_after_restart[0].resources.as_ref().unwrap(), handles);
        assert_eq!(Session::load(&path).unwrap().active_thread().user_turn_timestamps, times);
    });
}

#[test]
fn selected_models_are_recorded_on_the_session_before_and_after_its_first_save() {
    let _home = temp_home("runner-model-key");
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let (mut runner, _) = setup(vec![], Arc::new(Summarizer::default())).await;
        let selected = ScriptedModel::new(vec![output(20, None, TurnEndReason::EndTurn)]);
        runner
            .set_model(selected, ModelInfo::named("selected"))
            .await
            .unwrap();
        let path = runner.runtime().session().snapshot().json_path();
        assert!(
            !path.exists(),
            "selecting a model must not create a session"
        );
        submit(&mut runner, CancelToken::new())
            .await
            .result
            .unwrap();
        assert_eq!(Session::load(&path).unwrap().model, "selected");
        runner
            .set_model(ScriptedModel::new(vec![]), ModelInfo::named("another"))
            .await
            .unwrap();
        assert_eq!(Session::load(&path).unwrap().model, "another");
    });
}

/// A fork opened again before its first submission still stamps its own
/// identity, exactly once, after the inherited stamp naming its parent.
#[test]
fn a_fork_stamps_its_own_identity_once_across_restarts() {
    let _home = temp_home("runner-fork-stamp");
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let mut parent = Session::new("scripted");
        let stamp =
            prompts::thread_stamp_part(&parent.id, &parent.active_thread().id, parent.created_at);
        parent.replace_context(
            vec![
                Message::UserMessage {
                    content: vec![
                        stamp,
                        Content::Text {
                            text: "parent task".into(),
                        },
                    ],
                },
                crate::test_support::assistant("parent answer"),
            ],
            None,
        );
        let child = parent.fork_child("scripted");
        child.save().unwrap();
        let path = child.json_path();
        let open = |turns: usize| {
            let path = path.clone();
            async move {
                let session = ActiveSession::new(Session::load(&path).unwrap());
                let runtime = SessionRuntime::new(Harness::local_with_services(vec![]), session);
                let model =
                    ScriptedModel::new(vec![output(20, None, TurnEndReason::EndTurn); turns]);
                let agent = Agent::new(model, runtime.clone(), Arc::new(NullEventSink));
                SessionRunner::new(agent, runtime).await.unwrap()
            }
        };
        let stamps = |runner: &SessionRunner, id: &str| {
            runner
                .agent()
                .history()
                .iter()
                .flat_map(Message::content)
                .filter(|part| {
                    matches!(part, Content::System { kind, data, .. }
                    if kind == "session" && data["session_id"].as_str() == Some(id))
                })
                .count()
        };
        drop(open(0).await);
        let mut runner = open(1).await;
        submit(&mut runner, CancelToken::new())
            .await
            .result
            .unwrap();
        assert!(
            matches!(&runner.agent().history()[2], Message::UserMessage { content }
            if matches!(&content[0], Content::System { kind, data, .. }
                if kind == "session" && data["session_id"].as_str() == Some(child.id.as_str())))
        );
        drop(runner);
        let mut runner = open(1).await;
        submit(&mut runner, CancelToken::new())
            .await
            .result
            .unwrap();
        assert_eq!(stamps(&runner, &parent.id), 1);
        assert_eq!(stamps(&runner, &child.id), 1);
    });
}

#[test]
fn repairing_the_input_checkpoint_can_continue_without_resubmitting_the_task() {
    let home = temp_home("runner-input-repair");
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let (mut runner, model) = setup(
            vec![output(20, None, TurnEndReason::EndTurn)],
            Arc::new(Summarizer::default()),
        )
        .await;
        std::fs::write(home.path().join("session"), "store unavailable").unwrap();
        assert!(matches!(
            submit(&mut runner, CancelToken::new()).await.result,
            Err(AgentInteractionError::Checkpoint(_))
        ));
        assert_eq!(model.remaining(), 1);
        std::fs::remove_file(home.path().join("session")).unwrap();
        runner
            .continue_run(CancelToken::new())
            .await
            .result
            .unwrap();
        let saved = Session::load(&runner.runtime().session().snapshot().json_path()).unwrap();
        assert_eq!(
            saved
                .active_thread()
                .messages
                .iter()
                .filter(|message| message.is_user_turn())
                .count(),
            1
        );
        assert_eq!(saved.active_thread().user_turn_timestamps.len(), 1);
        assert_eq!(model.remaining(), 0);
    });
}

#[test]
fn failed_model_change_keeps_the_original_model_history_and_usage() {
    let home = temp_home("model-change-save");
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let (mut runner, old) = setup(
            vec![output(20, None, TurnEndReason::EndTurn); 2],
            Arc::new(Summarizer::default()),
        )
        .await;
        submit(&mut runner, CancelToken::new())
            .await
            .result
            .unwrap();
        let history = runner.agent().history().to_vec();
        let usage = runner.agent().last_usage();
        let replacement = ScriptedModel::new(vec![output(30, None, TurnEndReason::EndTurn)]);
        std::fs::rename(home.path().join("session"), home.path().join("saved")).unwrap();
        std::fs::write(home.path().join("session"), "store unavailable").unwrap();
        assert!(
            runner
                .set_model(replacement.clone(), ModelInfo::named("replacement"))
                .await
                .is_err()
        );
        assert_eq!(runner.agent().history(), history);
        assert_eq!(runner.agent().last_usage(), usage);
        std::fs::remove_file(home.path().join("session")).unwrap();
        std::fs::rename(home.path().join("saved"), home.path().join("session")).unwrap();
        submit(&mut runner, CancelToken::new())
            .await
            .result
            .unwrap();
        assert_eq!(old.remaining(), 0);
        assert_eq!(replacement.remaining(), 1);
    });
}

#[test]
fn queued_agents_refresh_the_same_thread_and_stale_live_retries_cannot_overwrite_it() {
    let home = temp_home("runner-stale-writer");
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let (mut first, first_model) = setup(
            vec![output(20, None, TurnEndReason::EndTurn)],
            Arc::new(Summarizer::default()),
        )
        .await;
        let runtime = first.runtime().clone();
        let model = ScriptedModel::new(vec![output(20, None, TurnEndReason::EndTurn)]);
        let agent = Agent::new(model, runtime.clone(), Arc::new(NullEventSink));
        let mut second = SessionRunner::new(agent, runtime.clone()).await.unwrap();
        std::fs::write(home.path().join("session"), "store unavailable").unwrap();
        assert!(matches!(
            submit(&mut first, CancelToken::new()).await.result,
            Err(AgentInteractionError::Checkpoint(_))
        ));
        std::fs::remove_file(home.path().join("session")).unwrap();
        second
            .submit(
                vec![Content::Text {
                    text: "newer work".into(),
                }],
                Utc::now(),
                CancelToken::new(),
            )
            .await
            .result
            .unwrap();
        let saved = serde_json::to_value(runtime.session().snapshot()).unwrap();
        assert!(matches!(
            first.continue_run(CancelToken::new()).await.result,
            Err(AgentInteractionError::Checkpoint(_))
        ));
        assert_eq!(first_model.remaining(), 1);
        assert_eq!(
            serde_json::to_value(runtime.session().snapshot()).unwrap(),
            saved
        );

        let agent = Agent::new(
            ScriptedModel::new(vec![output(20, None, TurnEndReason::EndTurn)]),
            runtime.clone(),
            Arc::new(NullEventSink),
        );
        let mut third = SessionRunner::new(agent, runtime.clone()).await.unwrap();
        let agent = Agent::new(
            ScriptedModel::new(vec![output(20, None, TurnEndReason::EndTurn)]),
            runtime.clone(),
            Arc::new(NullEventSink),
        );
        let mut fourth = SessionRunner::new(agent, runtime.clone()).await.unwrap();
        third
            .submit(
                vec![Content::Text {
                    text: "third".into(),
                }],
                Utc::now(),
                CancelToken::new(),
            )
            .await
            .result
            .unwrap();
        fourth
            .submit(
                vec![Content::Text {
                    text: "fourth".into(),
                }],
                Utc::now(),
                CancelToken::new(),
            )
            .await
            .result
            .unwrap();
        let saved = Session::load(&runtime.session().snapshot().json_path()).unwrap();
        let human: Vec<_> = saved
            .active_thread()
            .messages
            .iter()
            .filter(|message| message.is_user_turn())
            .collect();
        assert_eq!(human.len(), 3);
        let text = serde_json::to_string(&human).unwrap();
        assert!(text.contains("newer work") && text.contains("third") && text.contains("fourth"));
    });
}

#[test]
fn ineffective_compaction_stops_recompacting_and_manual_compaction_does_not_continue() {
    let _home = temp_home("runner-ineffective");
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let compactor = Arc::new(Summarizer::default());
        let (mut runner, model) = setup(
            vec![
                output(90, None, TurnEndReason::EndTurn),
                output(90, None, TurnEndReason::EndTurn),
            ],
            compactor.clone(),
        )
        .await;
        let warnings = Arc::new(Mutex::new(Vec::new()));
        let observer = warnings.clone();
        runner.set_observer(Arc::new(move |event| {
            if let WorkflowEvent::Warning(message) = event {
                observer.lock().unwrap().push(message);
            }
        }));
        submit(&mut runner, CancelToken::new())
            .await
            .result
            .unwrap();
        assert_eq!(model.remaining(), 0);
        assert_eq!(compactor.calls.load(Ordering::SeqCst), 1);
        assert!(warnings.lock().unwrap()[0].contains("did not reduce"));
        runner.compact(CancelToken::new()).await.unwrap();
        assert_eq!(compactor.calls.load(Ordering::SeqCst), 2);
        assert_eq!(model.remaining(), 0);
    });
}

#[test]
fn one_scripted_run_compacts_repeatedly_between_tool_rounds_and_keeps_its_shell() {
    let _home = temp_home("runner-compaction");
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let compactor = Arc::new(Summarizer::default());
        let (mut runner, model) = setup(vec![
            output(90, Some(json!({"action":"start", "session_id":"persistent", "command":"marker=42; export marker; exec bash --noprofile --norc", "idle_ms":10, "timeout_ms":1000})), TurnEndReason::ToolUse),
            output(20, Some(json!({"action":"write", "session_id":"persistent", "stdin":"echo marker-$marker\n", "idle_ms":20, "timeout_ms":1000})), TurnEndReason::ToolUse),
            output(90, Some(json!({"action":"write", "session_id":"persistent", "stdin":"echo next-$marker\n", "idle_ms":20, "timeout_ms":1000})), TurnEndReason::ToolUse),
            output(20, None, TurnEndReason::EndTurn),
        ], compactor.clone()).await;
        let outcome = submit(&mut runner, CancelToken::new()).await.result.unwrap();
        assert_eq!(compactor.calls.load(Ordering::SeqCst), 2);
        assert_eq!(model.remaining(), 0);
        assert_eq!(outcome.usage.unwrap().output_tokens, 28);
        assert_eq!(outcome.reason, TurnEndReason::EndTurn);
        let hosts = runner.runtime().resources().await;
        assert_eq!(hosts.iter().flat_map(|host| host.resources.iter().flatten()).count(), 1);
        let saved = Session::load(&runner.runtime().session().snapshot().json_path()).unwrap();
        assert_eq!(saved.threads().len(), 3);
        assert_eq!(saved.active_thread().user_turn_timestamps.len(), 1);
        assert!(saved.active_thread().pending_operation.is_none());
        let history = serde_json::to_string(&saved.active_thread().messages).unwrap();
        assert!(history.contains("marker-42"), "{history}");
        assert!(history.contains("next-42"), "{history}");
        let sources = compactor.sources.lock().unwrap();
        assert_eq!(serde_json::to_value(&saved.threads()[0]).unwrap(), serde_json::to_value(sources[0].active_thread()).unwrap());
        assert_eq!(serde_json::to_value(&saved.threads()[1]).unwrap(), serde_json::to_value(sources[1].active_thread()).unwrap());
    });
}

#[test]
fn compaction_preserves_the_run_truncation_cap() {
    let _home = temp_home("runner-truncation");
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let compactor = Arc::new(Summarizer::default());
        let (mut runner, model) = setup(
            vec![
                output(90, None, TurnEndReason::MaxTokens),
                output(20, None, TurnEndReason::MaxTokens),
                output(20, None, TurnEndReason::EndTurn),
            ],
            compactor.clone(),
        )
        .await;
        runner.agent_mut().set_max_truncated_resumes(1);
        let outcome = submit(&mut runner, CancelToken::new())
            .await
            .result
            .unwrap();
        assert_eq!(outcome.reason, TurnEndReason::MaxTokens);
        assert_eq!(model.remaining(), 1);
        assert_eq!(outcome.usage.unwrap().output_tokens, 14);
        assert_eq!(compactor.calls.load(Ordering::SeqCst), 1);
    });
}

#[test]
fn auto_compaction_does_not_continue_a_refusal_or_exhausted_output_cap() {
    let _home = temp_home("runner-stop-reasons");
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        for reason in [
            TurnEndReason::MaxTokens,
            TurnEndReason::Other("refusal".into()),
            TurnEndReason::Other("unknown".into()),
        ] {
            let compactor = Arc::new(Summarizer::default());
            let (mut runner, model) =
                setup(vec![output(90, None, reason.clone())], compactor.clone()).await;
            runner.agent_mut().set_max_truncated_resumes(0);
            let outcome = submit(&mut runner, CancelToken::new())
                .await
                .result
                .unwrap();
            assert_eq!(outcome.reason, reason);
            assert_eq!(model.remaining(), 0);
            assert_eq!(compactor.calls.load(Ordering::SeqCst), 0);
        }
    });
}

#[test]
fn failed_auto_compaction_stops_work_and_a_later_submission_can_retry() {
    let _home = temp_home("runner-compact-recovery");
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let compactor = Arc::new(Summarizer {
            transient_failure: AtomicBool::new(true),
            ..Default::default()
        });
        let (mut runner, model) = setup(
            vec![
                output(90, None, TurnEndReason::EndTurn),
                output(20, None, TurnEndReason::EndTurn),
            ],
            compactor.clone(),
        )
        .await;
        let error = submit(&mut runner, CancelToken::new())
            .await
            .result
            .unwrap_err();
        assert!(matches!(error, AgentInteractionError::Compaction(_)));
        assert_eq!(model.remaining(), 1);
        assert_eq!(compactor.calls.load(Ordering::SeqCst), 1);
        assert_eq!(runner.runtime().session().snapshot().threads().len(), 1);
        submit(&mut runner, CancelToken::new())
            .await
            .result
            .unwrap();
        assert_eq!(model.remaining(), 0);
        assert_eq!(compactor.calls.load(Ordering::SeqCst), 2);
        assert_eq!(runner.runtime().session().snapshot().threads().len(), 2);
    });
}

#[test]
fn cancellation_during_compaction_keeps_the_source_and_accepts_the_next_submission() {
    let _home = temp_home("runner-cancel");
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let compactor = Arc::new(Summarizer {
            cancel: AtomicBool::new(true),
            ..Default::default()
        });
        let (mut runner, model) = setup(
            vec![
                output(
                    90,
                    Some(json!({"command":"printf observed"})),
                    TurnEndReason::ToolUse,
                ),
                output(20, None, TurnEndReason::EndTurn),
            ],
            compactor,
        )
        .await;
        assert!(matches!(
            submit(&mut runner, CancelToken::new()).await.result,
            Err(AgentInteractionError::Cancelled)
        ));
        assert_eq!(model.remaining(), 1);
        assert_eq!(runner.runtime().session().snapshot().threads().len(), 1);
        assert!(runner.agent().state().is_idle());
        submit(&mut runner, CancelToken::new())
            .await
            .result
            .unwrap();
    });
}

#[test]
fn a_failed_compaction_commit_can_resume_live_work_without_repeating_a_tool() {
    let home = temp_home("runner-storage-repair");
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let effect = home.path().join("effect");
        let compactor = Arc::new(Summarizer {
            break_store: Some(home.path().to_owned()),
            ..Default::default()
        });
        let (mut runner, model) = setup(
            vec![
                output(
                    90,
                    Some(json!({"command":format!("printf x >> '{}'", effect.display())})),
                    TurnEndReason::ToolUse,
                ),
                output(20, None, TurnEndReason::EndTurn),
            ],
            compactor,
        )
        .await;
        assert!(matches!(
            submit(&mut runner, CancelToken::new()).await.result,
            Err(AgentInteractionError::Checkpoint(_))
        ));
        assert_eq!(model.remaining(), 1);
        std::fs::remove_file(home.path().join("session")).unwrap();
        std::fs::rename(home.path().join("saved-store"), home.path().join("session")).unwrap();
        let outcome = runner
            .continue_run(CancelToken::new())
            .await
            .result
            .unwrap();
        assert_eq!(outcome.usage.unwrap().output_tokens, 14);
        assert_eq!(std::fs::read_to_string(effect).unwrap(), "x");
        let saved = Session::load(&runner.runtime().session().snapshot().json_path()).unwrap();
        assert_eq!(saved.threads().len(), 2);
        assert!(saved.active_thread().pending_operation.is_none());
    });
}
