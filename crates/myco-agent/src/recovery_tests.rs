//! Persistence repair must publish validated output once without replaying effects.

use std::sync::atomic::AtomicUsize;

use super::*;
use crate::test_support::{ScriptedModel, user};
use myco_model::{GenerateOutput, ToolSpec};

#[derive(Default)]
struct Events(Mutex<Vec<AgentEvent>>);

impl EventSink for Events {
    fn emit(&self, event: AgentEvent) {
        self.0.lock().unwrap().push(event);
    }
}

impl Events {
    fn commits(&self) -> usize {
        self.0
            .lock()
            .unwrap()
            .iter()
            .filter(|event| matches!(event, AgentEvent::GenerationCommitted { .. }))
            .count()
    }
}

#[derive(Default)]
struct Tools(AtomicUsize);

impl ToolExecutor for Tools {
    fn tool_specs(&self) -> Vec<ToolSpec> {
        vec![]
    }

    fn dispatch(self: Arc<Self>, _: ToolUse, _: CancelToken, _: CancelToken) -> Async<ToolResult> {
        self.0.fetch_add(1, Ordering::Relaxed);
        Box::pin(async { ToolResult::text("completed once") })
    }
}

fn output(tools: bool) -> GenerateOutput {
    GenerateOutput {
        content: vec![Content::Text {
            text: "validated answer".into(),
        }],
        tool_uses: if tools {
            vec![ToolUse {
                name: "effect".into(),
                input: serde_json::json!({}),
            }]
        } else {
            vec![]
        },
        usage: None,
        turn_end_reason: if tools {
            TurnEndReason::ToolUse
        } else {
            TurnEndReason::EndTurn
        },
    }
}

fn fail_saves(agent: &mut Agent) -> Arc<AtomicBool> {
    let failing = Arc::new(AtomicBool::new(true));
    agent.set_checkpoint(Some(Box::new({
        let failing = failing.clone();
        move |state| {
            if state.history().len() > 1 && failing.load(Ordering::Relaxed) {
                Err("disk full".into())
            } else {
                Ok(())
            }
        }
    })));
    failing
}

#[tokio::test]
async fn repaired_terminal_checkpoint_commits_once_without_regeneration() {
    let model = ScriptedModel::new(vec![output(false)]);
    let events = Arc::new(Events::default());
    let mut agent = Agent::new(model.clone(), Arc::new(Tools::default()), events.clone());
    agent.append_input(user("task")).unwrap();
    let failing = fail_saves(&mut agent);
    agent.start_run().unwrap();
    assert!(matches!(
        agent.step_once(CancelToken::new()).await,
        Err(AgentInteractionError::Checkpoint(_))
    ));
    assert_eq!(events.commits(), 0);
    assert_eq!(model.remaining(), 0);
    assert!(agent.checkpoint().is_err());
    assert_eq!(events.commits(), 0);

    failing.store(false, Ordering::Relaxed);
    agent.checkpoint().unwrap();
    assert_eq!(events.commits(), 1);
    agent.checkpoint().unwrap();
    assert!(agent.step_once(CancelToken::new()).await.unwrap().is_some());
    assert_eq!(events.commits(), 1);
    assert_eq!(agent.history().len(), 2);
}

#[tokio::test]
async fn repaired_tool_checkpoints_commit_before_dispatch_and_never_repeat_completed_tools() {
    let model = ScriptedModel::new(vec![output(true), output(false)]);
    let events = Arc::new(Events::default());
    let tools = Arc::new(Tools::default());
    let mut agent = Agent::new(model.clone(), tools.clone(), events.clone());
    agent.append_input(user("task")).unwrap();
    let failing = fail_saves(&mut agent);
    agent.start_run().unwrap();
    assert!(matches!(
        agent.step_once(CancelToken::new()).await,
        Err(AgentInteractionError::Checkpoint(_))
    ));
    assert_eq!(tools.0.load(Ordering::Relaxed), 0);
    assert_eq!(model.remaining(), 1);
    failing.store(false, Ordering::Relaxed);
    agent.checkpoint().unwrap();
    assert_eq!(events.commits(), 1);

    // A completed tool observation can fail persistence independently of intent.
    agent.set_checkpoint(Some(Box::new({
        let failing = failing.clone();
        move |state| {
            if matches!(state.history().last(), Some(Message::ToolResults { .. }))
                && failing.load(Ordering::Relaxed)
            {
                Err("tool result save failed".into())
            } else {
                Ok(())
            }
        }
    })));
    failing.store(true, Ordering::Relaxed);
    assert!(matches!(
        agent.step_once(CancelToken::new()).await,
        Err(AgentInteractionError::Checkpoint(_))
    ));
    assert_eq!(tools.0.load(Ordering::Relaxed), 1);
    assert_eq!(model.remaining(), 1);
    failing.store(false, Ordering::Relaxed);
    assert!(agent.step_once(CancelToken::new()).await.unwrap().is_some());
    assert_eq!(tools.0.load(Ordering::Relaxed), 1);
    assert_eq!(events.commits(), 2);
    let events = events.0.lock().unwrap();
    let commit = events
        .iter()
        .position(|event| matches!(event, AgentEvent::GenerationCommitted { .. }))
        .unwrap();
    let dispatch = events
        .iter()
        .position(|event| matches!(event, AgentEvent::ToolStarted { .. }))
        .unwrap();
    assert!(commit < dispatch);
}

#[tokio::test]
async fn replacing_unsaved_history_does_not_commit_the_discarded_draft() {
    let events = Arc::new(Events::default());
    let mut agent = Agent::new(
        ScriptedModel::new(vec![output(false)]),
        Arc::new(Tools::default()),
        events.clone(),
    );
    agent.append_input(user("task")).unwrap();
    let failing = fail_saves(&mut agent);
    agent.start_run().unwrap();
    assert!(agent.step_once(CancelToken::new()).await.is_err());
    agent
        .replace_context(vec![user("different task")], None)
        .unwrap();
    failing.store(false, Ordering::Relaxed);
    agent.checkpoint().unwrap();
    assert_eq!(events.commits(), 0);
}
