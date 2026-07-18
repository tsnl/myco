use super::*;
use myco::generative_model::{GenerativeModelConfig, Model};

#[tokio::test]
#[ignore = "live provider API; run with: cargo test -- --ignored"]
async fn test_openai_responses_model_messaging() {
    crate::test_utils::load_dotenv();

    let model = myco::generative_model::new(GenerativeModelConfig {
        model: Model::Grok45Build,
        tools: Vec::new(),
        system_prompt: "You are a helpful assistant.".into(),
        backend_config: None,
    })
    .expect("create openai responses model");

    test_generative_model_messaging(model).await;
}

#[tokio::test]
#[ignore = "live provider API; needs OPENROUTER_API_KEY; run with: cargo test -- --ignored"]
async fn test_openrouter_model_messaging() {
    crate::test_utils::load_dotenv();

    let model = myco::generative_model::new(GenerativeModelConfig {
        model: Model::KimiK3,
        tools: Vec::new(),
        system_prompt: "You are a helpful assistant.".into(),
        backend_config: None,
    })
    .expect("create openrouter model");

    test_generative_model_messaging(model).await;
}
