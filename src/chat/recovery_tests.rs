//! Failed storage must pause effects without trapping a live session.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use chrono::Utc;
use serde_json::json;

use super::{CompactWorkerError, Compactor, SessionRunner};
use crate::agent::{Agent, AgentEvent, AgentInteractionError, EventSink, validate_context};
use crate::core::{Async, CancelToken, ModelInfo};
use crate::generative_model::{Content, GenerateOutput, Message, ToolUse, TurnEndReason};
use crate::session::{ActiveSession, CompactOutcome, Session, Thread, compact_thread};
use crate::test_support::{ScriptedModel, temp_home};
use crate::{Harness, SessionRuntime};

//
// Storage failure fixtures
//

struct BreakStore {
    home: PathBuf,
    before_tools: bool,
    broken: AtomicBool,
}

impl EventSink for BreakStore {
    fn emit(&self, event: AgentEvent) {
        let boundary = if self.before_tools {
            matches!(event, AgentEvent::GenerationFinished { .. })
        } else {
            matches!(event, AgentEvent::ToolFinished { .. })
        };
        if boundary && !self.broken.swap(true, Ordering::SeqCst) {
            std::fs::rename(self.home.join("session"), self.home.join("saved-store")).unwrap();
            std::fs::write(self.home.join("session"), "storage unavailable").unwrap();
        }
    }
}

struct Summarize;

impl Compactor for Summarize {
    fn compact(
        self: Arc<Self>,
        predecessor: Session,
        _cancel: CancelToken,
    ) -> Async<Result<(Thread, CompactOutcome), CompactWorkerError>> {
        Box::pin(async move {
            validate_context(&predecessor.active_thread().messages).unwrap();
            compact_thread(
                &predecessor,
                "Preserve the task and completed tool observations",
            )
            .map_err(CompactWorkerError::Failed)
        })
    }
}

fn output(calls: Vec<ToolUse>) -> GenerateOutput {
    GenerateOutput {
        content: vec![Content::Text {
            text: "progress".into(),
        }],
        turn_end_reason: if calls.is_empty() {
            TurnEndReason::EndTurn
        } else {
            TurnEndReason::ToolUse
        },
        tool_uses: calls,
        usage: None,
    }
}

async fn runner(home: &Path, before_tools: bool) -> (SessionRunner, Arc<ScriptedModel>) {
    let session = ActiveSession::new(Session::new("scripted"));
    let runtime = SessionRuntime::new(Harness::local_with_services(vec![]), session);
    let model = ScriptedModel::new(vec![
        output(vec![ToolUse {
            name: "bash".into(),
            input: json!({"command": format!("printf executed >> '{}'", home.join("effect").display())}),
        }]),
        output(vec![]),
    ]);
    let sink = Arc::new(BreakStore {
        home: home.into(),
        before_tools,
        broken: AtomicBool::new(false),
    });
    let agent = Agent::new(model.clone(), runtime.clone(), sink);
    let mut runner = SessionRunner::new(agent, runtime).await.unwrap();
    runner.set_compactor(Arc::new(Summarize), None);
    (runner, model)
}

async fn submit(runner: &mut SessionRunner) -> Result<(), AgentInteractionError> {
    runner
        .submit(
            vec![Content::Text {
                text: "continue the task".into(),
            }],
            Utc::now(),
            CancelToken::new(),
        )
        .await
        .result
        .map(|_| ())
}

//
// Recovery at user action boundaries
//

#[test]
fn repaired_storage_allows_new_input_compaction_and_model_changes_without_replaying_tools() {
    for before_tools in [true, false] {
        for action in ["submit", "compact", "model"] {
            let home = temp_home("recover-stopped-run");
            tokio::runtime::Runtime::new().unwrap().block_on(async {
                let (mut runner, model) = runner(home.path(), before_tools).await;
                assert!(matches!(
                    submit(&mut runner).await,
                    Err(AgentInteractionError::Checkpoint(_))
                ));
                assert!(!runner.agent().state().is_idle());
                assert_eq!(model.remaining(), 1);

                // Recovery itself must save before any fresh work can run.
                assert!(matches!(
                    runner.compact(CancelToken::new()).await,
                    Err(AgentInteractionError::Checkpoint(_))
                ));
                assert!(matches!(
                    submit(&mut runner).await,
                    Err(AgentInteractionError::Checkpoint(_))
                ));
                assert_eq!(model.remaining(), 1);
                std::fs::remove_file(home.path().join("session")).unwrap();
                std::fs::rename(home.path().join("saved-store"), home.path().join("session"))
                    .unwrap();

                match action {
                    "submit" => submit(&mut runner).await.unwrap(),
                    "compact" => {
                        runner.compact(CancelToken::new()).await.unwrap();
                    }
                    "model" => {
                        runner
                            .set_model(model.clone(), ModelInfo::named("replacement"))
                            .await
                            .unwrap();
                    }
                    _ => unreachable!(),
                }
                assert!(runner.agent().state().is_idle());
                assert_eq!(model.remaining(), usize::from(action != "submit"));
                assert_eq!(
                    std::fs::read_to_string(home.path().join("effect")).ok(),
                    (!before_tools).then(|| "executed".into())
                );
                let saved =
                    Session::load(&runner.runtime().session().snapshot().json_path()).unwrap();
                for thread in saved.threads() {
                    validate_context(&thread.messages).unwrap();
                    assert!(thread.pending_operation.is_none());
                }
                let history = &saved.threads()[0].messages;
                let results = history
                    .iter()
                    .find_map(|message| match message {
                        Message::ToolResults { tool_use_results } => Some(tool_use_results),
                        _ => None,
                    })
                    .unwrap();
                assert_eq!(results.len(), 1);
                assert_eq!(results[0].is_error, before_tools);
                if before_tools {
                    assert!(
                        serde_json::to_string(results)
                            .unwrap()
                            .contains("not executed")
                    );
                }
            });
        }
    }
}
