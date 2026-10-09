//! Automatic retries retain the predecessor and any successfully produced summary.

use std::path::PathBuf;
use std::sync::{
    Mutex,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;

use super::*;
use crate::agent::NullEventSink;
use crate::harness::Harness;
use crate::session::{ActiveSession, compact_thread};
use crate::test_support::{ScriptedModel, temp_home};

#[derive(Default)]
struct Summarizer {
    calls: AtomicUsize,
    failures: usize,
    sources: Mutex<Vec<Session>>,
    successors: Mutex<Vec<String>>,
    break_store: Option<PathBuf>,
}

impl Compactor for Summarizer {
    fn compact(
        self: Arc<Self>,
        predecessor: Session,
        _: CancelToken,
    ) -> Async<Result<(Thread, CompactOutcome), CompactWorkerError>> {
        Box::pin(async move {
            let attempt = self.calls.fetch_add(1, Ordering::SeqCst);
            self.sources.lock().unwrap().push(predecessor.clone());
            if attempt < self.failures {
                return Err(CompactWorkerError::Failed("summarizer unavailable".into()));
            }
            let result = compact_thread(&predecessor, "Retain the original task").unwrap();
            self.successors.lock().unwrap().push(result.0.id.clone());
            if let Some(home) = &self.break_store {
                break_store(home);
            }
            Ok(result)
        })
    }
}

fn break_store(home: &std::path::Path) {
    std::fs::rename(home.join("session"), home.join("saved-store")).unwrap();
    std::fs::write(home.join("session"), "disk unavailable").unwrap();
}

fn repair_store(home: &std::path::Path) {
    std::fs::remove_file(home.join("session")).unwrap();
    std::fs::rename(home.join("saved-store"), home.join("session")).unwrap();
}

async fn setup(compactor: Arc<Summarizer>) -> SessionRunner {
    let session = ActiveSession::new(Session::new("scripted"));
    session.set_auto_continue(true).unwrap();
    let runtime = SessionRuntime::new(Harness::local_with_services(vec![]), session);
    let agent = Agent::new(
        ScriptedModel::new(vec![]),
        runtime.clone(),
        Arc::new(NullEventSink),
    );
    let mut runner = SessionRunner::new(agent, runtime).await.unwrap();
    runner
        .agent
        .append_input(Message::UserMessage {
            content: vec![Content::Text {
                text: "original task".into(),
            }],
        })
        .unwrap();
    runner.agent.start_run().unwrap();
    runner.set_compactor(compactor, None);
    runner
}

async fn compact(
    runner: &mut SessionRunner,
    mode: CompactionMode,
    cancel: CancelToken,
) -> Result<CompactOutcome, AgentInteractionError> {
    let session = runner.runtime.session().clone();
    let writer = session.writer().await;
    runner
        .workflow
        .compact(&mut runner.agent, &runner.runtime, &writer, cancel, mode)
        .await
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

#[test]
fn automatic_compaction_retries_keep_the_same_task_and_cap_each_wait_at_five_seconds() {
    let _home = temp_home("auto-compaction-retry");
    runtime().block_on(async {
        tokio::time::pause();
        for mode in [CompactionMode::Automatic, CompactionMode::RequestSize] {
            let summarizer = Arc::new(Summarizer {
                failures: 5,
                ..Default::default()
            });
            let mut runner = setup(summarizer.clone()).await;
            let delays = Arc::new(Mutex::new(Vec::new()));
            runner.set_observer(Arc::new({
                let delays = delays.clone();
                move |event| {
                    if let WorkflowEvent::Retrying { delay, .. } = event {
                        delays.lock().unwrap().push(delay);
                    }
                }
            }));
            compact(&mut runner, mode, CancelToken::new())
                .await
                .unwrap();
            assert_eq!(
                *delays.lock().unwrap(),
                [1, 2, 4, 5, 5].map(Duration::from_secs)
            );
            assert_eq!(summarizer.calls.load(Ordering::SeqCst), 6);
            let sources = summarizer.sources.lock().unwrap();
            let first = serde_json::to_value(&sources[0]).unwrap();
            assert!(
                sources
                    .iter()
                    .all(|source| serde_json::to_value(source).unwrap() == first)
            );
            let saved = Session::load(&runner.runtime.session().snapshot().json_path()).unwrap();
            assert_eq!(saved.threads().len(), 2);
            assert_eq!(
                saved.threads()[0]
                    .messages
                    .iter()
                    .filter(|message| message.is_user_turn())
                    .count(),
                1
            );
            assert!(runner.agent.state().pending_operation().is_some());
            assert!(!runner.workflow.auto_failed);
        }
    });
}

#[test]
fn compaction_save_retries_preserve_one_successor_and_reset_delay_for_each_stage() {
    let home = temp_home("auto-compaction-save-retry");
    runtime().block_on(async {
        tokio::time::pause();
        let summarizer = Arc::new(Summarizer {
            break_store: Some(home.path().to_owned()),
            ..Default::default()
        });
        let mut runner = setup(summarizer.clone()).await;
        let delays = Arc::new(Mutex::new(Vec::new()));
        runner.set_observer(Arc::new({
            let delays = delays.clone();
            let home = home.path().to_owned();
            move |event| {
                if let WorkflowEvent::Retrying { delay, error } = event {
                    assert!(error.contains("persist"), "{error}");
                    delays.lock().unwrap().push(delay);
                    repair_store(&home);
                }
            }
        }));
        break_store(home.path());
        let outcome = compact(&mut runner, CompactionMode::Automatic, CancelToken::new())
            .await
            .unwrap();
        assert_eq!(*delays.lock().unwrap(), [Duration::from_secs(1); 2]);
        assert_eq!(summarizer.calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            summarizer.successors.lock().unwrap().as_slice(),
            std::slice::from_ref(&outcome.successor_id)
        );
        let saved = Session::load(&runner.runtime.session().snapshot().json_path()).unwrap();
        assert_eq!(saved.threads().len(), 2);
        assert_eq!(saved.active_thread().id, outcome.successor_id);
        assert_eq!(
            runner.agent.context().thread_id.as_deref(),
            Some(outcome.successor_id.as_str())
        );
    });
}

#[test]
fn disabling_or_cancelling_compaction_retry_keeps_the_predecessor() {
    let _home = temp_home("auto-compaction-stop-retry");
    runtime().block_on(async {
        tokio::time::pause();
        for disable in [false, true] {
            let summarizer = Arc::new(Summarizer {
                failures: usize::MAX,
                ..Default::default()
            });
            let mut runner = setup(summarizer.clone()).await;
            let cancel = CancelToken::new();
            runner.set_observer(Arc::new({
                let cancel = cancel.clone();
                let session = runner.runtime.session().clone();
                move |event| {
                    if matches!(event, WorkflowEvent::Retrying { .. }) {
                        if disable {
                            session.set_auto_continue(false).unwrap();
                        } else {
                            cancel.cancel();
                        }
                    }
                }
            }));
            let error = compact(&mut runner, CompactionMode::Automatic, cancel)
                .await
                .unwrap_err();
            assert_eq!(matches!(error, AgentInteractionError::Cancelled), !disable);
            assert_eq!(summarizer.calls.load(Ordering::SeqCst), 1);
            assert_eq!(runner.runtime.session().snapshot().threads().len(), 1);
        }
    });
}

#[test]
fn manual_compaction_retains_bounded_policy_even_when_auto_continue_is_enabled() {
    let _home = temp_home("manual-compaction-policy");
    runtime().block_on(async {
        let summarizer = Arc::new(Summarizer {
            failures: usize::MAX,
            ..Default::default()
        });
        let mut runner = setup(summarizer.clone()).await;
        runner.agent.recover_interrupted().unwrap();
        assert!(matches!(
            compact(&mut runner, CompactionMode::Manual, CancelToken::new()).await,
            Err(AgentInteractionError::Compaction(_))
        ));
        assert_eq!(summarizer.calls.load(Ordering::SeqCst), 1);
        assert!(!summarizer.sources.lock().unwrap()[0].auto_continue);
        assert!(
            runner
                .runtime
                .session()
                .with(|session| session.auto_continue)
        );
    });
}

#[test]
fn repeated_oversized_requests_retry_without_creating_more_successor_threads() {
    let _home = temp_home("auto-compaction-size-retry");
    runtime().block_on(async {
        tokio::time::pause();
        let session = ActiveSession::new(Session::new("scripted"));
        session.set_auto_continue(true).unwrap();
        let runtime = SessionRuntime::new(Harness::local_with_services(vec![]), session);
        let model = ScriptedModel::from_results(
            (0..4)
                .map(|_| {
                    Err(GenerateError::RequestTooLargeError(
                        "still oversized".into(),
                    ))
                })
                .collect(),
        );
        let agent = Agent::new(model.clone(), runtime.clone(), Arc::new(NullEventSink));
        let mut runner = SessionRunner::new(agent, runtime).await.unwrap();
        let summarizer = Arc::new(Summarizer::default());
        runner.set_compactor(summarizer.clone(), None);
        let cancel = CancelToken::new();
        let retries = Arc::new(AtomicUsize::new(0));
        runner.set_observer(Arc::new({
            let cancel = cancel.clone();
            let retries = retries.clone();
            move |event| {
                if matches!(event, WorkflowEvent::Retrying { .. })
                    && retries.fetch_add(1, Ordering::SeqCst) == 2
                {
                    cancel.cancel();
                }
            }
        }));
        let result = runner
            .submit(
                vec![Content::Text {
                    text: "original task".into(),
                }],
                Utc::now(),
                cancel,
            )
            .await;
        assert!(matches!(
            result.result,
            Err(AgentInteractionError::Cancelled)
        ));
        assert_eq!(retries.load(Ordering::SeqCst), 3);
        assert_eq!(model.remaining(), 0);
        assert_eq!(summarizer.calls.load(Ordering::SeqCst), 1);
        assert_eq!(runner.runtime.session().snapshot().threads().len(), 2);
    });
}
