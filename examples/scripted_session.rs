//! Offline session/eval example: real tools, injected model and compactor,
//! automatic compaction, artifact grading, and restart without restoring handles.
//! Run with `cargo run --locked --example scripted_session`.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use myco::core::{Async, AsyncStream};
use myco::generative_model::{
    Content, ContentDelta, ContentStart, GenerationEvent, GenerativeModel, Message, MessagePart,
    TokenUsage, ToolUseDelta, ToolUseStart, TurnEndReason,
};
use myco::{
    ActiveSession, Agent, CancelToken, CompactOutcome, CompactWorkerError, Compactor, Harness,
    ModelInfo, NullEventSink, RuntimeRecord, Session, SessionRunner, SessionRuntime, Thread,
};
use serde_json::json;

struct ScriptedModel {
    steps: Mutex<VecDeque<Vec<MessagePart>>>,
    expect_restart: bool,
}

impl GenerativeModel for ScriptedModel {
    fn generate(&self, input: &[Message]) -> AsyncStream<GenerationEvent> {
        if self.expect_restart {
            let runtime = RuntimeRecord::latest(input).expect("model receives runtime context");
            assert!(runtime.resumed);
            assert_eq!(
                runtime.unavailable_after_restart[0]
                    .resources
                    .as_ref()
                    .unwrap()[0]
                    .id,
                "eval"
            );
        }
        let parts = self
            .steps
            .lock()
            .unwrap()
            .pop_front()
            .expect("unexpected extra generation");
        Box::pin(futures::stream::iter(
            parts.into_iter().map(GenerationEvent::Part),
        ))
    }
}

struct Summarizer;

impl Compactor for Summarizer {
    fn compact(
        self: Arc<Self>,
        predecessor: Session,
        _cancel: CancelToken,
    ) -> Async<Result<(Thread, CompactOutcome), CompactWorkerError>> {
        Box::pin(async move {
            myco::compact_thread(&predecessor, "Complete the arithmetic fixture and write its result. Keep the existing eval shell.")
                .map_err(CompactWorkerError::Failed)
        })
    }
}

fn step(input_tokens: u64, tool: Option<serde_json::Value>) -> Vec<MessagePart> {
    let mut parts = vec![MessagePart::MessageStart];
    let reason = if let Some(input) = tool {
        parts.push(MessagePart::ToolUseStart(ToolUseStart {
            index: 0,
            name: "bash".into(),
        }));
        parts.push(MessagePart::ToolUseDelta(ToolUseDelta {
            index: 0,
            input_json_delta: input.to_string(),
        }));
        TurnEndReason::ToolUse
    } else {
        parts.push(MessagePart::ContentStart(ContentStart::Text { index: 0 }));
        parts.push(MessagePart::ContentDelta(ContentDelta::Text {
            index: 0,
            delta: "42".into(),
        }));
        TurnEndReason::EndTurn
    };
    parts.push(MessagePart::Usage(TokenUsage {
        input_tokens,
        output_tokens: 5,
        cached_input_tokens: 0,
    }));
    parts.push(MessagePart::TurnEndReason(reason));
    parts
}

async fn evaluate(dir: PathBuf) -> Result<(), Box<dyn std::error::Error>> {
    let artifact = dir.join("answer.txt");
    let artifact_arg = artifact.to_string_lossy().replace('\'', "'\\''");
    let model = Arc::new(ScriptedModel {
        steps: Mutex::new(VecDeque::from([
            step(
                900,
                Some(
                    json!({"action":"start", "session_id":"eval", "command":"bash --noprofile --norc", "idle_ms":10, "timeout_ms":1000}),
                ),
            ),
            step(
                100,
                Some(
                    json!({"action":"write", "session_id":"eval", "stdin":format!("printf '%s\\n' $((6 * 7)) > '{}'\n", artifact_arg), "idle_ms":20, "timeout_ms":1000}),
                ),
            ),
            step(100, None),
        ])),
        expect_restart: false,
    });
    let session = ActiveSession::new(Session::new("fixture"));
    // Each eval case gets a separate home and runtime. Production callers that
    // share a store across processes also hold SessionWriteLock for the run.
    let runtime = SessionRuntime::new(Harness::local_with_services(vec![]), session.clone());
    let agent = Agent::new(model.clone(), runtime.clone(), Arc::new(NullEventSink));
    let mut runner = SessionRunner::new(agent, runtime).await?;
    runner.set_model(model, ModelInfo::named("fixture")).await?;
    runner.set_compactor(Arc::new(Summarizer), Some(800));
    let outcome = runner
        .submit(
            vec![Content::Text {
                text: "Compute six times seven and save the answer.".into(),
            }],
            chrono::Utc::now(),
            CancelToken::new(),
        )
        .await
        .result?;
    assert_eq!(std::fs::read_to_string(&artifact)?.trim(), "42");
    assert_eq!(session.snapshot().threads().len(), 2);
    assert_eq!(outcome.usage.unwrap().output_tokens, 15);
    let path = session.snapshot().json_path();
    drop(runner);

    let restored = ActiveSession::new(Session::load(&path)?);
    let runtime = SessionRuntime::new(Harness::local_with_services(vec![]), restored);
    let model = Arc::new(ScriptedModel {
        steps: Mutex::new(VecDeque::from([step(100, None)])),
        expect_restart: true,
    });
    let agent = Agent::new(model.clone(), runtime.clone(), Arc::new(NullEventSink));
    let mut runner = SessionRunner::new(agent, runtime).await?;
    runner
        .set_model(model, ModelInfo::named("restart-fixture"))
        .await?;
    runner.resume(CancelToken::new()).await.result?;
    println!(
        "{}",
        json!({"passed":true, "session":path, "artifact":artifact, "stop_reason":outcome.reason, "agent_usage":outcome.usage})
    );
    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let dir = std::env::temp_dir().join(format!("myco-eval-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir)?;
    // SAFETY: no threads exist yet; configure the isolated store before Tokio starts.
    unsafe {
        std::env::set_var("MYCO_HOME", &dir);
        std::env::set_var("MYCO_PROFILE", "default");
    }
    tokio::runtime::Runtime::new()?.block_on(evaluate(dir))
}
