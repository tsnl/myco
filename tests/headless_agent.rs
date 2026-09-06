use std::sync::Arc;

use myco::generative_model::{Content, GenerateOutput, Message, TurnEndReason};
use myco::{Agent, CancelToken, Harness, NullEventSink};

mod test_utils;

#[tokio::test]
async fn headless_run_uses_supplied_context_without_adding_a_chat_turn() {
    let model = test_utils::ScriptedModel::new(vec![GenerateOutput {
        content: vec![Content::Text {
            text: "answer".into(),
        }],
        tool_uses: vec![],
        turn_end_reason: TurnEndReason::EndTurn,
        usage: None,
    }]);
    let mut agent = Agent::new(
        model,
        Harness::local_with_services(vec![]),
        Arc::new(NullEventSink),
    );
    agent.replace_context(
        vec![Message::UserMessage {
            content: vec![Content::Text {
                text: "eval task".into(),
            }],
        }],
        None,
    );
    let id = agent.context().agent_id;
    let agent = tokio::spawn(async move {
        agent.run(CancelToken::new()).await.unwrap();
        agent
    })
    .await
    .unwrap();
    assert_eq!(agent.context().agent_id, id);
    assert!(matches!(
        agent.history(),
        [
            Message::UserMessage { .. },
            Message::AssistantMessage { .. }
        ]
    ));
}
