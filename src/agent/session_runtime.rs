//! Shared ownership of live host resources, independent of any one agent or thread.

use std::sync::{Arc, OnceLock};

use uuid::Uuid;

use crate::harness::Harness;

pub struct SessionRuntime {
    pub(super) harness: Arc<Harness>,
    pub(super) owner_id: Uuid,
    session_id: OnceLock<String>,
}

impl SessionRuntime {
    pub fn new(harness: Arc<Harness>) -> Arc<Self> {
        Self::with_owner(harness, Uuid::new_v4())
    }

    pub(super) fn for_session(self: &Arc<Self>, id: &str) -> Arc<Self> {
        if self.session_id.get_or_init(|| id.to_string()) == id {
            return self.clone();
        }
        let runtime = Self::new(self.harness.clone());
        runtime.session_id.set(id.to_string()).expect("new runtime");
        runtime
    }

    pub fn running_tool_summaries(&self) -> Vec<String> {
        self.harness.running_tool_summaries(self.owner_id)
    }

    pub(super) fn with_owner(harness: Arc<Harness>, owner_id: Uuid) -> Arc<Self> {
        Arc::new(Self {
            harness,
            owner_id,
            session_id: OnceLock::new(),
        })
    }
}

impl Drop for SessionRuntime {
    fn drop(&mut self) {
        if !self.owner_id.is_nil() {
            self.harness.notify_agent_finished(self.owner_id);
        }
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

    async fn run(agent: &mut Agent, session: &ActiveSession) {
        crate::chat::run_session_turn(
            agent,
            session,
            vec![Content::Text {
                text: "task".into(),
            }],
            false,
            CancelToken::new(),
            |warning| panic!("{warning}"),
        )
        .await
        .result
        .unwrap();
    }

    #[test]
    fn shells_survive_compaction_and_agent_replacement_but_end_with_the_session_runtime() {
        let _home = temp_home("thread-shell");
        let executor = tokio::runtime::Runtime::new().unwrap();
        executor.block_on(async {
            let harness = Harness::local_with_services(vec![]);
            let live = SessionRuntime::new(harness.clone());
            let owner = live.owner_id;
            let session = ActiveSession::new(Session::new("test"));
            let model = ScriptedModel::new(vec![
                bash(json!({"action":"start", "session_id":"shared", "command":"bash --noprofile --norc", "idle_ms":10, "timeout_ms":1000})),
                bash(json!({"action":"write", "session_id":"shared", "stdin":"marker=first; echo observed-$marker\n", "idle_ms":50, "timeout_ms":1000})),
                done(),
            ]);
            let mut first = Agent::with_runtime(model, live.clone(), Arc::new(NullEventSink));
            run(&mut first, &session).await;
            let original = serde_json::to_value(session.snapshot().active_thread()).unwrap();
            assert!(original.to_string().contains("observed-first"));
            let first_agent_id = first.context().agent_id;
            drop(first);
            assert_eq!(live.running_tool_summaries().len(), 1);

            let (thread, _) = compact_thread(&session.snapshot(), "A shell holds marker=first").unwrap();
            session.writer().await.commit_thread(thread).unwrap();
            let model = ScriptedModel::new(vec![
                bash(json!({"action":"write", "session_id":"shared", "stdin":"echo inherited-$marker; marker=second; echo observed-$marker\n", "idle_ms":50, "timeout_ms":1000})),
                done(),
            ]);
            let mut second = Agent::with_runtime(model, live.clone(), Arc::new(NullEventSink));
            run(&mut second, &session).await;
            assert_ne!(second.context().agent_id, first_agent_id);
            assert_eq!(second.context().thread_id.as_deref(), Some(session.snapshot().active_thread().id.as_str()));
            let saved = session.snapshot();
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
            let mut outsider = Agent::with_runtime(model, live.clone(), Arc::new(NullEventSink));
            run(&mut outsider, &unrelated).await;
            assert_ne!(outsider.runtime().owner_id, owner);
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
