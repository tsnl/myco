//! Structured HTTP-200 errors preserve retry policy without accepting their drafts.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::StreamExt;
use myco::generative_model::{
    AnthropicBackendConfig, BackendConfig, Content, GenerationEvent, GenerativeModel,
    GenerativeModelConfig, Message, ModelSpec, OpenAIBackendConfig, Protocol, RetryPolicy,
    ThinkingMode,
};
use myco::{Agent, AgentEvent, CancelToken, EventSink};
use serde_json::{Value, json};

use crate::test_utils::StubHttpServer;

#[derive(Clone, Copy, Debug)]
enum Dialect {
    Anthropic,
    ResponsesFailed,
    ResponsesCompletedFailed,
    ResponsesError,
    ResponsesNestedError,
    Completions,
}

const DIALECTS: [Dialect; 6] = [
    Dialect::Anthropic,
    Dialect::ResponsesFailed,
    Dialect::ResponsesCompletedFailed,
    Dialect::ResponsesError,
    Dialect::ResponsesNestedError,
    Dialect::Completions,
];

impl Dialect {
    fn protocol(self) -> Protocol {
        match self {
            Self::Anthropic => Protocol::AnthropicMessages,
            Self::Completions => Protocol::OpenAICompletions,
            _ => Protocol::OpenAIResponses,
        }
    }

    fn transient_codes(self) -> [&'static str; 4] {
        match self {
            Self::Anthropic => [
                "overloaded_error",
                "api_error",
                "timeout_error",
                "rate_limit_error",
            ],
            _ => [
                "server_error",
                "rate_limit_exceeded",
                "slow_down",
                "server_is_overloaded",
            ],
        }
    }

    fn error(self, code: Value) -> Value {
        // A misleading message must never turn an unknown code into a retry.
        let error = json!({"type":code, "code":code, "message":"overloaded_error server_error"});
        match self {
            Self::Anthropic => json!({"type":"error", "error":error}),
            Self::ResponsesFailed => json!({"type":"response.failed", "response":{"error":error}}),
            Self::ResponsesCompletedFailed => {
                json!({"type":"response.completed", "response":{"status":"failed", "error":error}})
            }
            Self::ResponsesError => {
                json!({"type":"error", "code":code, "message":"overloaded_error server_error"})
            }
            Self::ResponsesNestedError => json!({"type":"error", "error":error}),
            Self::Completions => json!({"error":error}),
        }
    }

    fn draft(self, tool: bool) -> Value {
        match (self, tool) {
            (Self::Anthropic, true) => {
                json!({"type":"content_block_start", "index":0, "content_block":{"type":"tool_use", "name":"bash", "input":{}}})
            }
            (Self::Anthropic, false) => {
                json!({"type":"content_block_start", "index":0, "content_block":{"type":"text", "text":"DISCARDED"}})
            }
            (Self::Completions, true) => {
                json!({"choices":[{"delta":{"tool_calls":[{"index":0, "function":{"name":"bash", "arguments":"{}"}}]}}]})
            }
            (Self::Completions, false) => json!({"choices":[{"delta":{"content":"DISCARDED"}}]}),
            (_, true) => {
                json!({"type":"response.output_item.added", "output_index":0, "item":{"type":"function_call", "name":"bash", "arguments":"{}"}})
            }
            (_, false) => {
                json!({"type":"response.output_text.delta", "output_index":0, "delta":"DISCARDED"})
            }
        }
    }

    fn answer(self) -> Vec<u8> {
        StubHttpServer::sse_response(match self {
            Self::Anthropic => vec![
                json!({"type":"content_block_start", "index":0, "content_block":{"type":"text", "text":"OK"}}),
                json!({"type":"message_delta", "delta":{"stop_reason":"end_turn"}}),
                json!({"type":"message_stop"}),
            ],
            Self::Completions => {
                vec![json!({"choices":[{"delta":{"content":"OK"}, "finish_reason":"stop"}]})]
            }
            _ => vec![
                json!({"type":"response.output_text.delta", "output_index":0, "delta":"OK"}),
                json!({"type":"response.completed", "response":{"status":"completed"}}),
            ],
        })
    }
}

fn model(server: &StubHttpServer, dialect: Dialect) -> Arc<dyn GenerativeModel> {
    let openai = OpenAIBackendConfig {
        base_url: server.base_url(),
        ..Default::default()
    };
    myco::generative_model::new(GenerativeModelConfig {
        model: ModelSpec {
            key: "test".into(),
            api_id: "test".into(),
            protocol: dialect.protocol(),
            thinking: ThinkingMode::None,
            context_window_tokens: 4096,
            max_image_base64_bytes: 1024,
            max_truncated_resumes: 0,
            auto_compact_at_tokens: None,
        },
        tools: vec![],
        system_prompt: String::new(),
        backend_config: match dialect {
            Dialect::Anthropic => BackendConfig::Anthropic(AnthropicBackendConfig {
                anthropic_base_url: server.base_url(),
                ..Default::default()
            }),
            Dialect::Completions => BackendConfig::OpenAICompletions(openai),
            _ => BackendConfig::OpenAIResponses(openai),
        },
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

fn agent(server: &StubHttpServer, dialect: Dialect) -> (Agent, Arc<Events>) {
    let events = Arc::new(Events::default());
    let mut agent = Agent::new(
        model(server, dialect),
        crate::test_utils::tool_runtime(myco::Harness::local_with_services(vec![])),
        events.clone(),
    );
    agent.set_retry_policy(RetryPolicy {
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

#[tokio::test]
async fn explicit_transient_codes_survive_the_stream_driver_without_internal_retries() {
    for dialect in DIALECTS {
        for code in dialect.transient_codes() {
            let response = StubHttpServer::sse_response(vec![dialect.error(json!(code))]);
            let response = String::from_utf8(response)
                .unwrap()
                .replacen("\r\n", "\r\nRetry-After: 17\r\n", 1)
                .into_bytes();
            let server = StubHttpServer::sequence(vec![response]).await;
            let events: Vec<_> = model(&server, dialect)
                .generate(&[Message::UserMessage { content: prompt() }])
                .collect()
                .await;
            assert!(
                matches!(events.as_slice(), [GenerationEvent::Failure(failure)]
                if failure.retryable && failure.retry_after == Some(Duration::from_secs(17))
                && failure.cause.to_string().contains(code)),
                "{dialect:?} {events:?}"
            );
            assert_eq!(server.connections(), 1);
        }
    }
}

#[tokio::test]
async fn auth_request_quota_and_unknown_stream_errors_stop_after_one_request() {
    for dialect in DIALECTS {
        for code in [
            "authentication_error",
            "permission_error",
            "invalid_request_error",
            "invalid_prompt",
            "insufficient_quota",
            "organization_usage_limit_exceeded",
            "future_unknown",
            "",
        ]
        .into_iter()
        .map(Value::from)
        .chain([Value::Null, json!(502)])
        {
            let server = StubHttpServer::sequence(vec![
                StubHttpServer::sse_response(vec![dialect.error(json!(code))]),
                dialect.answer(),
            ])
            .await;
            let (mut agent, events) = agent(&server, dialect);
            myco::chat::interact(&mut agent, prompt(), CancelToken::new())
                .await
                .unwrap_err();
            assert_eq!(server.connections(), 1, "{dialect:?} {code}");
            assert_eq!(agent.history().len(), 1);
            assert!(events.0.lock().unwrap().iter().any(|event| matches!(event,
                AgentEvent::Failure { failure, retry_in: None, .. } if !failure.retryable)));
        }
    }
}

#[tokio::test]
async fn malformed_stream_recovery_does_not_ignore_retry_after() {
    for dialect in DIALECTS {
        for malformed_json in [false, true] {
            let response = if malformed_json {
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\ndata: invalid JSON\n\n"
                    .to_vec()
            } else {
                StubHttpServer::sse_response(vec![dialect.draft(false)])
            };
            let response = String::from_utf8(response)
                .unwrap()
                .replacen("\r\n", "\r\nRetry-After: 60\r\n", 1)
                .into_bytes();
            let server = StubHttpServer::sequence(vec![response, dialect.answer()]).await;
            let (mut agent, _) = agent(&server, dialect);
            let error = myco::chat::interact(&mut agent, prompt(), CancelToken::new())
                .await
                .unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("provider Retry-After requires 60.0s"),
                "{error}"
            );
            assert_eq!(server.connections(), 1);
        }
    }
}

#[tokio::test]
async fn stream_recovery_discards_provisional_text_and_tool_calls() {
    for dialect in DIALECTS {
        for tool in [false, true] {
            let server = StubHttpServer::sequence(vec![
                StubHttpServer::sse_response(vec![
                    dialect.draft(tool),
                    dialect.error(json!(dialect.transient_codes()[0])),
                ]),
                dialect.answer(),
            ])
            .await;
            let (mut agent, events) = agent(&server, dialect);
            let output = myco::chat::interact(&mut agent, prompt(), CancelToken::new())
                .await
                .unwrap();
            assert!(matches!(output.as_slice(), [Content::Text { text }] if text == "OK"));
            assert_eq!(server.connections(), 2);
            assert_eq!(agent.history().len(), 2);
            assert!(
                !serde_json::to_string(agent.history())
                    .unwrap()
                    .contains("DISCARDED")
            );
            let events = events.0.lock().unwrap();
            assert!(events.iter().any(|event| matches!(
                event,
                AgentEvent::Failure {
                    retry_in: Some(_),
                    ..
                }
            )));
            assert!(events.iter().all(|event| !matches!(
                event,
                AgentEvent::ToolStarted { .. } | AgentEvent::ToolFinished { .. }
            )));
        }
    }
}
