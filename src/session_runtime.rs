//! Session-bound tool ownership and agent binding, independent of the frontend.

use std::sync::Arc;

use uuid::Uuid;

use crate::agent::{Agent, ToolExecutor};
use crate::core::{Async, CancelToken};
use crate::generative_model::{ToolResult, ToolSpec, ToolUse};
use crate::harness::Harness;
use crate::session::ActiveSession;

pub struct SessionRuntime {
    harness: Arc<Harness>,
    owner_id: Uuid,
    session_id: String,
    session: ActiveSession,
}

impl SessionRuntime {
    pub fn new(harness: Arc<Harness>, session: ActiveSession) -> Arc<Self> {
        Arc::new(Self {
            harness,
            owner_id: Uuid::new_v4(),
            session_id: session.id(),
            session,
        })
    }

    pub fn session(&self) -> &ActiveSession {
        &self.session
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    pub fn bind_agent(self: &Arc<Self>, agent: &mut Agent) {
        let session = self.session.snapshot();
        assert_eq!(
            self.session_id, session.id,
            "a runtime belongs to one session"
        );
        let thread = session.active_thread();
        let mut context = agent.context().clone();
        context.session_id = Some(session.id.clone());
        context.thread_id = Some(thread.id.clone());
        agent.set_context(context);
        agent.set_tools(self.clone());
        agent.replace_context(thread.messages.clone(), thread.last_usage);
        agent.set_checkpoint(None);
    }

    pub fn running_tool_summaries(&self) -> Vec<String> {
        self.harness.running_tool_summaries(self.owner_id)
    }
}

impl ToolExecutor for SessionRuntime {
    fn tool_specs(&self) -> Vec<ToolSpec> {
        self.harness.tool_specs()
    }

    fn dispatch(self: Arc<Self>, tool: ToolUse, cancel: CancelToken) -> Async<ToolResult> {
        self.harness
            .clone()
            .dispatch_tool_use(tool, self.owner_id, cancel)
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
