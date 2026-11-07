use super::*;
use myco::generative_model::{
    BackendConfig, GenerateError, GenerativeModelConfig, GoogleGenAIBackendConfig, ModelSpec,
    Protocol, ThinkingMode, ToolSpec, ToolUse,
};

use crate::test_utils::StubHttpServer;

#[tokio::test]
#[ignore = "live provider API; needs GEMINI_API_KEY; run with: cargo test -- --ignored"]
async fn test_google_genai_model_messaging() {
    crate::test_utils::load_dotenv();

    let (spec, backend) = crate::test_utils::live_google_gemini();
    let model = myco::generative_model::new(GenerativeModelConfig {
        model: spec,
        tools: Vec::new(),
        system_prompt: "You are a helpful assistant.".into(),
        backend_config: backend,
    })
    .expect("create google genai model");

    test_generative_model_messaging(model).await;
}

//
// Wire tests: the driver against a stub server. No provider, no network — the
// URL posted to, the JSON put on the wire, and SSE reassembly through the real
// client.
//

fn stub_model(
    base_url: &str,
    thinking: ThinkingMode,
    tools: Vec<ToolSpec>,
) -> std::sync::Arc<dyn GenerativeModel> {
    myco::generative_model::new(GenerativeModelConfig {
        model: ModelSpec {
            key: "gemini".into(),
            api_id: "gemini-test".into(),
            protocol: Protocol::GoogleGenAI,
            thinking,
            context_window_tokens: 4096,
            max_image_base64_bytes: myco::config::DEFAULT_MAX_IMAGE_BASE64_BYTES,
            max_truncated_resumes: 3,
            auto_compact_at_tokens: None,
        },
        tools,
        system_prompt: "Answer in one word.".into(),
        backend_config: BackendConfig::GoogleGenAI(GoogleGenAIBackendConfig {
            base_url: base_url.into(),
            auth_token: "AIzaStubKey".into(),
            max_output_tokens: Some(32),
            ..Default::default()
        }),
    })
    .expect("create stub-backed model")
}

fn user_turn(text: &str) -> Vec<Message> {
    vec![Message::UserMessage {
        content: vec![Content::Text { text: text.into() }],
    }]
}

#[tokio::test]
async fn wire_request_shape_and_text_stream() {
    let server = StubHttpServer::sse(vec![
        serde_json::json!({
            "candidates": [{"content": {"parts": [{"text": "O"}], "role": "model"}, "index": 0}],
            "usageMetadata": {"promptTokenCount": 11, "totalTokenCount": 11}
        }),
        serde_json::json!({
            "candidates": [{"content": {"parts": [{"text": "K"}], "role": "model"},
                            "finishReason": "STOP", "index": 0}],
            "usageMetadata": {"promptTokenCount": 11, "candidatesTokenCount": 2,
                              "thoughtsTokenCount": 5, "totalTokenCount": 18}
        }),
    ])
    .await;

    let model = stub_model(&server.base_url(), ThinkingMode::Budget, Vec::new());
    let output = GenerateOutput::from_generation(model.generate(&user_turn("Say OK.")))
        .await
        .expect("stream decodes");

    assert!(matches!(output.turn_end_reason, TurnEndReason::EndTurn));
    assert!(output.tool_uses.is_empty());
    match output.content.as_slice() {
        [Content::Text { text }] => assert_eq!(text, "OK"),
        other => panic!("expected one text block, got {other:?}"),
    }
    let usage = output.usage.expect("usage reported");
    assert_eq!(usage.input_tokens, 11);
    assert_eq!(usage.output_tokens, 7);

    let request = server.captured().await;
    assert_eq!(
        request.path,
        "/v1beta/models/gemini-test:streamGenerateContent?alt=sse"
    );
    assert_eq!(
        request.body["systemInstruction"]["parts"][0]["text"],
        "Answer in one word."
    );
    assert_eq!(request.body["generationConfig"]["maxOutputTokens"], 32);
    // The default `budget` mode maps the default (high) effort onto a budget.
    assert_eq!(
        request.body["generationConfig"]["thinkingConfig"],
        serde_json::json!({"includeThoughts": true, "thinkingBudget": 16000})
    );
    assert!(request.body.get("tools").is_none(), "{request:?}");
    assert_eq!(request.body["contents"][0]["role"], "user");
    assert_eq!(request.body["contents"][0]["parts"][0]["text"], "Say OK.");
}

#[tokio::test]
async fn wire_function_call_round_trip_carries_schema_signature_and_response() {
    let server = StubHttpServer::sse(vec![serde_json::json!({
        "candidates": [{"content": {"parts": [
            {"text": "Listing.", "thought": true},
            {"functionCall": {"name": "bash", "args": {"command": "ls"}},
             "thoughtSignature": "c2lnLWFiYw=="}
        ], "role": "model"}, "finishReason": "STOP"}]
    })])
    .await;

    let tools = vec![ToolSpec {
        name: "bash".into(),
        description: "run a command".into(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {"command": {"type": "string"}},
            "required": ["command"],
            "additionalProperties": false
        }),
    }];
    let model = stub_model(&server.base_url(), ThinkingMode::None, tools.clone());
    let output = GenerateOutput::from_generation(model.generate(&user_turn("List files.")))
        .await
        .expect("stream decodes");

    assert!(matches!(output.turn_end_reason, TurnEndReason::ToolUse));
    let call = match output.tool_uses.as_slice() {
        [tool_use] => tool_use.clone(),
        other => panic!("expected one tool use, got {other:?}"),
    };
    assert_eq!(call.name, "bash");
    assert_eq!(call.input, serde_json::json!({"command": "ls"}));
    assert_eq!(call.thought_signature.as_deref(), Some("c2lnLWFiYw=="));
    assert!(matches!(
        output.content.as_slice(),
        [Content::Thinking { text, .. }] if text == "Listing."
    ));

    let request = server.captured().await;
    // `thinking = none` sends no thinkingConfig at all.
    assert!(
        request.body["generationConfig"]
            .get("thinkingConfig")
            .is_none(),
        "{request:?}"
    );
    let declaration = &request.body["tools"][0]["functionDeclarations"][0];
    assert_eq!(declaration["name"], "bash");
    assert_eq!(declaration["parametersJsonSchema"], tools[0].input_schema);
    assert!(declaration.get("parameters").is_none(), "{declaration}");

    // The next turn echoes the signature on the call and answers it by name.
    let server = StubHttpServer::sse(vec![serde_json::json!({
        "candidates": [{"content": {"parts": [{"text": "Two files."}]}, "finishReason": "STOP"}]
    })])
    .await;
    let model = stub_model(&server.base_url(), ThinkingMode::None, tools);
    let mut history = user_turn("List files.");
    history.push(Message::AssistantMessage {
        content: output.content,
        tool_uses: vec![ToolUse {
            name: call.name,
            input: call.input,
            thought_signature: call.thought_signature,
        }],
        turn_end_reason: Some(output.turn_end_reason),
    });
    history.push(Message::ToolResults {
        tool_use_results: vec![myco::generative_model::ToolResult::text("a\nb\n")],
    });
    GenerateOutput::from_generation(model.generate(&history))
        .await
        .expect("stream decodes");
    let request = server.captured().await;
    let contents = request.body["contents"].as_array().unwrap();
    assert_eq!(contents.len(), 3);
    assert_eq!(contents[1]["role"], "model");
    // Thinking stays in the session; only the call goes back.
    assert_eq!(contents[1]["parts"].as_array().unwrap().len(), 1);
    assert_eq!(contents[1]["parts"][0]["functionCall"]["name"], "bash");
    assert_eq!(contents[1]["parts"][0]["thoughtSignature"], "c2lnLWFiYw==");
    assert_eq!(contents[2]["role"], "user");
    assert_eq!(
        contents[2]["parts"][0]["functionResponse"],
        serde_json::json!({"name": "bash", "response": {"output": "a\nb\n"}})
    );
}

#[tokio::test]
async fn wire_http_error_body_reaches_the_caller() {
    let server = StubHttpServer::status(
        400,
        r#"{"error":{"code":400,"message":"Unknown name \"thinkingLevel\"","status":"INVALID_ARGUMENT"}}"#,
    )
    .await;
    let model = stub_model(&server.base_url(), ThinkingMode::Effort, Vec::new());
    let error = GenerateOutput::from_generation(model.generate(&user_turn("hi")))
        .await
        .unwrap_err();
    match error {
        GenerateError::ExecutionError(message) => {
            assert!(message.contains("HTTP 400"), "{message}");
            assert!(message.contains("thinkingLevel"), "{message}");
        }
        other => panic!("expected ExecutionError, got {other:?}"),
    }
    let request = server.captured().await;
    assert_eq!(
        request.body["generationConfig"]["thinkingConfig"],
        serde_json::json!({"includeThoughts": true, "thinkingLevel": "HIGH"})
    );
}
