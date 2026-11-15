//! Agent loop behavior: tool rounds, cancellation, checkpoints, retries, and
//! context replacement, over scripted models and in-memory tools.

use super::*;
use crate::test_support::{
    ScriptedModel, assistant, assistant_tool, result_text, tool_results, user,
};
use crate::test_support::{TestTools, interact};
use futures::stream;
use myco_model::{
    ContentDelta, GenerateError, GenerateOutput, GenerationEvent, MessagePart, ToolSpec,
    TurnEndReason,
};
use serde_json::json;
use std::sync::Mutex;
use std::time::{Duration, Instant};

#[derive(Default)]
struct EventLog(Mutex<Vec<AgentEvent>>);

#[tokio::test]
async fn preparing_a_generation_retries_its_notice_checkpoint_without_polling_twice() {
    let model = ScriptedModel::new(vec![GenerateOutput {
        content: vec![],
        tool_uses: vec![],
        usage: None,
        turn_end_reason: TurnEndReason::EndTurn,
    }]);
    let mut agent = Agent::new(
        model.clone(),
        TestTools::new(vec![]),
        Arc::new(NullEventSink),
    );
    agent.append_input(user("task")).unwrap();
    agent.start_run().unwrap();
    let polls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let observed = polls.clone();
    agent.set_before_generation_notice(Some(Box::new(move |_, _| {
        observed.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Some("new prelude".repeat(1000)) })
    })));
    let broken = Arc::new(AtomicBool::new(true));
    let disk = broken.clone();
    agent.set_checkpoint(Some(Box::new(move |state| {
        if disk.load(Ordering::SeqCst) && state.context_tokens_estimate() > 100 {
            Err("disk full".into())
        } else {
            Ok(())
        }
    })));
    assert!(matches!(
        agent.prepare_generation(CancelToken::new()).await,
        Err(AgentInteractionError::Checkpoint(_))
    ));
    let size = agent.context_tokens_estimate();
    assert!(size > 3000);
    assert_eq!(model.remaining(), 1);
    broken.store(false, Ordering::SeqCst);
    agent.prepare_generation(CancelToken::new()).await.unwrap();
    agent.step(CancelToken::new()).await.unwrap();
    assert_eq!(polls.load(Ordering::SeqCst), 1);
    assert_eq!(model.remaining(), 0);
    assert_eq!(
        agent
            .history()
            .iter()
            .flat_map(Message::content)
            .filter(
                |part| matches!(part, Content::System { kind, .. } if kind == "generation_notice")
            )
            .count(),
        1
    );
}

#[tokio::test]
async fn generation_commit_events_require_a_successful_history_checkpoint() {
    for fails in [false, true] {
        let model = ScriptedModel::new(vec![GenerateOutput {
            content: vec![Content::Text {
                text: "answer".into(),
            }],
            tool_uses: vec![],
            usage: None,
            turn_end_reason: TurnEndReason::EndTurn,
        }]);
        let events = Arc::new(EventLog::default());
        let mut agent = Agent::new(model, TestTools::new(vec![]), events.clone());
        agent.append_input(user("task")).unwrap();
        agent.set_checkpoint(Some(Box::new(move |state| {
            if fails
                && state
                    .history()
                    .iter()
                    .any(|message| matches!(message, Message::AssistantMessage { .. }))
            {
                Err("disk full".into())
            } else {
                Ok(())
            }
        })));
        agent.start_run().unwrap();
        let result = agent.step(CancelToken::new()).await;
        assert_eq!(result.is_ok(), !fails);
        let events = events.0.lock().unwrap();
        assert!(
            events
                .iter()
                .any(|event| matches!(event, AgentEvent::GenerationFinished { .. }))
        );
        assert_eq!(
            events
                .iter()
                .any(|event| matches!(event, AgentEvent::GenerationCommitted { .. })),
            !fails
        );
        if !fails {
            let committed = events
                .iter()
                .position(|event| matches!(event, AgentEvent::GenerationCommitted { .. }))
                .unwrap();
            let finished = events
                .iter()
                .position(|event| matches!(event, AgentEvent::TurnFinished { .. }))
                .unwrap();
            assert!(committed < finished);
        }
    }
}

#[tokio::test]
async fn cancelling_notice_poll_preserves_the_update_for_the_next_run() {
    let started = CancelToken::new();
    let release = CancelToken::new();
    let model = ScriptedModel::new(vec![GenerateOutput {
        content: vec![],
        tool_uses: vec![],
        turn_end_reason: TurnEndReason::EndTurn,
        usage: None,
    }]);
    let mut agent = Agent::new(
        model.clone(),
        TestTools::new(vec![]),
        Arc::new(NullEventSink),
    );
    agent.replace_context(vec![user("task")], None).unwrap();
    agent.set_before_generation_notice(Some(Box::new({
        let started = started.clone();
        let release = release.clone();
        move |_, _| {
            let started = started.clone();
            let release = release.clone();
            Box::pin(async move {
                started.cancel();
                release.cancelled().await;
                Some("updated context".into())
            })
        }
    })));
    let cancel = CancelToken::new();
    let (outcome, ()) = tokio::join!(agent.run(cancel.clone()), async {
        started.cancelled().await;
        cancel.cancel();
    });
    assert!(matches!(outcome, Err(AgentInteractionError::Cancelled)));
    assert_eq!(model.remaining(), 1);
    assert_eq!(
        serde_json::to_value(agent.history()).unwrap(),
        serde_json::to_value([user("task")]).unwrap()
    );

    let saved = Arc::new(Mutex::new(Vec::new()));
    agent.set_checkpoint(Some(Box::new({
        let saved = saved.clone();
        move |state| {
            if state.pending_operation().is_some() {
                *saved.lock().unwrap() = state.history().to_vec();
            }
            Ok(())
        }
    })));
    release.cancel();
    agent.run(CancelToken::new()).await.unwrap();
    assert_eq!(model.remaining(), 0);
    let snapshot = saved.lock().unwrap();
    assert!(
        matches!(snapshot.as_slice(), [Message::UserMessage { content }]
        if matches!(content.last(), Some(Content::System { kind, text, .. })
            if kind == "generation_notice" && text == "updated context"))
    );
    assert_eq!(agent.history().len(), 2);
}

impl EventSink for EventLog {
    fn emit(&self, event: AgentEvent) {
        self.0.lock().unwrap().push(event);
    }
}

#[tokio::test]
async fn each_run_closes_its_event_stream_once_on_success_failure_or_cancel() {
    let model = ScriptedModel::new(vec![GenerateOutput {
        content: vec![],
        tool_uses: vec![],
        turn_end_reason: TurnEndReason::EndTurn,
        usage: None,
    }])
    .then_fail(GenerateError::ExecutionError("unavailable".into()));
    let events = Arc::new(EventLog::default());
    let mut agent = Agent::new(model, TestTools::new(vec![]), events.clone());
    let cancelled = CancelToken::new();
    cancelled.cancel();
    for (cancel, succeeds) in [
        (CancelToken::new(), true),
        (CancelToken::new(), false),
        (cancelled, false),
    ] {
        let result = interact(&mut agent, vec![], cancel).await;
        assert_eq!(result.is_ok(), succeeds);
        let mut emitted = events.0.lock().unwrap();
        assert!(matches!(
            emitted.last(),
            Some(AgentEvent::TurnFinished { .. })
        ));
        assert_eq!(
            emitted
                .iter()
                .filter(|event| matches!(event, AgentEvent::TurnFinished { .. }))
                .count(),
            1
        );
        emitted.clear();
    }
}

/// Sleeps, records start/end instants, returns the configured label.
struct SlowService {
    name: String,
    delay: Duration,
    starts: Arc<Mutex<Vec<(String, Instant)>>>,
    ends: Arc<Mutex<Vec<(String, Instant)>>>,
}

impl ToolExecutor for SlowService {
    fn tool_specs(&self) -> Vec<ToolSpec> {
        vec![ToolSpec {
            name: self.name.clone(),
            description: format!("slow test tool {}", self.name),
            input_schema: json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false,
            }),
        }]
    }

    fn dispatch(
        self: Arc<Self>,
        tool_use: ToolUse,
        _cancel: CancelToken,
        _background: CancelToken,
    ) -> Async<ToolResult> {
        Box::pin(async move {
            let started = Instant::now();
            self.starts
                .lock()
                .unwrap()
                .push((tool_use.name.clone(), started));
            tokio::time::sleep(self.delay).await;
            let ended = Instant::now();
            self.ends
                .lock()
                .unwrap()
                .push((tool_use.name.clone(), ended));
            ToolResult::text(format!("done:{}", tool_use.name))
        })
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_tool_uses_overlap_and_preserve_order() {
    let starts = Arc::new(Mutex::new(Vec::new()));
    let ends = Arc::new(Mutex::new(Vec::new()));
    // Long enough that serial execution is unambiguous even under CI load.
    let delay = Duration::from_millis(300);

    // Two distinct tool names so the test executor can route both (same service type).
    let slow_a = Arc::new(SlowService {
        name: "slow_a".into(),
        delay,
        starts: starts.clone(),
        ends: ends.clone(),
    });
    let slow_b = Arc::new(SlowService {
        name: "slow_b".into(),
        delay,
        starts: starts.clone(),
        ends: ends.clone(),
    });

    let tools = TestTools::new(vec![
        slow_a as Arc<dyn ToolExecutor>,
        slow_b as Arc<dyn ToolExecutor>,
    ]);

    let model = ScriptedModel::new(vec![
        GenerateOutput {
            content: vec![],
            tool_uses: vec![
                ToolUse {
                    name: "slow_a".into(),
                    input: json!({}),
                    thought_signature: None,
                },
                ToolUse {
                    name: "slow_b".into(),
                    input: json!({}),
                    thought_signature: None,
                },
            ],
            turn_end_reason: TurnEndReason::ToolUse,
            usage: None,
        },
        GenerateOutput {
            content: vec![Content::Text {
                text: "all done".into(),
            }],
            tool_uses: vec![],
            turn_end_reason: TurnEndReason::EndTurn,
            usage: None,
        },
    ]);

    let mut agent = Agent::new(model, tools, Arc::new(NullEventSink));
    let wall_start = Instant::now();
    let reply = interact(
        &mut agent,
        vec![Content::Text {
            text: "run both".into(),
        }],
        crate::CancelToken::new(),
    )
    .await
    .expect("interact");
    let wall = wall_start.elapsed();

    // Reply is the final assistant text.
    assert_eq!(reply.len(), 1);
    match &reply[0] {
        Content::Text { text } => assert_eq!(text, "all done"),
        other => panic!("expected text reply, got {other:?}"),
    }

    // History: user, assistant(tool_use), tool_results, assistant(end).
    let history = agent.history();
    assert_eq!(history.len(), 4);
    match &history[2] {
        Message::ToolResults { tool_use_results } => {
            assert_eq!(tool_use_results.len(), 2);
            // Order matches the original tool_uses list, not completion order.
            assert_eq!(result_text(&tool_use_results[0]), "done:slow_a");
            assert_eq!(result_text(&tool_use_results[1]), "done:slow_b");
            assert!(!tool_use_results[0].is_error);
            assert!(!tool_use_results[1].is_error);
        }
        other => panic!("expected ToolResults, got {other:?}"),
    }

    // Both tools started before either finished → concurrent.
    let starts = starts.lock().unwrap().clone();
    let ends = ends.lock().unwrap().clone();
    assert_eq!(starts.len(), 2);
    assert_eq!(ends.len(), 2);
    let first_end = ends.iter().map(|(_, t)| *t).min().unwrap();
    let last_start = starts.iter().map(|(_, t)| *t).max().unwrap();
    assert!(
        last_start < first_end,
        "expected overlapping execution: last_start={last_start:?} first_end={first_end:?} starts={starts:?} ends={ends:?}"
    );

    // Overlap of starts/ends is the real concurrency signal. Wall clock is
    // only a coarse guard against fully serial execution; allow large slack
    // for CI / parallel suite load (scheduler jitter, other tests).
    assert!(
        wall < delay * 6 + Duration::from_secs(1),
        "expected concurrent wall time ~1 delay, got {wall:?} (delay={delay:?})"
    );
}

/// Tool intent must be durable before dispatch, and its result durable
/// before the following generation.
#[tokio::test]
async fn checkpoints_include_tool_intent_and_completed_observations() {
    let slow = Arc::new(SlowService {
        name: "slow_a".into(),
        delay: Duration::from_millis(1),
        starts: Arc::new(Mutex::new(Vec::new())),
        ends: Arc::new(Mutex::new(Vec::new())),
    });
    let tools = TestTools::new(vec![slow as Arc<dyn ToolExecutor>]);
    let model = ScriptedModel::new(vec![
        GenerateOutput {
            content: vec![],
            tool_uses: vec![ToolUse {
                name: "slow_a".into(),
                input: json!({}),
                thought_signature: None,
            }],
            turn_end_reason: TurnEndReason::ToolUse,
            usage: None,
        },
        GenerateOutput {
            content: vec![Content::Text {
                text: "done".into(),
            }],
            tool_uses: vec![],
            turn_end_reason: TurnEndReason::EndTurn,
            usage: None,
        },
    ]);
    let mut agent = Agent::new(model, tools, Arc::new(NullEventSink));
    let snapshots: Arc<Mutex<Vec<AgentState>>> = Arc::new(Mutex::new(Vec::new()));
    let record = snapshots.clone();
    agent.set_checkpoint(Some(Box::new(move |state| {
        validate_checkpoint(state.history(), state.pending_operation()).unwrap();
        record.lock().unwrap().push(state.clone());
        Ok(())
    })));

    interact(
        &mut agent,
        vec![Content::Text { text: "run".into() }],
        crate::CancelToken::new(),
    )
    .await
    .expect("interact");

    let snapshots = snapshots.lock().unwrap();
    let intent = snapshots
        .iter()
        .position(|state| {
            matches!(
                state.pending_operation(),
                Some(PendingOperation::Tools { .. })
            )
        })
        .unwrap();
    let observed = snapshots
        .iter()
        .position(|state| matches!(state.history().last(), Some(Message::ToolResults { .. })))
        .unwrap();
    assert!(intent < observed);
    assert!(matches!(
        snapshots[observed].pending_operation(),
        Some(PendingOperation::Generation { .. })
    ));
    assert_eq!(snapshots.first().unwrap().history().len(), 1);
    let finished = snapshots.last().unwrap();
    assert!(finished.pending_operation().is_none());
    assert_eq!(finished.history().len(), 4);
}

#[tokio::test]
async fn failed_intent_checkpoint_prevents_tool_dispatch_and_another_generation() {
    let starts = Arc::new(Mutex::new(Vec::new()));
    let tool = Arc::new(SlowService {
        name: "effect".into(),
        delay: Duration::ZERO,
        starts: starts.clone(),
        ends: Arc::new(Mutex::new(Vec::new())),
    });
    let model = ScriptedModel::new(vec![
        GenerateOutput {
            content: vec![],
            tool_uses: vec![ToolUse {
                name: "effect".into(),
                input: json!({}),
                thought_signature: None,
            }],
            turn_end_reason: TurnEndReason::ToolUse,
            usage: None,
        },
        GenerateOutput {
            content: vec![],
            tool_uses: vec![],
            turn_end_reason: TurnEndReason::EndTurn,
            usage: None,
        },
    ]);
    let mut agent = Agent::new(model, TestTools::new(vec![tool]), Arc::new(NullEventSink));
    agent.set_checkpoint(Some(Box::new(|state| {
        if matches!(
            state.pending_operation(),
            Some(PendingOperation::Tools { .. })
        ) {
            Err("disk full".into())
        } else {
            Ok(())
        }
    })));
    let error = interact(
        &mut agent,
        vec![Content::Text {
            text: "task".into(),
        }],
        CancelToken::new(),
    )
    .await
    .unwrap_err();
    assert!(matches!(error, AgentInteractionError::Checkpoint(_)));
    assert_eq!(error.recovery(), Recovery::Stop);
    assert!(starts.lock().unwrap().is_empty());
    assert!(matches!(
        agent.state().pending_operation(),
        Some(PendingOperation::Tools { .. })
    ));
    assert!(matches!(
        agent.run(CancelToken::new()).await,
        Err(AgentInteractionError::State(StateError::Busy))
    ));
    assert!(starts.lock().unwrap().is_empty());
    agent.set_checkpoint(None);
    agent.continue_run(CancelToken::new()).await.unwrap();
    assert_eq!(starts.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn failed_result_checkpoint_retains_observations_and_stops_before_the_next_model_call() {
    let starts = Arc::new(Mutex::new(Vec::new()));
    let tool = Arc::new(SlowService {
        name: "effect".into(),
        delay: Duration::ZERO,
        starts: starts.clone(),
        ends: Arc::new(Mutex::new(Vec::new())),
    });
    let model = ScriptedModel::new(vec![
        GenerateOutput {
            content: vec![],
            tool_uses: vec![ToolUse {
                name: "effect".into(),
                input: json!({}),
                thought_signature: None,
            }],
            turn_end_reason: TurnEndReason::ToolUse,
            usage: None,
        },
        GenerateOutput {
            content: vec![],
            tool_uses: vec![],
            turn_end_reason: TurnEndReason::EndTurn,
            usage: None,
        },
    ]);
    let mut agent = Agent::new(model, TestTools::new(vec![tool]), Arc::new(NullEventSink));
    agent.set_checkpoint(Some(Box::new(|state| {
        if matches!(state.history().last(), Some(Message::ToolResults { .. })) {
            Err("disk full".into())
        } else {
            Ok(())
        }
    })));
    let error = interact(
        &mut agent,
        vec![Content::Text {
            text: "task".into(),
        }],
        CancelToken::new(),
    )
    .await
    .unwrap_err();
    assert!(matches!(error, AgentInteractionError::Checkpoint(_)));
    assert_eq!(starts.lock().unwrap().len(), 1);
    assert!(
        matches!(agent.history().last(), Some(Message::ToolResults { tool_use_results }) if !tool_use_results[0].is_error)
    );
    assert!(matches!(
        agent.state().pending_operation(),
        Some(PendingOperation::Generation { .. })
    ));
    agent.set_checkpoint(None);
    agent.continue_run(CancelToken::new()).await.unwrap();
    assert_eq!(starts.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn dropping_a_tool_step_requires_explicit_recovery_and_never_replays_it() {
    let starts = Arc::new(Mutex::new(Vec::new()));
    let tool = Arc::new(SlowService {
        name: "effect".into(),
        delay: Duration::from_secs(60),
        starts: starts.clone(),
        ends: Arc::new(Mutex::new(Vec::new())),
    });
    let model = ScriptedModel::new(vec![
        GenerateOutput {
            content: vec![],
            tool_uses: vec![ToolUse {
                name: "effect".into(),
                input: json!({}),
                thought_signature: None,
            }],
            turn_end_reason: TurnEndReason::ToolUse,
            usage: None,
        },
        GenerateOutput {
            content: vec![],
            tool_uses: vec![],
            turn_end_reason: TurnEndReason::EndTurn,
            usage: None,
        },
    ]);
    let mut agent = Agent::new(model, TestTools::new(vec![tool]), Arc::new(NullEventSink));
    agent
        .append_input(Message::UserMessage {
            content: vec![Content::Text {
                text: "task".into(),
            }],
        })
        .unwrap();
    agent.start_run().unwrap();
    assert!(agent.step(CancelToken::new()).await.unwrap().is_none());
    {
        let step = agent.step(CancelToken::new());
        futures::pin_mut!(step);
        assert!(futures::poll!(step).is_pending());
    }
    assert_eq!(starts.lock().unwrap().len(), 1);
    assert!(matches!(
        agent.continue_run(CancelToken::new()).await,
        Err(AgentInteractionError::State(StateError::Busy))
    ));
    agent.recover_interrupted().unwrap();
    assert!(agent.history().iter().any(|message| matches!(message, Message::ToolResults { tool_use_results } if tool_use_results[0].is_error)));
    agent.run(CancelToken::new()).await.unwrap();
    assert_eq!(starts.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn last_usage_sums_output_across_tool_round_trips() {
    let tool = Arc::new(SlowService {
        name: "fast".into(),
        delay: Duration::ZERO,
        starts: Arc::new(Mutex::new(Vec::new())),
        ends: Arc::new(Mutex::new(Vec::new())),
    });
    let tools = TestTools::new(vec![tool as Arc<dyn ToolExecutor>]);

    let model = ScriptedModel::new(vec![
        GenerateOutput {
            content: vec![],
            tool_uses: vec![ToolUse {
                name: "fast".into(),
                input: json!({}),
                thought_signature: None,
            }],
            turn_end_reason: TurnEndReason::ToolUse,
            usage: Some(TokenUsage {
                input_tokens: 1_000,
                output_tokens: 500,
                cached_input_tokens: 800,
            }),
        },
        GenerateOutput {
            content: vec![Content::Text {
                text: "done".into(),
            }],
            tool_uses: vec![],
            turn_end_reason: TurnEndReason::EndTurn,
            usage: Some(TokenUsage {
                input_tokens: 1_600,
                output_tokens: 20,
                cached_input_tokens: 900,
            }),
        },
    ]);

    let mut agent = Agent::new(model, tools, Arc::new(NullEventSink));
    interact(
        &mut agent,
        vec![Content::Text { text: "go".into() }],
        crate::CancelToken::new(),
    )
    .await
    .expect("interact");

    // Input side tracks the latest request (the live context); output sums the turn.
    let usage = agent.last_usage().expect("usage recorded");
    assert_eq!(usage.input_tokens, 1_600);
    assert_eq!(usage.cached_input_tokens, 900);
    assert_eq!(usage.output_tokens, 520);
    assert_eq!(usage.context_tokens(), 1_600);
}

/// Slow generate stream: cancel mid-stream must return Cancelled quickly.
struct SlowStreamModel {
    delay: Duration,
    chunks: usize,
}

impl GenerativeModel for SlowStreamModel {
    fn generate(&self, _input: &[Message]) -> myco_model::AsyncStream<GenerationEvent> {
        let delay = self.delay;
        let chunks = self.chunks;
        // State machine: 0 = MessageStart, 1 = ContentStart, 2..chunks+1 = delayed
        // deltas, last = TurnEndReason.
        Box::pin(stream::unfold(0usize, move |step| {
            let delay = delay;
            async move {
                let last = chunks + 2;
                if step > last {
                    return None;
                }
                let part = if step == 0 {
                    MessagePart::MessageStart
                } else if step == 1 {
                    MessagePart::ContentStart(generative_model::ContentStart::Text { index: 0 })
                } else if step <= chunks + 1 {
                    tokio::time::sleep(delay).await;
                    MessagePart::ContentDelta(ContentDelta::Text {
                        index: 0,
                        delta: format!("chunk{}", step - 2),
                    })
                } else {
                    MessagePart::TurnEndReason(TurnEndReason::EndTurn)
                };
                Some((GenerationEvent::Part(part), step + 1))
            }
        }))
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancel_during_generate_returns_cancelled() {
    let tools = TestTools::new(vec![]);
    let model = Arc::new(SlowStreamModel {
        delay: Duration::from_millis(200),
        chunks: 20,
    });
    let mut agent = Agent::new(model, tools, Arc::new(NullEventSink));
    let cancel = crate::CancelToken::new();
    let cancel2 = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        cancel2.cancel();
    });

    let t0 = Instant::now();
    let err = interact(
        &mut agent,
        vec![Content::Text { text: "go".into() }],
        cancel,
    )
    .await
    .expect_err("should cancel");
    let elapsed = t0.elapsed();
    assert!(
        matches!(err, AgentInteractionError::Cancelled),
        "got {err:?}"
    );
    assert!(
        // Prompt under light load; allow CI / suite contention headroom.
        elapsed < Duration::from_secs(2),
        "cancel should be prompt, took {elapsed:?}"
    );
    // User message kept; no incomplete assistant pushed.
    assert_eq!(agent.history().len(), 1);
    assert!(matches!(agent.history()[0], Message::UserMessage { .. }));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancel_during_slow_tool_records_cancelled_result() {
    let starts = Arc::new(Mutex::new(Vec::new()));
    let ends = Arc::new(Mutex::new(Vec::new()));
    let slow = Arc::new(SlowService {
        name: "slow_a".into(),
        // Long enough that a delayed cancel still hits mid-tool under load.
        delay: Duration::from_secs(5),
        starts: starts.clone(),
        ends: ends.clone(),
    });
    let tools = TestTools::new(vec![slow as Arc<dyn ToolExecutor>]);
    let model = ScriptedModel::new(vec![GenerateOutput {
        content: vec![],
        tool_uses: vec![ToolUse {
            name: "slow_a".into(),
            input: json!({}),
            thought_signature: None,
        }],
        turn_end_reason: TurnEndReason::ToolUse,
        usage: None,
    }]);
    // No EndTurn scripted — cancel during tools must stop without another generate.
    let mut agent = Agent::new(model, tools, Arc::new(NullEventSink));
    let cancel = crate::CancelToken::new();
    let cancel2 = cancel.clone();
    let starts_bg = starts.clone();
    tokio::spawn(async move {
        // Cancel only after the tool has started (not a fixed sleep race).
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if !starts_bg.lock().unwrap().is_empty() {
                break;
            }
            if Instant::now() > deadline {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        cancel2.cancel();
    });

    let t0 = Instant::now();
    let err = interact(
        &mut agent,
        vec![Content::Text { text: "run".into() }],
        cancel,
    )
    .await
    .expect_err("should cancel");
    let elapsed = t0.elapsed();
    assert!(matches!(err, AgentInteractionError::Cancelled));
    // A cancel-ignoring tool is abandoned after CANCEL_TOOL_GRACE — the
    // turn must end well before the tool's own 5s delay.
    assert!(
        elapsed < Duration::from_secs(4),
        "should wait only the cancel grace, not the full tool delay, took {elapsed:?}"
    );

    // user + assistant(tool_use) + tool_results (cancelled)
    let history = agent.history();
    assert_eq!(history.len(), 3);
    match &history[2] {
        Message::ToolResults { tool_use_results } => {
            assert_eq!(tool_use_results.len(), 1);
            assert!(tool_use_results[0].is_error);
            let text = result_text(&tool_use_results[0]);
            assert!(text.contains("effects are unknown"), "{text}");
        }
        other => panic!("expected ToolResults, got {other:?}"),
    }
}

#[tokio::test]
async fn generate_error_after_tool_results_keeps_well_formed_history() {
    let slow = Arc::new(SlowService {
        name: "slow_a".into(),
        delay: Duration::from_millis(1),
        starts: Arc::new(Mutex::new(Vec::new())),
        ends: Arc::new(Mutex::new(Vec::new())),
    });
    let tools = TestTools::new(vec![slow as Arc<dyn ToolExecutor>]);
    let model = ScriptedModel::new(vec![GenerateOutput {
        content: vec![],
        tool_uses: vec![ToolUse {
            name: "slow_a".into(),
            input: json!({}),
            thought_signature: None,
        }],
        turn_end_reason: TurnEndReason::ToolUse,
        usage: None,
    }])
    .then_fail(GenerateError::ExecutionError(
        "provider 500 after tools".into(),
    ));
    let mut agent = Agent::new(model, tools, Arc::new(NullEventSink));
    let err = interact(
        &mut agent,
        vec![Content::Text {
            text: "run tool then fail".into(),
        }],
        crate::CancelToken::new(),
    )
    .await
    .expect_err("second generate should fail");
    assert!(
        matches!(err, AgentInteractionError::GenerateError(_)),
        "got {err:?}"
    );

    // user + assistant(tool_use) + tool_results — no incomplete assistant.
    let history = agent.history();
    assert_eq!(history.len(), 3, "history={history:?}");
    assert!(matches!(history[0], Message::UserMessage { .. }));
    match &history[1] {
        Message::AssistantMessage {
            tool_uses,
            turn_end_reason,
            ..
        } => {
            assert_eq!(tool_uses.len(), 1);
            assert_eq!(tool_uses[0].name, "slow_a");
            assert_eq!(*turn_end_reason, Some(TurnEndReason::ToolUse));
        }
        other => panic!("expected assistant tool_use, got {other:?}"),
    }
    match &history[2] {
        Message::ToolResults { tool_use_results } => {
            assert_eq!(tool_use_results.len(), 1);
            assert!(!tool_use_results[0].is_error);
        }
        other => panic!("expected ToolResults, got {other:?}"),
    }
}

#[tokio::test]
async fn generate_error_before_assistant_keeps_only_user() {
    let tools = TestTools::new(vec![]);
    let model = ScriptedModel::new(vec![]).then_fail(GenerateError::ExecutionError(
        "boom on first generate".into(),
    ));
    let mut agent = Agent::new(model, tools, Arc::new(NullEventSink));
    let err = interact(
        &mut agent,
        vec![Content::Text { text: "hi".into() }],
        crate::CancelToken::new(),
    )
    .await
    .expect_err("generate should fail");
    assert!(matches!(err, AgentInteractionError::GenerateError(_)));
    assert_eq!(agent.history().len(), 1);
    assert!(matches!(agent.history()[0], Message::UserMessage { .. }));
}

/// A tool_use stop with zero streamed tool uses is an invalid draft.
#[tokio::test]
async fn tool_use_stop_with_zero_tool_uses_errors_not_loops() {
    let tools = TestTools::new(vec![]);
    let model = ScriptedModel::new(vec![GenerateOutput {
        content: vec![Content::Text { text: "hmm".into() }],
        tool_uses: vec![],
        turn_end_reason: TurnEndReason::ToolUse,
        usage: None,
    }]);
    let mut agent = Agent::new(model, tools, Arc::new(NullEventSink));
    agent.set_retry_policy(RetryPolicy {
        max_attempts: 1,
        ..Default::default()
    });
    let err = interact(
        &mut agent,
        vec![Content::Text { text: "hi".into() }],
        crate::CancelToken::new(),
    )
    .await
    .expect_err("malformed turn should error");
    assert!(matches!(err, AgentInteractionError::GenerateError(_)));
    assert_eq!(agent.history().len(), 1);
    assert!(matches!(agent.history()[0], Message::UserMessage { .. }));
}

/// A truncated call still needs a result before generation can continue,
/// including when the executor rejects it.
#[tokio::test]
async fn max_tokens_mid_tool_call_answers_the_dangling_tool_use_and_resumes() {
    let tools = TestTools::new(vec![]);
    let model = ScriptedModel::new(vec![
        GenerateOutput {
            content: vec![Content::Text {
                text: "let me check".into(),
            }],
            tool_uses: vec![ToolUse {
                name: "unavailable".into(),
                input: serde_json::json!({}),
                thought_signature: None,
            }],
            turn_end_reason: TurnEndReason::MaxTokens,
            usage: None,
        },
        GenerateOutput {
            content: vec![Content::Text { text: "ok".into() }],
            tool_uses: vec![],
            turn_end_reason: TurnEndReason::EndTurn,
            usage: None,
        },
    ]);
    let mut agent = Agent::new(model, tools, Arc::new(NullEventSink));
    let reply = interact(
        &mut agent,
        vec![Content::Text { text: "hi".into() }],
        crate::CancelToken::new(),
    )
    .await
    .expect("turn should resume through the truncation, not error");

    // The truncation is resumed inside the same turn, so the caller gets the
    // continuation rather than the partial answer that preceded the tool call.
    assert!(matches!(&reply[0], Content::Text { text } if text == "ok"));

    // user + assistant(tool_use) + tool_results + assistant — the tool_use is
    // answered, and the resumed generate appended its reply to the same turn.
    assert_eq!(agent.history().len(), 4);
    match &agent.history()[2] {
        Message::ToolResults { tool_use_results } => {
            assert_eq!(tool_use_results.len(), 1);
            assert!(tool_use_results[0].is_error);
            let text = result_text(&tool_use_results[0]);
            assert!(text.contains("unknown tool 'unavailable'"), "text={text}");
        }
        other => panic!("expected ToolResults, got {other:?}"),
    }
}

/// A `max_tokens` stop with no tool calls leaves history on the assistant's
/// cut-off message, which providers reject as a prefill. The turn resumes by
/// asking for the rest in a user turn — the one continuation every provider
/// accepts — so a truncated sentence finishes instead of dead-ending.
#[tokio::test]
async fn max_tokens_without_tool_calls_resumes_with_a_continue_turn() {
    let tools = TestTools::new(vec![]);
    let model = ScriptedModel::new(vec![
        GenerateOutput {
            content: vec![Content::Text {
                text: "half a sen".into(),
            }],
            tool_uses: vec![],
            turn_end_reason: TurnEndReason::MaxTokens,
            usage: None,
        },
        GenerateOutput {
            content: vec![Content::Text {
                text: "tence.".into(),
            }],
            tool_uses: vec![],
            turn_end_reason: TurnEndReason::EndTurn,
            usage: None,
        },
    ]);
    let mut agent = Agent::new(model, tools, Arc::new(NullEventSink));
    let reply = interact(
        &mut agent,
        vec![Content::Text { text: "hi".into() }],
        crate::CancelToken::new(),
    )
    .await
    .expect("turn should resume through the truncation");

    assert!(matches!(&reply[0], Content::Text { text } if text == "tence."));

    // user + assistant(truncated) + user(continue) + assistant(rest).
    assert_eq!(agent.history().len(), 4);
    match &agent.history()[2] {
        Message::UserMessage { content } => match content.as_slice() {
            [Content::System { text, kind, .. }] => {
                assert_eq!(text, CONTINUE_PROMPT);
                assert_eq!(kind, "continuation");
            }
            other => panic!("expected one text block, got {other:?}"),
        },
        other => panic!("expected the continuation user turn, got {other:?}"),
    }
}

/// A model whose output cap is too low truncates every turn. Resuming is
/// capped so an unattended run stops instead of spending the night
/// re-truncating; the turn still ends cleanly rather than erroring.
#[tokio::test]
async fn consecutive_max_tokens_resumes_are_bounded() {
    const CAP: u32 = 2;
    let tools = TestTools::new(vec![]);
    let truncated_with_tool_call = || GenerateOutput {
        content: vec![Content::Text {
            text: "still going".into(),
        }],
        tool_uses: vec![ToolUse {
            name: "bash".into(),
            input: serde_json::json!({}),
            thought_signature: None,
        }],
        turn_end_reason: TurnEndReason::MaxTokens,
        usage: None,
    };
    // One more script than the cap can consume, so the surplus proves the
    // loop stopped on the cap rather than on an exhausted script list.
    let scripts = (0..CAP + 2).map(|_| truncated_with_tool_call()).collect();
    let model = ScriptedModel::new(scripts);
    let mut agent = Agent::new(model.clone(), tools, Arc::new(NullEventSink));
    agent.set_max_truncated_resumes(CAP);
    interact(
        &mut agent,
        vec![Content::Text { text: "hi".into() }],
        crate::CancelToken::new(),
    )
    .await
    .expect("turn should hand back once the cap is hit, not error");

    // The initial generate plus CAP resumes.
    assert_eq!(model.remaining() as u32, 1);
}

/// `max_truncated_resumes = 0` is the opt-out: the turn hands back the
/// partial answer, exactly as it did before resuming existed.
#[tokio::test]
async fn zero_max_truncated_resumes_hands_back_the_partial_answer() {
    let tools = TestTools::new(vec![]);
    let model = ScriptedModel::new(vec![
        GenerateOutput {
            content: vec![Content::Text {
                text: "half a sen".into(),
            }],
            tool_uses: vec![],
            turn_end_reason: TurnEndReason::MaxTokens,
            usage: None,
        },
        GenerateOutput {
            content: vec![Content::Text {
                text: "unused".into(),
            }],
            tool_uses: vec![],
            turn_end_reason: TurnEndReason::EndTurn,
            usage: None,
        },
    ]);
    let mut agent = Agent::new(model.clone(), tools, Arc::new(NullEventSink));
    agent.set_max_truncated_resumes(0);
    let reply = interact(
        &mut agent,
        vec![Content::Text { text: "hi".into() }],
        crate::CancelToken::new(),
    )
    .await
    .expect("turn should hand back the partial answer");

    assert!(matches!(&reply[0], Content::Text { text } if text == "half a sen"));
    // user + assistant only: no continuation turn, no second generate.
    assert_eq!(agent.history().len(), 2);
    assert_eq!(model.remaining(), 1, "second script must stay unconsumed");
}

/// A turn that ends cleanly with no tool calls gains no ToolResults message.
#[tokio::test]
async fn plain_end_turn_pushes_no_tool_results() {
    let tools = TestTools::new(vec![]);
    let model = ScriptedModel::new(vec![GenerateOutput {
        content: vec![Content::Text { text: "hi".into() }],
        tool_uses: vec![],
        turn_end_reason: TurnEndReason::EndTurn,
        usage: None,
    }]);
    let mut agent = Agent::new(model, tools, Arc::new(NullEventSink));
    interact(
        &mut agent,
        vec![Content::Text { text: "hi".into() }],
        crate::CancelToken::new(),
    )
    .await
    .expect("turn should succeed");
    assert_eq!(agent.history().len(), 2);
}

/// An oversized request is a property of the history, so the top-level
/// error must say the last message has to come out — not "try again".
/// (The rewind contract itself is proven by
/// `rewind_drops_the_whole_turn_and_keeps_earlier_ones`.)
#[tokio::test]
async fn oversized_request_reports_omit_last_message() {
    let tools = TestTools::new(vec![]);
    let model =
        ScriptedModel::new(vec![]).then_fail(GenerateError::RequestTooLargeError("42 MiB".into()));
    let mut agent = Agent::new(model, tools, Arc::new(NullEventSink));
    let err = interact(
        &mut agent,
        vec![Content::Image {
            source: "data:image/png;base64,AAAA".into(),
        }],
        crate::CancelToken::new(),
    )
    .await
    .expect_err("oversized request should fail");

    assert_eq!(err.recovery(), Recovery::OmitLastMessage);
}

/// History is well-formed after a cancel, so the same turn can be re-sent.
/// (Generate-error recovery mapping is proven in `generative_model` tests.)
#[test]
fn cancelled_interaction_is_retryable() {
    assert_eq!(AgentInteractionError::Cancelled.recovery(), Recovery::Retry);
}

/// Rewinding mid-turn drops the assistant/tool messages that followed the
/// user message too — the remaining prefix must end where the *previous*
/// turn ended, or the next request is malformed.
#[tokio::test]
async fn truncating_context_drops_the_whole_turn_and_keeps_earlier_ones() {
    let tools = TestTools::new(vec![]);
    let model = ScriptedModel::new(vec![]);
    let mut agent = Agent::new(model, tools, Arc::new(NullEventSink));
    agent
        .replace_context(
            vec![
                user("first"),
                assistant("ok"),
                user("second"),
                assistant_tool(None, "noop", json!({})),
                tool_results(&["done"]),
            ],
            None,
        )
        .unwrap();

    let dropped = agent.truncate_history(2).unwrap();
    assert!(
        matches!(&dropped[0], Message::UserMessage { content } if matches!(&content[0], Content::Text { text } if text == "second"))
    );

    let history = agent.history();
    assert_eq!(history.len(), 2);
    assert!(matches!(history[0], Message::UserMessage { .. }));
    assert!(matches!(history[1], Message::AssistantMessage { .. }));
}

/// Simulate crash after tools: persist history, new agent + model resumes and ends turn.
#[tokio::test]
async fn resume_after_tools_mid_turn_continues_cleanly() {
    // The well-formed mid-turn snapshot a checkpoint would have persisted
    // before the crash: user + assistant(tool_use) + matching tool_results
    // (the shape `generate_error_after_tool_results_keeps_well_formed_history`
    // proves the agent leaves behind).
    let snapshot = vec![
        user("mid turn"),
        assistant_tool(None, "slow_a", json!({})),
        tool_results(&["done:slow_a"]),
    ];

    // "Resume": new agent, same well-formed history, model only needs EndTurn.
    let tools = TestTools::new(vec![]);
    let resume_model = ScriptedModel::new(vec![GenerateOutput {
        content: vec![Content::Text {
            text: "recovered".into(),
        }],
        tool_uses: vec![],
        turn_end_reason: TurnEndReason::EndTurn,
        usage: None,
    }]);
    let mut resumed = Agent::new(resume_model, tools, Arc::new(NullEventSink));
    resumed.replace_context(snapshot, None).unwrap();

    // Continue by interacting with a follow-up user message (CLI would re-prompt);
    // history already has tool_results so a fresh user turn is the normal path.
    // Also verify the restored context is well-formed for provider requests by
    // checking the model can complete a new turn on top.
    let reply = interact(
        &mut resumed,
        vec![Content::Text {
            text: "continue".into(),
        }],
        crate::CancelToken::new(),
    )
    .await
    .expect("resume interact");
    assert_eq!(reply.len(), 1);
    match &reply[0] {
        Content::Text { text } => assert_eq!(text, "recovered"),
        other => panic!("expected text, got {other:?}"),
    }

    let history = resumed.history();
    // prior 3 + new user + new assistant
    assert_eq!(history.len(), 5);
    assert!(matches!(history[0], Message::UserMessage { .. }));
    assert!(matches!(history[1], Message::AssistantMessage { .. }));
    assert!(matches!(history[2], Message::ToolResults { .. }));
    assert!(matches!(history[3], Message::UserMessage { .. }));
    assert!(matches!(history[4], Message::AssistantMessage { .. }));
}
