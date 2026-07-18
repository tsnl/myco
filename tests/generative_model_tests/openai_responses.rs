use super::*;
use myco::generative_model::GenerativeModelConfig;

#[tokio::test]
#[ignore = "live provider API; needs XAI_API_KEY; run with: cargo test -- --ignored"]
async fn test_openai_responses_model_messaging() {
    crate::test_utils::load_dotenv();

    let (spec, backend) = crate::test_utils::live_xai_grok();
    let model = myco::generative_model::new(GenerativeModelConfig {
        model: spec,
        tools: Vec::new(),
        system_prompt: "You are a helpful assistant.".into(),
        backend_config: backend,
    })
    .expect("create openai responses model");

    test_generative_model_messaging(model).await;
}

#[tokio::test]
#[ignore = "live provider API; needs OPENROUTER_API_KEY; run with: cargo test -- --ignored"]
async fn test_openrouter_model_messaging() {
    crate::test_utils::load_dotenv();

    let (spec, backend) = crate::test_utils::live_openrouter_kimi();
    let model = myco::generative_model::new(GenerativeModelConfig {
        model: spec,
        tools: Vec::new(),
        system_prompt: "You are a helpful assistant.".into(),
        backend_config: backend,
    })
    .expect("create openrouter model");

    test_generative_model_messaging(model).await;
}
