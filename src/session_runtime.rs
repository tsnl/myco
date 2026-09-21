//! Session-bound tool ownership and agent binding, independent of the frontend.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use uuid::Uuid;

use crate::agent::{Agent, BeforeGenerationNotice, ToolExecutor};
use crate::core::{Async, CancelToken, ModelInfo};
use crate::generative_model::{Content, Message, ToolResult, ToolSpec, ToolUse};
use crate::harness::Harness;
use crate::prelude::{self, PreludeEntry};
use crate::session::ActiveSession;

mod lifecycle;
pub use lifecycle::RuntimeRecord;

struct PreludeObservation {
    entries: Vec<PreludeEntry>,
    thread_id: Option<String>,
    notice: Option<(usize, String)>,
}

/// Track changes relative to the exact prelude used to build the agent's model.
/// The observer follows that model across session switches and thread changes.
pub fn prelude_change_notices(initial: Vec<PreludeEntry>) -> BeforeGenerationNotice {
    let dir = prelude::dir().ok();
    let observed = Arc::new(Mutex::new(PreludeObservation {
        entries: initial,
        thread_id: None,
        notice: None,
    }));
    Box::new(move |context, history| {
        let observed = observed.clone();
        let (before, lost_notice) = {
            let observed = observed.lock().unwrap();
            let retained = observed.notice.as_ref().is_none_or(|(index, notice)| {
                let last = match history.get(*index) {
                    Some(Message::UserMessage { content }) => content.last(),
                    Some(Message::ToolResults { tool_use_results }) => tool_use_results
                        .last()
                        .and_then(|result| result.content.last()),
                    _ => None,
                };
                observed.thread_id == context.thread_id
                    && matches!(last, Some(Content::Text { text }) if text == notice)
            });
            (observed.entries.clone(), !retained)
        };
        let thread_id = context.thread_id.clone();
        let index = match history.last() {
            Some(Message::UserMessage { .. }) => history.len() - 1,
            Some(Message::ToolResults { tool_use_results }) if !tool_use_results.is_empty() => {
                history.len() - 1
            }
            _ => history.len(),
        };
        let dir = dir.clone();
        Box::pin(async move {
            let dir = dir?;
            let current = tokio::task::spawn_blocking(move || prelude::scan(&dir))
                .await
                .ok()?
                .ok()?;
            let notice = if lost_notice {
                // A successor can retain the latest notice while omitting earlier
                // updates. Reload the live prelude even if files have not changed.
                "\n\n[myco: Prelude changes]\n\
                 This context may omit earlier prelude updates. Use prelude action=list \
                 to reload the full current prelude before relying on the old snapshot."
                    .to_string()
            } else {
                prelude::change_notice(&before, &current)?
            };
            // Commit only when the cancellable future returns the notice;
            // a detached filesystem read cannot consume a pending update.
            *observed.lock().unwrap() = PreludeObservation {
                entries: current,
                thread_id,
                notice: Some((index, notice.clone())),
            };
            Some(notice)
        })
    })
}

pub struct SessionRuntime {
    harness: Arc<Harness>,
    owner_id: Uuid,
    session_id: String,
    session: ActiveSession,
    max_image_base64_bytes: AtomicU64,
}

impl SessionRuntime {
    pub fn new(harness: Arc<Harness>, session: ActiveSession) -> Arc<Self> {
        Arc::new(Self {
            harness,
            owner_id: Uuid::new_v4(),
            session_id: session.id(),
            session,
            max_image_base64_bytes: AtomicU64::new(u64::MAX),
        })
    }

    pub fn session(&self) -> &ActiveSession {
        &self.session
    }

    pub fn set_max_image_base64_bytes(&self, limit: u64) {
        self.max_image_base64_bytes.store(limit, Ordering::Relaxed);
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Bind or recover the latest saved context while holding the session writer.
    /// Superseded checkpoints cannot discard or overwrite newer observations.
    pub fn bind_agent(
        self: &Arc<Self>,
        agent: &mut Agent,
    ) -> Result<(), crate::agent::AgentInteractionError> {
        // Preserve unsaved observations only while this agent still owns the
        // checkpoint. A superseded writer must not overwrite newer history.
        if agent.checkpoint_failed() {
            agent.checkpoint()?;
        }
        let session = self.session.snapshot();
        assert_eq!(
            self.session_id, session.id,
            "a runtime belongs to one session"
        );
        let thread = session.active_thread();
        if !agent.state().is_idle() {
            return Err(crate::agent::StateError::Busy.into());
        }
        let mut history =
            crate::agent::recover_checkpoint(thread.messages.clone(), thread.pending_operation)?;
        crate::core::image_store::ImageStore::for_profile()
            .and_then(|store| store.externalize_messages(&mut history))
            .map_err(crate::agent::AgentInteractionError::Checkpoint)?;
        if thread.pending_operation.is_some() {
            let mut recovered = crate::agent::AgentState::default();
            recovered.replace_context(history.clone(), thread.last_usage)?;
            self.session
                .persist_agent_state(&thread.id, &recovered, true, None)
                .map_err(crate::agent::AgentInteractionError::Checkpoint)?;
        }
        agent.replace_context(history, thread.last_usage)?;
        let mut context = agent.context().clone();
        context.session_id = Some(session.id.clone());
        context.thread_id = Some(thread.id.clone());
        agent.set_context(context);
        agent.set_tools(self.clone());
        agent.set_checkpoint(None);
        Ok(())
    }

    pub(crate) fn install_compacted_context(
        self: &Arc<Self>,
        agent: &mut Agent,
        continuing: bool,
    ) -> Result<(), crate::agent::AgentInteractionError> {
        let session = self.session.snapshot();
        let thread = session.active_thread();
        if continuing {
            agent.replace_at_boundary(thread.messages.clone(), thread.last_usage)?;
        } else {
            agent.replace_context(thread.messages.clone(), thread.last_usage)?;
        }
        let mut context = agent.context().clone();
        context.session_id = Some(session.id.clone());
        context.thread_id = Some(thread.id.clone());
        agent.set_context(context);
        agent.set_checkpoint(None);
        Ok(())
    }

    pub fn running_tool_summaries(&self) -> Vec<String> {
        self.harness.running_tool_summaries(self.owner_id)
    }

    pub async fn observe(
        &self,
        history: &[crate::generative_model::Message],
        model: ModelInfo,
        resume: bool,
    ) -> Option<crate::generative_model::Content> {
        let resources = self.harness.resources(self.owner_id).await;
        RuntimeRecord::observe(history, self.owner_id, model, resources, resume)
    }
}

impl ToolExecutor for SessionRuntime {
    fn tool_specs(&self) -> Vec<ToolSpec> {
        self.harness.tool_specs()
    }

    fn dispatch(self: Arc<Self>, tool: ToolUse, cancel: CancelToken) -> Async<ToolResult> {
        Box::pin(async move {
            let mut result = self
                .harness
                .clone()
                .dispatch_tool_use(tool, self.owner_id, cancel)
                .await;
            let limit = self.max_image_base64_bytes.load(Ordering::Relaxed);
            if result.content.iter().any(|part| matches!(part,
                crate::generative_model::Content::Image { source }
                if source.split_once(',').map_or(source.len(), |(_, data)| data.len()) as u64 > limit)) {
                return ToolResult::err(format!("image exceeds the current model's {limit}-byte base64 limit; resize it before viewing"));
            }
            if let Err(error) = crate::core::image_store::ImageStore::for_profile()
                .and_then(|store| store.externalize(&mut result.content))
            {
                return ToolResult::err(format!("could not store tool image: {error}"));
            }
            result
        })
    }
}

impl Drop for SessionRuntime {
    fn drop(&mut self) {
        self.harness.notify_agent_finished(self.owner_id);
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::agent::{Agent, NullEventSink};
    use crate::core::CancelToken;
    use crate::generative_model::{Content, GenerateOutput, ToolUse, TurnEndReason};
    use crate::session::{ActiveSession, Session, compact_thread};
    use crate::test_support::{ScriptedModel, temp_home};

    struct RecordingModel {
        inner: Arc<ScriptedModel>,
        requests: std::sync::Mutex<Vec<Vec<crate::generative_model::Message>>>,
    }

    impl crate::generative_model::GenerativeModel for RecordingModel {
        fn generate(
            &self,
            input: &[crate::generative_model::Message],
        ) -> crate::core::AsyncStream<crate::generative_model::GenerationEvent> {
            self.requests.lock().unwrap().push(input.to_vec());
            self.inner.generate(input)
        }
    }

    #[test]
    fn prelude_edits_reach_next_model_request_and_saved_history_once() {
        let _home = temp_home("live-prelude");
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            let harness = Harness::local_with_services(vec![Arc::new(
                crate::tool_services::PreludeTool::new(4096),
            )]);
            let session = ActiveSession::new(Session::new("test"));
            let live = SessionRuntime::new(harness, session.clone());
            let model = Arc::new(RecordingModel {
                inner: ScriptedModel::new(vec![
                    GenerateOutput {
                        content: vec![],
                        tool_uses: vec![ToolUse {
                            name: "prelude".into(),
                            input: json!({"action": "add", "text": "Use the new build command."}),
                        }],
                        turn_end_reason: TurnEndReason::ToolUse,
                        usage: None,
                    },
                    done(),
                    done(),
                ]),
                requests: std::sync::Mutex::default(),
            });
            let mut agent = Agent::new(model.clone(), live.clone(), Arc::new(NullEventSink));
            agent.set_before_generation_notice(Some(prelude_change_notices(vec![])));
            run(&mut agent, &live).await;
            run(&mut agent, &live).await;

            let requests = model.requests.lock().unwrap();
            assert_eq!(requests.len(), 3);
            assert!(
                !serde_json::to_string(&requests[0])
                    .unwrap()
                    .contains("Prelude changes")
            );
            let next = serde_json::to_string(&requests[1]).unwrap();
            assert!(next.contains("Prelude changes"), "{next}");
            let entry = &crate::prelude::entries(&crate::prelude::dir().unwrap())[0];
            assert!(next.contains(&format!("added: {}", entry.name)), "{next}");
            assert_eq!(
                serde_json::to_string(&requests[2])
                    .unwrap()
                    .matches("Prelude changes")
                    .count(),
                1
            );
            let saved = Session::load(&session.snapshot().json_path()).unwrap();
            assert_eq!(
                serde_json::to_value(saved.active_thread().messages.clone()).unwrap(),
                serde_json::to_value(agent.history()).unwrap()
            );
            assert_eq!(saved.active_thread().user_turn_timestamps.len(), 2);
        });
    }

    #[test]
    fn context_reset_requests_live_prelude_even_when_the_latest_notice_is_retained() {
        let _home = temp_home("prelude-compaction");
        let dir = prelude::dir().unwrap();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("build.md"), "old command").unwrap();
        let (prompt, initial) = crate::prompts::agent_prompt_epilogue();
        let notices = prelude_change_notices(initial);
        std::fs::write(dir.join("build.md"), "new command").unwrap();
        assert!(prompt.contains("old command"));
        assert!(!prompt.contains("new command"));

        tokio::runtime::Runtime::new().unwrap().block_on(async {
            let session = ActiveSession::new(Session::new("test"));
            let live = SessionRuntime::new(Harness::local_with_services(vec![]), session.clone());
            let model = Arc::new(RecordingModel {
                inner: ScriptedModel::new(vec![done(), done(), done(), done()]),
                requests: Mutex::default(),
            });
            let mut agent = Agent::new(model.clone(), live.clone(), Arc::new(NullEventSink));
            agent.set_before_generation_notice(Some(notices));
            run(&mut agent, &live).await;
            std::fs::write(dir.join("second.md"), "another fact").unwrap();
            run(&mut agent, &live).await;
            run(&mut agent, &live).await;
            let original = serde_json::to_value(session.snapshot().active_thread()).unwrap();
            let (next, _) = compact_thread(&session.snapshot(), "Continue the task.").unwrap();
            let retained = serde_json::to_string(&next.messages).unwrap();
            assert!(retained.contains("added: second.md"), "{retained}");
            assert!(!retained.contains("modified: build.md"), "{retained}");
            session.writer().await.commit_thread(next).unwrap();
            run(&mut agent, &live).await;
            assert_eq!(
                serde_json::to_value(&session.snapshot().threads()[0]).unwrap(),
                original
            );

            let requests = model.requests.lock().unwrap();
            let first = serde_json::to_string(&requests[0]).unwrap();
            assert!(first.contains("modified: build.md"), "{first}");
            let compacted = serde_json::to_string(&requests[3]).unwrap();
            assert!(
                compacted.contains("reload the full current prelude"),
                "{compacted}"
            );
            assert_eq!(compacted.matches("Prelude changes").count(), 2);
        });
    }

    #[test]
    fn rejected_input_rewinds_the_user_task_and_redelivers_its_prelude_notice() {
        let _home = temp_home("prelude-rewind");
        let dir = prelude::dir().unwrap();
        std::fs::create_dir_all(&dir).unwrap();
        let notices = prelude_change_notices(vec![]);
        std::fs::write(dir.join("new.md"), "new fact").unwrap();
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            let session = ActiveSession::new(Session::new("test"));
            let live = SessionRuntime::new(Harness::local_with_services(vec![]), session.clone());
            let model = ScriptedModel::new(vec![]).then_fail(
                crate::generative_model::GenerateError::RequestTooLargeError("too large".into()),
            );
            let mut agent = Agent::new(model, live.clone(), Arc::new(NullEventSink));
            agent.set_before_generation_notice(Some(notices));
            let outcome = crate::chat::run_session_turn(
                &mut agent,
                &live,
                vec![Content::Text {
                    text: "user task".into(),
                }],
                false,
                CancelToken::new(),
                chrono::Utc::now(),
                |warning| panic!("{warning}"),
            )
            .await;
            assert!(outcome.result.is_err());
            let rewound = serde_json::to_string(&outcome.rewound.unwrap()).unwrap();
            assert!(rewound.contains("user task"), "{rewound}");
            assert!(rewound.contains("Prelude changes"), "{rewound}");
            assert!(!agent.history().iter().any(Message::is_user_turn));
            assert!(
                !serde_json::to_string(agent.history())
                    .unwrap()
                    .contains("Prelude changes")
            );
            let model = Arc::new(RecordingModel {
                inner: ScriptedModel::new(vec![done()]),
                requests: Mutex::default(),
            });
            agent.set_model(model.clone());
            run(&mut agent, &live).await;
            let requests = model.requests.lock().unwrap();
            let text = serde_json::to_string(&requests[0]).unwrap();
            assert_eq!(text.matches("Prelude changes").count(), 1);
            assert!(text.contains("reload the full current prelude"), "{text}");
        });
    }

    #[test]
    fn scan_failures_preserve_pending_changes_until_a_read_succeeds() {
        let _home = temp_home("prelude-scan-retry");
        let dir = prelude::dir().unwrap();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("entry.md"), "before").unwrap();
        let notices = prelude_change_notices(prelude::entries(&dir));
        std::fs::write(dir.join("entry.md"), [0xff]).unwrap();
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            let input = vec![crate::test_support::user("task")];
            assert!(
                notices(&crate::agent::TraceContext::default(), &input)
                    .await
                    .is_none()
            );
            std::fs::write(dir.join("entry.md"), "after").unwrap();
            assert!(
                notices(&crate::agent::TraceContext::default(), &input)
                    .await
                    .unwrap()
                    .contains("modified: entry.md")
            );
        });
    }

    fn bash(input: serde_json::Value) -> GenerateOutput {
        GenerateOutput {
            content: vec![],
            tool_uses: vec![ToolUse {
                name: "bash".into(),
                input,
            }],
            turn_end_reason: TurnEndReason::ToolUse,
            usage: None,
        }
    }

    fn done() -> GenerateOutput {
        GenerateOutput {
            content: vec![],
            tool_uses: vec![],
            turn_end_reason: TurnEndReason::EndTurn,
            usage: None,
        }
    }

    async fn run(agent: &mut Agent, runtime: &Arc<SessionRuntime>) {
        crate::chat::run_session_turn(
            agent,
            runtime,
            vec![Content::Text {
                text: "task".into(),
            }],
            false,
            CancelToken::new(),
            chrono::Utc::now(),
            |warning| panic!("{warning}"),
        )
        .await
        .result
        .unwrap();
    }

    #[test]
    fn restart_records_unknown_outcomes_without_reexecuting_a_saved_tool_batch() {
        let home = temp_home("pending-recovery");
        let effect_path = home.path().join("external-effect");
        let mut state = crate::agent::AgentState::default();
        state
            .append_input(crate::test_support::user("task"))
            .unwrap();
        let crate::agent::Effect::Generate { operation } = state.start().unwrap() else {
            panic!()
        };
        state
            .generated(
                operation,
                bash(json!({"command":format!("printf replayed >> '{}'", effect_path.display())})),
            )
            .unwrap();
        let original = ActiveSession::new(Session::new("test"));
        let thread = original.snapshot().active_thread().id.clone();
        original
            .persist_agent_state(&thread, &state, true, None)
            .unwrap();
        // The crash window includes tools that have executed but whose results
        // never reached the store. Restart cannot distinguish this from no dispatch.
        std::fs::write(&effect_path, "executed once").unwrap();
        let loaded = Session::load(&original.snapshot().json_path()).unwrap();
        assert!(loaded.active_thread().pending_operation.is_some());
        let active = ActiveSession::new(loaded);
        let runtime = SessionRuntime::new(Harness::local_with_services(vec![]), active.clone());
        let mut agent = Agent::new(
            ScriptedModel::new(vec![done()]),
            runtime.clone(),
            Arc::new(NullEventSink),
        );
        runtime.bind_agent(&mut agent).unwrap();
        let recovered = Session::load(&active.snapshot().json_path()).unwrap();
        assert!(recovered.active_thread().pending_operation.is_none());
        assert!(
            matches!(&agent.history()[2], crate::generative_model::Message::ToolResults { tool_use_results }
            if tool_use_results[0].is_error && serde_json::to_string(tool_use_results).unwrap().contains("effects are unknown"))
        );
        assert!(!agent.history().last().unwrap().is_user_turn());
        tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(agent.run(CancelToken::new()))
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(effect_path).unwrap(),
            "executed once"
        );
    }

    #[test]
    fn a_failed_checkpoint_does_not_change_the_active_session_or_claim_recovery() {
        let home = temp_home("pending-save-failure");
        let mut state = crate::agent::AgentState::default();
        state
            .append_input(crate::test_support::user("task"))
            .unwrap();
        state.start().unwrap();
        let session = ActiveSession::new(Session::new("test"));
        let thread = session.snapshot().active_thread().id.clone();
        let original = serde_json::to_value(session.snapshot()).unwrap();
        std::fs::write(home.path().join("session"), "blocks the store").unwrap();
        assert!(
            session
                .persist_agent_state(&thread, &state, true, None)
                .is_err()
        );
        assert_eq!(serde_json::to_value(session.snapshot()).unwrap(), original);
        std::fs::remove_file(home.path().join("session")).unwrap();
        session
            .persist_agent_state(&thread, &state, true, None)
            .unwrap();
        let pending = serde_json::to_value(session.snapshot()).unwrap();
        std::fs::rename(home.path().join("session"), home.path().join("saved-store")).unwrap();
        std::fs::write(home.path().join("session"), "blocks recovery").unwrap();
        let runtime = SessionRuntime::new(Harness::local_with_services(vec![]), session.clone());
        let mut agent = Agent::new(
            ScriptedModel::new(vec![]),
            runtime.clone(),
            Arc::new(NullEventSink),
        );
        assert!(matches!(
            runtime.bind_agent(&mut agent),
            Err(crate::agent::AgentInteractionError::Checkpoint(_))
        ));
        assert!(agent.history().is_empty());
        assert_eq!(serde_json::to_value(session.snapshot()).unwrap(), pending);
    }

    #[test]
    fn shells_survive_archive_compaction_and_agent_replacement_but_end_with_the_session_runtime() {
        let _home = temp_home("thread-shell");
        let executor = tokio::runtime::Runtime::new().unwrap();
        executor.block_on(async {
            let harness = Harness::local_with_services(vec![]);
            let session = ActiveSession::new(Session::new("test"));
            let live = SessionRuntime::new(harness.clone(), session.clone());
            let owner = live.owner_id;
            let model = ScriptedModel::new(vec![
                bash(json!({"action":"start", "session_id":"shared", "command":"bash --noprofile --norc", "idle_ms":10, "timeout_ms":1000})),
                bash(json!({"action":"write", "session_id":"shared", "stdin":"marker=first; echo observed-$marker\n", "idle_ms":50, "timeout_ms":1000})),
                done(),
            ]);
            let mut first = Agent::new(model, live.clone(), Arc::new(NullEventSink));
            run(&mut first, &live).await;
            let original = serde_json::to_value(session.snapshot().active_thread()).unwrap();
            assert!(original.to_string().contains("observed-first"));
            let first_agent_id = first.context().agent_id;
            drop(first);
            session.set_archived(true).unwrap();
            assert_eq!(live.running_tool_summaries().len(), 1);

            let (thread, _) = compact_thread(&session.snapshot(), "A shell holds marker=first").unwrap();
            session.writer().await.commit_thread(thread).unwrap();
            let model = ScriptedModel::new(vec![
                bash(json!({"action":"write", "session_id":"shared", "stdin":"echo inherited-$marker; marker=second; echo observed-$marker\n", "idle_ms":50, "timeout_ms":1000})),
                done(),
            ]);
            let mut second = Agent::new(model, live.clone(), Arc::new(NullEventSink));
            run(&mut second, &live).await;
            assert_ne!(second.context().agent_id, first_agent_id);
            assert_eq!(second.context().thread_id.as_deref(), Some(session.snapshot().active_thread().id.as_str()));
            let saved = session.snapshot();
            assert!(saved.archived);
            let current = serde_json::to_string(saved.active_thread()).unwrap();
            assert!(current.contains("inherited-first"), "{current}");
            assert!(current.contains("observed-second"), "{current}");
            assert_eq!(serde_json::to_value(&saved.threads()[0]).unwrap(), original);
            drop(second);
            let unrelated = ActiveSession::new(Session::new("test"));
            let model = ScriptedModel::new(vec![
                bash(json!({"action":"write", "session_id":"shared", "stdin":"echo stolen\n", "idle_ms":10, "timeout_ms":1000})),
                done(),
            ]);
            let mut outsider = Agent::new(model, live.clone(), Arc::new(NullEventSink));
            let other_live = SessionRuntime::new(harness.clone(), unrelated);
            run(&mut outsider, &other_live).await;
            assert_ne!(other_live.owner_id, owner);
            let output = serde_json::to_string(outsider.history()).unwrap();
            assert!(output.contains("owned by another myco session"), "{output}");
            drop(outsider);
            assert_eq!(live.running_tool_summaries().len(), 1);
            drop(live);
            tokio::time::timeout(std::time::Duration::from_secs(2), async {
                while !harness.running_tool_summaries(owner).is_empty() {
                    tokio::task::yield_now().await;
                }
            }).await.expect("session runtime releases its tools");
        });
    }
}
