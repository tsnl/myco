use std::sync::Arc;

use myco::generative_model::{self, Content, GenerativeModelConfig};
use myco::harness::Harness;
use myco::tool_services::ToolService;
use myco::*;

mod generative_model_tests;
mod test_tools;
mod test_utils;

#[tokio::test]
#[ignore = "live provider API; run with: cargo test -- --ignored"]
async fn test_agent_tool_use() {
    test_utils::load_dotenv();

    let harness = Harness::local_with_services(vec![
        Arc::new(test_tools::LetterCounterTool) as Arc<dyn ToolService>
    ]);

    let (spec, backend) = test_utils::live_anthropic_haiku();
    let model = generative_model::new(GenerativeModelConfig {
        model: spec,
        tools: harness.tool_specs(),
        system_prompt: "You are a helpful assistant. Respond exclusively in decimal integers."
            .into(),
        backend_config: backend,
    })
    .expect("create anthropic model");

    let mut agent = Agent::new(model, harness, Arc::new(myco::NullEventSink));

    let user_prompts_and_answers = [
        ("How many Rs are there in 'strawberry'?", "3"),
        ("How about the letter 'x'?", "0"),
    ];

    for (prompt, answer) in user_prompts_and_answers {
        let input = vec![Content::Text {
            text: prompt.to_string(),
        }];
        let ret_content = agent
            .interact(input, myco::CancelToken::new())
            .await
            .unwrap();
        eprintln!("Tool result: {ret_content:#?}");

        assert_eq!(ret_content.len(), 1);
        match &ret_content[0] {
            Content::Text { text } => {
                eprintln!("Assistant: {text:?}");
                assert!(
                    text.contains(answer),
                    "expected {text:?} to contain {answer:?}"
                );
            }
            _ => panic!("expected text content"),
        }
    }
}
