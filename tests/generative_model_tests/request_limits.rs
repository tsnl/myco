//! All protocols apply the cap to the actual JSON body before opening a connection.

use myco::generative_model::{
    self, AnthropicBackendConfig, BackendConfig, Content, GenerateError, GenerateOutput,
    GenerativeModelConfig, Message, ModelSpec, OpenAIBackendConfig, Protocol, Recovery,
    ThinkingMode, ToolSpec,
};
use serde_json::json;

use crate::test_utils::StubHttpServer;

fn model(
    server: &StubHttpServer,
    protocol: Protocol,
    limit: usize,
) -> std::sync::Arc<dyn generative_model::GenerativeModel> {
    let openai = OpenAIBackendConfig {
        base_url: server.base_url(),
        max_request_bytes: limit,
        ..Default::default()
    };
    let backend_config = match protocol {
        Protocol::AnthropicMessages => BackendConfig::Anthropic(AnthropicBackendConfig {
            anthropic_base_url: server.base_url(),
            max_request_bytes: limit,
            ..Default::default()
        }),
        Protocol::OpenAIResponses => BackendConfig::OpenAIResponses(openai),
        Protocol::OpenAICompletions => BackendConfig::OpenAICompletions(openai),
    };
    generative_model::new(GenerativeModelConfig {
        model: ModelSpec {
            key: "test".into(),
            api_id: "test".into(),
            protocol,
            thinking: ThinkingMode::None,
            context_window_tokens: 4096,
            max_image_base64_bytes: 1024,
            max_truncated_resumes: 0,
            auto_compact_at_tokens: None,
        },
        system_prompt: "Describe the images.\nInclude the café sign.\n".repeat(10),
        tools: vec![ToolSpec {
            name: "inspect".into(),
            description: "Read an image".into(),
            input_schema: json!({"type":"object"}),
        }],
        backend_config,
    })
    .unwrap()
}

fn history() -> Vec<Message> {
    let image = Content::Image {
        source: format!("data:image/png;base64,{}", "AAAA".repeat(100)),
    };
    vec![
        Message::UserMessage {
            content: vec![image.clone()],
        },
        Message::AssistantMessage {
            content: vec![Content::Text {
                text: "Next image?".into(),
            }],
            tool_uses: vec![],
            turn_end_reason: Some(generative_model::TurnEndReason::EndTurn),
        },
        Message::UserMessage {
            content: vec![image],
        },
    ]
}

#[tokio::test]
async fn every_protocol_counts_encoded_images_history_and_json_overhead_against_its_cap() {
    for protocol in [
        Protocol::AnthropicMessages,
        Protocol::OpenAIResponses,
        Protocol::OpenAICompletions,
    ] {
        let history = history();
        let baseline = StubHttpServer::status(400, "test response").await;
        let error = GenerateOutput::from_generation(
            model(&baseline, protocol, 30_000_000).generate(&history),
        )
        .await
        .unwrap_err();
        assert!(
            matches!(error, GenerateError::ExecutionError(_)),
            "{error:?}"
        );
        let bytes = serde_json::to_vec(&baseline.captured().await.body)
            .unwrap()
            .len();
        assert!(
            bytes > 800,
            "must include both encoded images and request overhead"
        );

        let rejected = StubHttpServer::status(400, "must not receive a request").await;
        let error = GenerateOutput::from_generation(
            model(&rejected, protocol, bytes - 1).generate(&history),
        )
        .await
        .unwrap_err();
        assert_eq!(
            error.recovery(),
            Recovery::OmitLastMessage,
            "{protocol}: {error:?}"
        );
        assert!(
            error
                .to_string()
                .contains(&format!("request is {bytes} bytes")),
            "{error}"
        );
        assert_eq!(
            rejected.connections(),
            0,
            "{protocol} uploaded an oversized request"
        );

        let exact = StubHttpServer::status(400, "test response").await;
        let error =
            GenerateOutput::from_generation(model(&exact, protocol, bytes).generate(&history))
                .await
                .unwrap_err();
        assert!(
            matches!(error, GenerateError::ExecutionError(_)),
            "{protocol}: {error:?}"
        );
        assert_eq!(exact.connections(), 1, "the exact limit must be allowed");
    }
}
