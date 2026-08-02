use super::*;
use myco::generative_model::{
    BackendConfig, GenerateError, GenerativeModelConfig, ModelSpec, OpenAIBackendConfig, Protocol,
    RetryPolicy, ThinkingMode, ToolSpec,
};

use crate::test_utils::StubHttpServer;

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

//
// Wire tests: the driver against a stub server. No provider, no network — these
// run in the normal `cargo test` suite and cover the layer between the stream
// accumulator (unit-tested) and a real gateway: the URL posted to, the JSON put
// on the wire, and SSE reassembly through the real client.
//

/// A driver pointed at `base_url`, configured the way a `[models]` entry for a
/// local server resolves (no auth, no reasoning effort).
fn stub_model(base_url: &str, tools: Vec<ToolSpec>) -> std::sync::Arc<dyn GenerativeModel> {
    myco::generative_model::new(GenerativeModelConfig {
        model: ModelSpec {
            key: "local".into(),
            api_id: "test-model".into(),
            protocol: Protocol::OpenAICompletions,
            thinking: ThinkingMode::None,
            context_window_tokens: 4096,
            max_image_base64_bytes: myco::config::DEFAULT_MAX_IMAGE_BASE64_BYTES,
            max_truncated_resumes: 3,
        },
        tools,
        system_prompt: "Answer in one word.".into(),
        backend_config: BackendConfig::OpenAICompletions(OpenAIBackendConfig {
            base_url: format!("{base_url}/v1"),
            max_output_tokens: Some(32),
            ..Default::default()
        }),
    })
    .expect("create stub-backed model")
}

/// [`stub_model`] with an explicit retry policy. The sub-millisecond backoff
/// keeps retry tests instant while still exercising the real wait path.
fn stub_model_with_retry(
    base_url: &str,
    retry: RetryPolicy,
) -> std::sync::Arc<dyn GenerativeModel> {
    myco::generative_model::new(GenerativeModelConfig {
        model: ModelSpec {
            key: "local".into(),
            api_id: "test-model".into(),
            protocol: Protocol::OpenAICompletions,
            thinking: ThinkingMode::None,
            context_window_tokens: 4096,
            max_image_base64_bytes: myco::config::DEFAULT_MAX_IMAGE_BASE64_BYTES,
            max_truncated_resumes: 3,
        },
        tools: Vec::new(),
        system_prompt: "Answer in one word.".into(),
        backend_config: BackendConfig::OpenAICompletions(OpenAIBackendConfig {
            base_url: format!("{base_url}/v1"),
            max_output_tokens: Some(32),
            retry,
            ..Default::default()
        }),
    })
    .expect("create stub-backed model")
}

fn fast_retry(max_attempts: u32) -> RetryPolicy {
    RetryPolicy {
        max_attempts,
        initial_backoff: std::time::Duration::from_millis(1),
        max_backoff: std::time::Duration::from_millis(5),
        backoff_multiplier: 2.0,
    }
}

fn ok_sse_response() -> Vec<u8> {
    StubHttpServer::sse_response(vec![
        serde_json::json!({"choices": [{"index": 0, "delta": {"content": "OK"}}]}),
        serde_json::json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]}),
    ])
}

fn user_turn(text: &str) -> Vec<Message> {
    vec![Message::UserMessage {
        content: vec![Content::Text { text: text.into() }],
    }]
}

#[tokio::test]
async fn wire_request_shape_and_text_stream() {
    let server = StubHttpServer::sse(vec![
        serde_json::json!({"choices": [{"index": 0, "delta": {"role": "assistant"}}]}),
        serde_json::json!({"choices": [{"index": 0, "delta": {"content": "OK"}}]}),
        serde_json::json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]}),
        // Usage arrives in its own choice-less chunk, after the finish_reason.
        serde_json::json!({"choices": [], "usage": {"prompt_tokens": 11, "completion_tokens": 2}}),
    ])
    .await;

    let model = stub_model(&server.base_url(), Vec::new());
    let output = GenerateOutput::from_stream(model.generate(&user_turn("Say OK.")))
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
    assert_eq!(usage.output_tokens, 2);

    let request = server.captured().await;
    assert_eq!(request.path, "/v1/chat/completions");
    assert_eq!(request.body["model"], "test-model");
    assert_eq!(request.body["stream"], true);
    assert_eq!(request.body["stream_options"]["include_usage"], true);
    assert_eq!(request.body["max_completion_tokens"], 32);
    // `thinking = none` opts out; `max_tokens` is the deprecated spelling.
    assert!(
        request.body.get("reasoning_effort").is_none(),
        "{request:?}"
    );
    assert!(request.body.get("max_tokens").is_none(), "{request:?}");
    assert!(request.body.get("tools").is_none(), "{request:?}");
    assert_eq!(request.body["messages"][0]["role"], "system");
    assert_eq!(request.body["messages"][1]["role"], "user");
    assert_eq!(request.body["messages"][1]["content"], "Say OK.");
}

#[tokio::test]
async fn wire_tool_call_streamed_across_chunks() {
    let server = StubHttpServer::sse(vec![
        // Servers open the call with id + name, then stream arguments in pieces.
        serde_json::json!({"choices": [{"index": 0, "delta": {"tool_calls": [
            {"index": 0, "id": "call_1", "type": "function",
             "function": {"name": "bash", "arguments": ""}}
        ]}}]}),
        serde_json::json!({"choices": [{"index": 0, "delta": {"tool_calls": [
            {"index": 0, "function": {"arguments": "{\"command\":"}}
        ]}}]}),
        serde_json::json!({"choices": [{"index": 0, "delta": {"tool_calls": [
            {"index": 0, "function": {"arguments": "\"ls\"}"}}
        ]}}]}),
        serde_json::json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}]}),
    ])
    .await;

    let tools = vec![ToolSpec {
        name: "bash".into(),
        description: "run a command".into(),
        input_schema: serde_json::json!({"type": "object"}),
    }];
    let model = stub_model(&server.base_url(), tools);
    let output = GenerateOutput::from_stream(model.generate(&user_turn("List files.")))
        .await
        .expect("stream decodes");

    assert!(matches!(output.turn_end_reason, TurnEndReason::ToolUse));
    match output.tool_uses.as_slice() {
        [tool_use] => {
            assert_eq!(tool_use.name, "bash");
            assert_eq!(tool_use.input, serde_json::json!({"command": "ls"}));
        }
        other => panic!("expected one tool use, got {other:?}"),
    }

    let request = server.captured().await;
    assert_eq!(request.body["tools"][0]["type"], "function");
    assert_eq!(request.body["tools"][0]["function"]["name"], "bash");
}

#[tokio::test]
async fn wire_http_error_body_reaches_the_caller() {
    let server = StubHttpServer::status(
        400,
        r#"{"error":{"message":"unsupported parameter: max_completion_tokens"}}"#,
    )
    .await;

    let model = stub_model(&server.base_url(), Vec::new());
    let error = GenerateOutput::from_stream(model.generate(&user_turn("Say OK.")))
        .await
        .expect_err("HTTP 400 is an error");

    match error {
        GenerateError::ExecutionError(message) => {
            assert!(message.contains("400"), "{message}");
            assert!(message.contains("unsupported parameter"), "{message}");
        }
        other => panic!("expected ExecutionError, got {other:?}"),
    }
}

//
// Retry. Overnight runs die on a single provider blip without these, so the
// wire behaviour — how many attempts actually leave the client, and for which
// statuses — is worth pinning against a real socket.
//

/// A 503 then a 529 (Anthropic's overloaded) then success: the caller sees only
/// the successful turn, and three requests reached the server.
#[tokio::test]
async fn wire_retries_transient_statuses_then_succeeds() {
    let server = StubHttpServer::sequence(vec![
        StubHttpServer::status_response(503, r#"{"error":"service unavailable"}"#),
        StubHttpServer::status_response(529, r#"{"error":{"type":"overloaded_error"}}"#),
        ok_sse_response(),
    ])
    .await;

    let model = stub_model_with_retry(&server.base_url(), fast_retry(3));
    let output = GenerateOutput::from_stream(model.generate(&user_turn("Say OK.")))
        .await
        .expect("third attempt succeeds");

    match output.content.as_slice() {
        [Content::Text { text }] => assert_eq!(text, "OK"),
        other => panic!("expected one text block, got {other:?}"),
    }
    assert_eq!(server.connections(), 3, "two retries then the success");
}

/// A deterministic status is never retried: resending a malformed request only
/// delays the error the user needs to see.
#[tokio::test]
async fn wire_does_not_retry_a_client_error() {
    let server = StubHttpServer::sequence(vec![StubHttpServer::status_response(
        400,
        r#"{"error":"bad request"}"#,
    )])
    .await;

    let model = stub_model_with_retry(&server.base_url(), fast_retry(5));
    GenerateOutput::from_stream(model.generate(&user_turn("Say OK.")))
        .await
        .expect_err("HTTP 400 is an error");

    assert_eq!(server.connections(), 1, "400 must not be retried");
}

/// Retry is bounded: a provider that is down stays down, and the turn has to
/// end with the error rather than looping.
#[tokio::test]
async fn wire_gives_up_after_max_attempts() {
    let down = || StubHttpServer::status_response(503, r#"{"error":"down"}"#);
    let server = StubHttpServer::sequence(vec![down(), down(), down(), down()]).await;

    let model = stub_model_with_retry(&server.base_url(), fast_retry(3));
    let error = GenerateOutput::from_stream(model.generate(&user_turn("Say OK.")))
        .await
        .expect_err("all attempts fail");

    match error {
        GenerateError::ExecutionError(message) => assert!(message.contains("503"), "{message}"),
        other => panic!("expected ExecutionError, got {other:?}"),
    }
    // Exactly max_attempts — the fourth canned response is never collected.
    assert_eq!(server.connections(), 3);
}

/// `max_attempts = 1` is the opt-out.
#[tokio::test]
async fn wire_retry_can_be_disabled() {
    let server = StubHttpServer::sequence(vec![
        StubHttpServer::status_response(503, r#"{"error":"down"}"#),
        ok_sse_response(),
    ])
    .await;

    let model = stub_model_with_retry(&server.base_url(), fast_retry(1));
    GenerateOutput::from_stream(model.generate(&user_turn("Say OK.")))
        .await
        .expect_err("no retry, so the 503 surfaces");

    assert_eq!(server.connections(), 1);
}
