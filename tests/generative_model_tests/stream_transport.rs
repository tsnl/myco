//! Real body-read failures exercise the boundary between driver and agent retry policy.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::StreamExt;
use myco::generative_model::{
    BackendConfig, Content, GenerationEvent, GenerativeModel, GenerativeModelConfig, Message,
    ModelSpec, OpenAIBackendConfig, Protocol, RetryPolicy, ThinkingMode,
};
use myco::{Agent, AgentEvent, CancelToken, EventSink};
use serde_json::json;

use crate::test_utils::StubHttpServer;

fn model(server: &StubHttpServer) -> Arc<dyn GenerativeModel> {
    myco::generative_model::new(GenerativeModelConfig {
        model: ModelSpec {
            key: "test".into(),
            api_id: "test".into(),
            protocol: Protocol::OpenAIResponses,
            thinking: ThinkingMode::None,
            context_window_tokens: 4096,
            max_image_base64_bytes: 1024,
            max_truncated_resumes: 0,
            auto_compact_at_tokens: None,
        },
        tools: vec![],
        system_prompt: String::new(),
        backend_config: BackendConfig::OpenAIResponses(OpenAIBackendConfig {
            base_url: server.base_url(),
            ..Default::default()
        }),
    })
    .unwrap()
}

#[derive(Default)]
struct Events(Mutex<Vec<AgentEvent>>);

impl EventSink for Events {
    fn emit(&self, event: AgentEvent) {
        self.0.lock().unwrap().push(event);
    }
}

fn agent(server: &StubHttpServer, attempts: u32) -> (Agent, Arc<Events>) {
    let events = Arc::new(Events::default());
    let mut agent = Agent::new(
        model(server),
        crate::test_utils::tool_runtime(myco::Harness::local_with_services(vec![])),
        events.clone(),
    );
    agent.set_retry_policy(RetryPolicy {
        max_attempts: attempts,
        initial_backoff: Duration::ZERO,
        ..Default::default()
    });
    (agent, events)
}

fn prompt() -> Vec<Content> {
    vec![Content::Text {
        text: "Hello".into(),
    }]
}

// Closing before Content-Length bytes arrive fails inside reqwest's body stream,
// after successful HTTP headers, just as a peer reset does under HTTP/2.
fn broken_stream(body: &str) -> Vec<u8> {
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len() + 100,
    ).into_bytes()
}

fn answer() -> Vec<u8> {
    StubHttpServer::sse_response(vec![
        json!({"type":"response.output_text.delta", "output_index":0, "delta":"OK"}),
        json!({"type":"response.completed", "response":{"status":"completed"}}),
    ])
}

#[tokio::test]
async fn early_body_failure_is_retryable_without_emitting_response_parts() {
    let server = StubHttpServer::sequence(vec![broken_stream("")]).await;
    let history = [Message::UserMessage { content: prompt() }];
    let events: Vec<_> = model(&server).generate(&history).collect().await;
    assert!(
        matches!(events.as_slice(), [GenerationEvent::Failure(failure)]
        if failure.retryable && failure.cause.to_string().contains("stream body")),
        "{events:?}"
    );
}

#[tokio::test]
async fn early_disconnects_retry_even_after_non_output_sse_events() {
    for prelude in [
        "",
        ": keepalive\n\n",
        "data: {\"type\":\"response.created\"}\n\n",
        "data: {\"type\":\"response.in_progress\"}\n\n",
    ] {
        let server = StubHttpServer::sequence(vec![broken_stream(prelude), answer()]).await;
        let (mut agent, events) = agent(&server, 3);
        let output = myco::chat::interact(&mut agent, prompt(), CancelToken::new())
            .await
            .unwrap();
        assert!(matches!(output.as_slice(), [Content::Text { text }] if text == "OK"));
        assert_eq!(server.connections(), 2);
        assert_eq!(agent.history().len(), 2);
        let events = events.0.lock().unwrap();
        assert!(events.iter().any(|event| matches!(
            event,
            AgentEvent::Failure {
                retry_in: Some(_),
                ..
            }
        )));
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e, AgentEvent::TextDelta { .. }))
                .count(),
            1
        );
    }
}

#[tokio::test]
async fn body_failures_retry_text_or_tool_drafts_without_executing_them() {
    for partial in [
        json!({"type":"response.output_text.delta", "output_index":0, "delta":"Partial answer"}),
        json!({"type":"response.output_item.added", "output_index":0, "item":{"type":"function_call", "name":"bash", "call_id":"call_1", "arguments":""}}),
    ] {
        let server = StubHttpServer::sequence(vec![
            broken_stream(&format!("data: {partial}\n\n")),
            answer(),
        ])
        .await;
        let (mut agent, events) = agent(&server, 3);
        let output = myco::chat::interact(&mut agent, prompt(), CancelToken::new())
            .await
            .unwrap();
        assert!(matches!(output.as_slice(), [Content::Text { text }] if text == "OK"));
        assert_eq!(server.connections(), 2);
        assert_eq!(
            agent.history().len(),
            2,
            "only the input and successful response enter model history"
        );
        assert!(
            !serde_json::to_string(agent.history())
                .unwrap()
                .contains("Partial answer")
        );
        let events = events.0.lock().unwrap();
        assert!(
            events.iter().any(|event| matches!(event, AgentEvent::Failure { failure, retry_in: Some(_), .. } if failure.retryable))
        );
        assert!(
            events.iter().all(|e| !matches!(
                e,
                AgentEvent::ToolStarted { .. } | AgentEvent::ToolFinished { .. }
            )),
            "no tools may execute"
        );
    }
}

#[tokio::test]
async fn early_body_failure_respects_retry_budget_and_disable_setting() {
    for attempts in [1, 3] {
        let server = StubHttpServer::sequence(vec![broken_stream(""); 4]).await;
        let (mut agent, events) = agent(&server, attempts);
        myco::chat::interact(&mut agent, prompt(), CancelToken::new())
            .await
            .unwrap_err();
        assert_eq!(server.connections(), attempts as usize);
        assert!(
            events.0.lock().unwrap().iter().any(|event| matches!(event, AgentEvent::Failure { attempt, retry_in: None, .. } if *attempt == attempts))
        );
    }
}

#[tokio::test]
async fn malformed_sse_is_terminal_even_before_output() {
    let server =
        StubHttpServer::sequence(vec![broken_stream("data: invalid json\n\n"), answer()]).await;
    let (mut agent, events) = agent(&server, 3);
    myco::chat::interact(&mut agent, prompt(), CancelToken::new())
        .await
        .unwrap_err();
    assert_eq!(server.connections(), 1);
    assert!(
        events.0.lock().unwrap().iter().any(|event| matches!(event, AgentEvent::Failure { failure, retry_in: None, .. } if !failure.retryable))
    );
}
