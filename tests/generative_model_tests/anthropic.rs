use super::*;
use myco::generative_model::GenerativeModelConfig;

#[tokio::test]
#[ignore = "live provider API; run with: cargo test -- --ignored"]
async fn test_anthropic_model_messaging() {
    crate::test_utils::load_dotenv();

    let (spec, backend) = crate::test_utils::live_anthropic_haiku();
    let model = myco::generative_model::new(GenerativeModelConfig {
        model: spec,
        tools: Vec::new(),
        system_prompt: "You are a helpful assistant.".into(),
        backend_config: backend,
    })
    .expect("create anthropic model");

    test_generative_model_messaging(model).await;
}
