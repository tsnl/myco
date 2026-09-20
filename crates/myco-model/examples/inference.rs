use std::io::{self, Write};

use futures::StreamExt;
use myco_model::{
    BackendConfig, Content, ContentDelta, GenerativeModelConfig, Message, MessageAccumulator,
    MessagePart, ModelSpec, OpenAIBackendConfig, Protocol, ThinkingMode,
};

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let model = myco_model::new(GenerativeModelConfig {
        model: ModelSpec {
            key: "example".into(),
            api_id: std::env::var("MYCO_EXAMPLE_MODEL")?,
            protocol: Protocol::OpenAICompletions,
            thinking: ThinkingMode::None,
            context_window_tokens: 32768,
            max_image_base64_bytes: 5 * 1024 * 1024,
            max_truncated_resumes: 0,
            auto_compact_at_tokens: None,
        },
        tools: vec![],
        system_prompt: "Give a brief, helpful answer.".into(),
        backend_config: BackendConfig::OpenAICompletions(OpenAIBackendConfig {
            base_url: std::env::var("MYCO_EXAMPLE_BASE_URL")?,
            auth_token: std::env::var("MYCO_EXAMPLE_API_KEY").unwrap_or_default(),
            effort: None,
            max_output_tokens: Some(512),
            ..Default::default()
        }),
    })?;
    let history = vec![Message::UserMessage {
        content: vec![Content::Text {
            text: "What makes a good library interface?".into(),
        }],
    }];

    let mut stream = model.generate(&history);
    let mut response = MessageAccumulator::default();
    while let Some(event) = stream.next().await {
        // This example performs one attempt; errors propagate to the caller.
        let part = event.into_result()?;
        response.push(&part)?;
        if let MessagePart::ContentDelta(ContentDelta::Text { delta, .. }) = part {
            print!("{delta}");
            io::stdout().flush()?;
        }
    }
    let output = response.finish()?;
    println!();
    eprintln!(
        "stop: {:?}; usage: {:?}",
        output.turn_end_reason, output.usage
    );
    Ok(())
}
