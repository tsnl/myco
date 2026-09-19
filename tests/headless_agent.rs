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
        crate::test_utils::tool_runtime(Harness::local_with_services(vec![])),
        Arc::new(NullEventSink),
    );
    agent
        .replace_context(
            vec![Message::UserMessage {
                content: vec![Content::Text {
                    text: "eval task".into(),
                }],
            }],
            None,
        )
        .unwrap();
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

#[tokio::test]
async fn headless_outcome_reports_truncation_without_reusing_old_run_usage() {
    let model = test_utils::ScriptedModel::new(vec![GenerateOutput {
        content: vec![Content::Text {
            text: "partial".into(),
        }],
        tool_uses: vec![],
        turn_end_reason: TurnEndReason::MaxTokens,
        usage: None,
    }]);
    let mut agent = Agent::new(
        model,
        test_utils::tool_runtime(Harness::local_with_services(vec![])),
        Arc::new(NullEventSink),
    );
    let previous_usage = myco::generative_model::TokenUsage {
        input_tokens: 400,
        output_tokens: 100,
        cached_input_tokens: 0,
    };
    agent
        .replace_context(
            vec![Message::UserMessage {
                content: vec![Content::Text {
                    text: "task".into(),
                }],
            }],
            Some(previous_usage),
        )
        .unwrap();
    agent.set_max_truncated_resumes(0);
    let outcome = agent.run_with_outcome(CancelToken::new()).await.unwrap();
    assert_eq!(outcome.reason, TurnEndReason::MaxTokens);
    assert!(outcome.usage.is_none());
    assert_eq!(agent.last_usage(), Some(previous_usage));
    assert!(matches!(&outcome.answer[0], Content::Text { text } if text == "partial"));
}
