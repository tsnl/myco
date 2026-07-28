use super::*;
use myco::generative_model::{GenerativeModelConfig, Protocol};

#[tokio::test]
#[ignore = "live provider API; needs OPENROUTER_API_KEY; run with: cargo test -- --ignored"]
async fn test_openai_completions_model_messaging() {
    crate::test_utils::load_dotenv();

    let (spec, backend) = crate::test_utils::live_openrouter_kimi(Protocol::OpenAICompletions);
    let model = myco::generative_model::new(GenerativeModelConfig {
        model: spec,
        tools: Vec::new(),
        system_prompt: "You are a helpful assistant.".into(),
        backend_config: backend,
    })
    .expect("create openai chat completions model");

    test_generative_model_messaging(model).await;
}
