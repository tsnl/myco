//! Bounded auxiliary inference with a separate, parent-linked saved history.
//! The caller supplies capabilities and retry policy and remains responsible for
//! interpreting the result. This runner never installs a result in the parent.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::agent::{Agent, AgentInteractionError, EventSink, ToolExecutor, TraceContext};
use crate::core::{AsyncStream, CancelToken, uuid_simple_hex};
use crate::generative_model::{
    self, CatalogModel, Content, GenerateError, GenerationEvent, GenerationFailure,
    GenerativeModel, GenerativeModelConfig, Message, RetryPolicy,
};
use crate::session::{ActiveSession, Session, SessionKind, SessionWriteLock};

//
// Requests and outcomes
//

pub(super) struct AuxiliaryTask {
    pub kind: SessionKind,
    pub parent_session_id: String,
    pub title: String,
    pub system_prompt: String,
    pub input: Vec<Content>,
    pub tools: Arc<dyn ToolExecutor>,
    pub max_requests: usize,
    /// The role knows which configuration setting can increase its budget.
    pub request_limit_error: String,
    pub retry: RetryPolicy,
}

#[derive(Debug)]
pub(super) enum AuxiliaryError {
    Cancelled,
    Failed(String),
}

//
// Saved worker lifecycle
//

pub(super) async fn run(
    task: AuxiliaryTask,
    catalog: &CatalogModel,
    cancel: CancelToken,
    wrap_model: impl FnOnce(Arc<dyn GenerativeModel>) -> Arc<dyn GenerativeModel>,
    sink: Arc<dyn EventSink>,
) -> Result<Vec<Content>, AuxiliaryError> {
    let worker_id = uuid::Uuid::new_v4();
    let (session, _writer) = saved_session(&task, catalog, worker_id)?;
    let model = generative_model::new(GenerativeModelConfig {
        model: catalog.spec.clone(),
        tools: task.tools.tool_specs(),
        system_prompt: task.system_prompt,
        backend_config: catalog.backend.clone(),
    })
    .map_err(|error| AuxiliaryError::Failed(format!("failed to create model: {error:?}")))?;
    let model = crate::core::image_store::with_images(
        wrap_model(model),
        crate::core::image_store::ImageStore::for_profile().map_err(AuxiliaryError::Failed)?,
        catalog.spec.max_image_base64_bytes,
    );
    let context = TraceContext {
        agent_id: worker_id,
        depth: 1,
        session_id: Some(session.id()),
        thread_id: Some(session.with(|session| session.active_thread().id.clone())),
    };
    let mut worker = Agent::with_context(
        Arc::new(BoundedModel {
            inner: model,
            requests: AtomicUsize::new(0),
            limit: task.max_requests,
            exhausted: task.request_limit_error,
        }),
        task.tools,
        sink,
        context,
    );
    worker.set_retry_policy(task.retry);
    worker.set_context_window_tokens(catalog.spec.context_window_tokens);
    worker.set_max_truncated_resumes(catalog.spec.max_truncated_resumes);
    super::wire_checkpoint(&mut worker, &session);
    let result = run_worker(&mut worker, task.input, cancel).await;
    // Even failed or cancelled work must leave its observations inspectable.
    // Persistence failure takes precedence over a model result or cancellation.
    super::persist_session(&worker, &session, true).map_err(|error| {
        AuxiliaryError::Failed(format!(
            "could not save {} worker state: {error}",
            task.kind
        ))
    })?;
    result
}

fn saved_session(
    task: &AuxiliaryTask,
    catalog: &CatalogModel,
    worker_id: uuid::Uuid,
) -> Result<(ActiveSession, SessionWriteLock), AuxiliaryError> {
    if task.kind.is_user() {
        return Err(AuxiliaryError::Failed(
            "auxiliary sessions must be hidden".into(),
        ));
    }
    let mut session = Session::new_hidden(
        catalog.spec.key.clone(),
        uuid_simple_hex(worker_id),
        task.kind,
        Some(task.parent_session_id.clone()),
    );
    // The child is discoverable on disk while running. Its lease prevents a
    // browser or another process from opening a competing writer.
    let writer = SessionWriteLock::acquire(&session.id).map_err(|error| {
        AuxiliaryError::Failed(format!(
            "could not lock {} worker session: {error}",
            task.kind
        ))
    })?;
    session.title = Some(task.title.clone());
    session.save().map_err(|error| {
        AuxiliaryError::Failed(format!(
            "could not save {} worker session: {error}",
            task.kind
        ))
    })?;
    Ok((ActiveSession::new(session), writer))
}

async fn run_worker(
    worker: &mut Agent,
    input: Vec<Content>,
    cancel: CancelToken,
) -> Result<Vec<Content>, AuxiliaryError> {
    super::interact(worker, input, cancel)
        .await
        .map_err(|error| match error {
            AgentInteractionError::Cancelled => AuxiliaryError::Cancelled,
            error => AuxiliaryError::Failed(format!("worker failed: {error}")),
        })
}

//
// Provider request budget
//

struct BoundedModel {
    inner: Arc<dyn GenerativeModel>,
    requests: AtomicUsize,
    limit: usize,
    exhausted: String,
}

impl GenerativeModel for BoundedModel {
    fn generate(&self, input: &[Message]) -> AsyncStream<GenerationEvent> {
        if self.requests.fetch_add(1, Ordering::SeqCst) >= self.limit {
            let failure =
                GenerationFailure::terminal(GenerateError::ExecutionError(self.exhausted.clone()));
            return Box::pin(futures::stream::once(async {
                GenerationEvent::Failure(failure)
            }));
        }
        self.inner.generate(input)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::NullEventSink;
    use crate::core::Async;
    use crate::generative_model::{GenerateOutput, ToolResult, ToolSpec, ToolUse, TurnEndReason};
    use crate::session::SessionLockError;
    use crate::test_support::{ScriptedModel, temp_home};
    use futures::StreamExt;
    use serde_json::json;

    struct NoTools;

    impl ToolExecutor for NoTools {
        fn tool_specs(&self) -> Vec<ToolSpec> {
            vec![]
        }

        fn dispatch(
            self: Arc<Self>,
            _: ToolUse,
            _: CancelToken,
            _: CancelToken,
        ) -> Async<ToolResult> {
            Box::pin(async { ToolResult::err("this worker has no tools") })
        }
    }

    fn input() -> Vec<Content> {
        vec![Content::Text {
            text: "inspect the supplied task".into(),
        }]
    }

    #[tokio::test]
    async fn repeated_tool_requests_stop_at_the_model_budget() {
        let output = GenerateOutput {
            content: vec![],
            tool_uses: vec![ToolUse {
                name: "bash".into(),
                input: json!({}),
            }],
            turn_end_reason: TurnEndReason::ToolUse,
            usage: None,
        };
        let inner = ScriptedModel::new(vec![output; 3]);
        let model = Arc::new(BoundedModel {
            inner: inner.clone(),
            requests: AtomicUsize::new(0),
            limit: 2,
            exhausted: "worker reached its 2-request limit".into(),
        });
        let mut worker = Agent::new(model, Arc::new(NoTools), Arc::new(NullEventSink));
        let result = run_worker(&mut worker, input(), CancelToken::new()).await;
        assert!(
            matches!(result, Err(AuxiliaryError::Failed(text)) if text.contains("2-request limit"))
        );
        assert_eq!(inner.remaining(), 1);
        crate::agent::validate_context(worker.history()).unwrap();
        assert!(worker.state().pending_operation().is_none());
    }

    #[tokio::test]
    async fn worker_checkpoint_failure_stops_before_model_or_tool_work() {
        let inner = ScriptedModel::new(vec![]);
        let mut worker = Agent::new(inner, Arc::new(NoTools), Arc::new(NullEventSink));
        worker.set_checkpoint(Some(Box::new(|_| Err("storage unavailable".into()))));
        let result = run_worker(&mut worker, input(), CancelToken::new()).await;
        assert!(
            matches!(result, Err(AuxiliaryError::Failed(text)) if text.contains("storage unavailable"))
        );
        assert!(worker.checkpoint_failed());
        assert!(worker.state().pending_operation().is_none());
    }

    #[tokio::test]
    async fn retries_count_towards_the_same_request_budget() {
        struct Retry;
        impl GenerativeModel for Retry {
            fn generate(&self, _: &[Message]) -> AsyncStream<GenerationEvent> {
                Box::pin(futures::stream::once(async {
                    GenerationEvent::Failure(GenerationFailure::transient(
                        GenerateError::ExecutionError("retry".into()),
                        None,
                    ))
                }))
            }
        }
        let model = BoundedModel {
            inner: Arc::new(Retry),
            requests: AtomicUsize::new(0),
            limit: 1,
            exhausted: "worker reached its 1-request limit".into(),
        };
        assert!(
            matches!(model.generate(&[]).next().await, Some(GenerationEvent::Failure(failure)) if failure.retryable)
        );
        assert!(
            matches!(model.generate(&[]).next().await, Some(GenerationEvent::Failure(failure)) if !failure.retryable)
        );
    }

    #[tokio::test]
    async fn user_cancel_settles_pending_generation() {
        struct Pending(Arc<tokio::sync::Notify>);
        impl GenerativeModel for Pending {
            fn generate(&self, _: &[Message]) -> AsyncStream<GenerationEvent> {
                self.0.notify_one();
                Box::pin(futures::stream::pending())
            }
        }
        let started = Arc::new(tokio::sync::Notify::new());
        let mut worker = Agent::new(
            Arc::new(Pending(started.clone())),
            Arc::new(NoTools),
            Arc::new(NullEventSink),
        );
        let cancel = CancelToken::new();
        let (result, ()) = tokio::join!(run_worker(&mut worker, input(), cancel.clone()), async {
            started.notified().await;
            cancel.cancel();
        });
        assert!(matches!(result, Err(AuxiliaryError::Cancelled)));
        assert!(worker.state().pending_operation().is_none());
        crate::agent::validate_context(worker.history()).unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn slow_generation_can_finish_after_two_minutes() {
        struct Slow;
        impl GenerativeModel for Slow {
            fn generate(&self, _: &[Message]) -> AsyncStream<GenerationEvent> {
                Box::pin(
                    futures::stream::once(async {
                        tokio::time::sleep(std::time::Duration::from_secs(121)).await;
                        ScriptedModel::new(vec![GenerateOutput {
                            content: vec![Content::Text {
                                text: "summary ready".into(),
                            }],
                            tool_uses: vec![],
                            turn_end_reason: TurnEndReason::EndTurn,
                            usage: None,
                        }])
                        .generate(&[])
                    })
                    .flatten(),
                )
            }
        }
        let mut worker = Agent::new(Arc::new(Slow), Arc::new(NoTools), Arc::new(NullEventSink));
        run_worker(&mut worker, input(), CancelToken::new())
            .await
            .unwrap();
        assert!(worker.state().pending_operation().is_none());
        crate::agent::validate_context(worker.history()).unwrap();
    }

    fn catalog() -> CatalogModel {
        crate::Config::resolve_with(
            crate::ConfigUserSettings {
                config_path: Some("/unused/config.toml".into()),
                ..Default::default()
            },
            |_| None,
            |_, _| {
                Ok(toml::from_str(
                    r#"
                [models.test]
                protocol = "openai-responses"
                base_url = "http://127.0.0.1:1"
                auth = { source = "none" }
                context_window = 100000
            "#,
                )
                .unwrap())
            },
            || Ok(vec![]),
            |_| unreachable!(),
        )
        .unwrap()
        .models
        .get("test")
        .unwrap()
        .clone()
    }

    fn task(parent: &Session) -> AuxiliaryTask {
        AuxiliaryTask {
            kind: SessionKind::Subagent,
            parent_session_id: parent.id.clone(),
            title: "inspect task".into(),
            system_prompt: "Inspect supplied data with no tools.".into(),
            input: input(),
            tools: Arc::new(NoTools),
            max_requests: 2,
            request_limit_error: "worker reached its 2-request limit".into(),
            retry: RetryPolicy::default(),
        }
    }

    fn child_of(parent: &Session) -> Session {
        let children: Vec<_> = crate::session::list_all_sessions_including_hidden()
            .unwrap()
            .into_iter()
            .filter(|session| session.parent_session_id.as_deref() == Some(&parent.id))
            .collect();
        assert_eq!(children.len(), 1);
        Session::load_by_id_or_prefix(&children[0].id).unwrap()
    }

    #[test]
    fn saved_child_has_separate_history_and_parent_identity_without_inheriting_auto_mode() {
        let _home = temp_home("auxiliary-child");
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            let mut parent = Session::new("test");
            parent.auto_continue = true;
            parent
                .active_thread_mut()
                .messages
                .push(crate::test_support::user("parent-only context"));
            parent.save().unwrap();
            let original = std::fs::read(parent.json_path()).unwrap();
            let model = ScriptedModel::new(vec![GenerateOutput {
                content: vec![Content::Text {
                    text: "inspected".into(),
                }],
                tool_uses: vec![],
                usage: None,
                turn_end_reason: TurnEndReason::EndTurn,
            }]);
            let output = run(
                task(&parent),
                &catalog(),
                CancelToken::new(),
                |_| model.clone(),
                Arc::new(NullEventSink),
            )
            .await
            .unwrap();
            let child = child_of(&parent);
            assert_eq!(
                output,
                vec![Content::Text {
                    text: "inspected".into()
                }]
            );
            assert_ne!(child.id, parent.id);
            assert_eq!(child.kind, SessionKind::Subagent);
            assert_eq!(child.title.as_deref(), Some("inspect task"));
            assert!(!child.auto_continue);
            assert_eq!(
                child.active_thread().messages,
                vec![
                    crate::test_support::user("inspect the supplied task"),
                    crate::test_support::assistant("inspected")
                ]
            );
            assert!(child.active_thread().pending_operation.is_none());
            assert_eq!(std::fs::read(parent.json_path()).unwrap(), original);
            assert_eq!(crate::session::list_all_sessions().unwrap().len(), 1);
            assert_eq!(model.remaining(), 0);
            assert!(SessionWriteLock::acquire(&child.id).is_ok());
        });
    }

    #[test]
    fn failed_final_save_cannot_return_successful_worker_output() {
        struct BlockFinalSave(std::path::PathBuf);
        impl EventSink for BlockFinalSave {
            fn emit(&self, event: crate::agent::AgentEvent) {
                if matches!(event, crate::agent::AgentEvent::TurnFinished { .. }) {
                    std::fs::rename(self.0.join("session"), self.0.join("saved-session")).unwrap();
                    std::fs::write(self.0.join("session"), "storage unavailable").unwrap();
                }
            }
        }
        let home = temp_home("auxiliary-final-save");
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            let parent = Session::new("test");
            parent.save().unwrap();
            let model = ScriptedModel::new(vec![GenerateOutput {
                content: vec![Content::Text { text: "must not return as successful".into() }],
                tool_uses: vec![], usage: None, turn_end_reason: TurnEndReason::EndTurn,
            }]);
            let result = run(task(&parent), &catalog(), CancelToken::new(), |_| model.clone(), Arc::new(BlockFinalSave(home.path().into()))).await;
            assert!(matches!(result, Err(AuxiliaryError::Failed(ref text)) if text.contains("could not save subagent worker state")), "{result:?}");
            assert_eq!(model.remaining(), 0);
            std::fs::remove_file(home.path().join("session")).unwrap();
            std::fs::rename(home.path().join("saved-session"), home.path().join("session")).unwrap();
            let child = child_of(&parent);
            assert_eq!(child.active_thread().messages.len(), 2);
            assert!(SessionWriteLock::acquire(&child.id).is_ok());
        });
    }

    #[test]
    fn worker_holds_its_child_writer_until_cancelled_history_is_saved() {
        struct Pending(Arc<tokio::sync::Notify>);
        impl GenerativeModel for Pending {
            fn generate(&self, _: &[Message]) -> AsyncStream<GenerationEvent> {
                self.0.notify_one();
                Box::pin(futures::stream::pending())
            }
        }
        let _home = temp_home("auxiliary-cancel");
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            let parent = Session::new("test");
            parent.save().unwrap();
            let started = Arc::new(tokio::sync::Notify::new());
            let model = Arc::new(Pending(started.clone()));
            let cancel = CancelToken::new();
            let catalog = catalog();
            let (result, ()) = tokio::join!(
                run(
                    task(&parent),
                    &catalog,
                    cancel.clone(),
                    |_| model,
                    Arc::new(NullEventSink)
                ),
                async {
                    started.notified().await;
                    let child = child_of(&parent);
                    assert!(matches!(
                        SessionWriteLock::acquire(&child.id),
                        Err(SessionLockError::Busy { .. })
                    ));
                    cancel.cancel();
                }
            );
            assert!(matches!(result, Err(AuxiliaryError::Cancelled)));
            let child = child_of(&parent);
            assert!(SessionWriteLock::acquire(&child.id).is_ok());
            assert!(child.active_thread().pending_operation.is_none());
            crate::agent::validate_context(&child.active_thread().messages).unwrap();
            assert_eq!(child.active_thread().messages.len(), 1);
            assert!(
                Session::load(&parent.json_path())
                    .unwrap()
                    .active_thread()
                    .messages
                    .is_empty()
            );
        });
    }
}
