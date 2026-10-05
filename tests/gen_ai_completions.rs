use std::time::Duration;

use futures_core::stream::FusedStream;
use futures_util::StreamExt;
use myco::blob::{Blob, BlobError, BlobRef, BlobStore, MediaType};
use myco::gen_ai::{
    Config, ContentPart, DeltaKind, Error, Event, Finish, GenAiClient, InputContentPart, Message,
    MessageKind, Tool, Usage,
};
use serde_json::{Value, json};

#[allow(dead_code)]
mod common;
use common::{collect, completed, encoded_request, events, fixture, request, unfinished_body};

//
// Fixtures
//

fn client(endpoint: &str) -> GenAiClient {
    GenAiClient::new(
        Config::OpenAiCompletions {
            endpoint: endpoint.into(),
            api_key: "test-key".into(),
        },
        BlobStore::default(),
    )
    .unwrap()
}

fn chunk(delta: Value, finish: Option<&str>) -> Value {
    json!({"id":"chatcmpl_test", "object":"chat.completion.chunk", "usage":null,
        "choices":[{"index":0, "delta":delta, "finish_reason":finish}]})
}

fn stream(chunks: &[Value]) -> String {
    format!("{}data: [DONE]\n\n", events(chunks))
}

async fn run(body: &str) -> common::Trace {
    let (endpoint, _) = fixture(body, 200, "").await;
    collect(&client(&endpoint), request()).await
}

fn call_delta(index: usize, id: &str, name: &str, arguments: &str) -> Value {
    json!({"index":index, "id":id, "type":"function", "function":{
        "name":name, "arguments":arguments,
    }})
}

fn tools_stream() -> String {
    stream(&[
        chunk(
            json!({"role":"assistant", "content":"Checking 雪", "tool_calls":[
                call_delta(0, "call_read", "read", "{\"path\":"),
                call_delta(1, "call_list", "list", "{"),
            ]}),
            None,
        ),
        chunk(
            json!({"tool_calls":[
                {"index":1, "function":{"arguments":"}"}},
                {"index":0, "function":{"arguments":"\"note\"}"}},
            ]}),
            Some("tool_calls"),
        ),
        json!({"id":"chatcmpl_test", "object":"chat.completion.chunk", "choices":[],
            "usage":{"prompt_tokens":20, "completion_tokens":6, "prompt_tokens_details":{"cached_tokens":5}}}),
    ])
}

fn tool_calls(message: &Message) -> Vec<&myco::gen_ai::ToolCall> {
    let MessageKind::Assistant { content } = &message.kind else {
        panic!("expected assistant")
    };
    content
        .iter()
        .filter_map(|part| match part {
            ContentPart::ToolCall(call) => Some(call),
            _ => None,
        })
        .collect()
}

//
// Requests and history
//

#[tokio::test]
async fn request_encodes_tools_images_and_instructions_before_dispatch() {
    let (endpoint, mut capture) = fixture(
        &stream(&[chunk(json!({"content":"Hello"}), Some("stop"))]),
        200,
        "",
    )
    .await;
    let model = client(&endpoint);
    let reference = model
        .blobs()
        .insert(Blob {
            media_type: MediaType::Png,
            data: b"image".as_slice().into(),
        })
        .unwrap();
    let mut input = request();
    input.instructions = "Be concise".into();
    input.messages = vec![Message::new(MessageKind::User {
        content: vec![
            InputContentPart::Text {
                content: "Read this".into(),
            },
            InputContentPart::Image { blob: reference },
        ],
    })];
    input.tools = vec![Tool {
        name: "read".into(),
        description: "Read a file".into(),
        parameters: json!({"type":"object"}),
    }];
    input
        .driver_options
        .insert("temperature".into(), json!(0.2));
    let mut generation = model.generate(input).unwrap();
    let Event::Request { body } = generation.next().await.unwrap().unwrap() else {
        panic!("expected request")
    };
    assert!(
        tokio::time::timeout(Duration::from_millis(30), &mut capture)
            .await
            .is_err()
    );
    assert_eq!(
        body["messages"][0],
        json!({"role":"system", "content":"Be concise"})
    );
    assert_eq!(
        body["messages"][1]["content"][1]["image_url"]["url"],
        "data:image/png;base64,aW1hZ2U="
    );
    assert_eq!(
        body["tools"][0],
        json!({"type":"function", "function":{
            "name":"read", "description":"Read a file", "parameters":{"type":"object"}, "strict":false,
        }})
    );
    assert_eq!(body["max_completion_tokens"], 64);
    assert_eq!(body["stream_options"], json!({"include_usage":true}));
    assert_eq!(body["store"], false);
    assert_eq!(body["n"], 1);
    assert_eq!(body["temperature"], 0.2);
    while let Some(event) = generation.next().await {
        event.unwrap();
    }
    let captured = capture.await.unwrap();
    assert_eq!(captured.body, body);
    assert!(
        captured
            .headers
            .to_lowercase()
            .contains("authorization: bearer test-key")
    );
    assert!(!body.to_string().contains("test-key"));
}

#[tokio::test]
async fn completed_calls_replay_through_a_fresh_client_with_correlated_results() {
    let trace = run(&tools_stream()).await;
    let message = completed(&trace).message.clone();
    let mut input = request();
    input.messages.push(message.clone());
    for call in tool_calls(&message) {
        assert!(uuid::Uuid::parse_str(&call.id).is_ok());
        assert_ne!(Some(call.id.as_str()), call.provider_id.as_deref());
        input.messages.push(Message::new(MessageKind::ToolResult {
            call_id: call.id.clone(),
            content: vec![InputContentPart::Text {
                content: "failed".into(),
            }],
            is_error: true,
        }));
    }
    let body = encoded_request(&client("http://127.0.0.1:1/chat/completions"), input).await;
    assert_eq!(body["messages"][1]["tool_calls"][0]["id"], "call_read");
    assert_eq!(
        body["messages"][1]["tool_calls"][0]["function"]["arguments"],
        "{\"path\":\"note\"}"
    );
    assert_eq!(
        body["messages"][2],
        json!({"role":"tool", "tool_call_id":"call_read", "content":[
            {"type":"text", "text":"Tool error:"}, {"type":"text", "text":"failed"},
        ]})
    );
    assert_eq!(body["messages"][3]["tool_call_id"], "call_list");
}

#[tokio::test]
async fn unsupported_history_and_managed_options_fail_before_a_stream_exists() {
    let model = client("http://127.0.0.1:1/chat/completions");
    for reasoning in [
        ContentPart::Reasoning {
            text: "thought".into(),
            signature: Some("signature".into()),
        },
        ContentPart::EncryptedReasoning {
            provider_id: "rs_1".into(),
            summary: vec![],
            data: "encrypted".into(),
        },
        ContentPart::RedactedReasoning("redacted".into()),
    ] {
        let mut input = request();
        input.messages.push(Message::new(MessageKind::Assistant {
            content: vec![reasoning],
        }));
        assert!(matches!(
            model.generate(input),
            Err(Error::InvalidRequest(_))
        ));
    }
    for name in [
        "max_completion_tokens",
        "max_tokens",
        "stream_options",
        "n",
        "functions",
        "function_call",
    ] {
        let mut input = request();
        input.driver_options.insert(name.into(), json!(1));
        assert!(matches!(
            model.generate(input),
            Err(Error::InvalidRequest(_))
        ));
    }
    let trace = run(&tools_stream()).await;
    let message = completed(&trace).message.clone();
    let mut input = request();
    input.messages.push(message.clone());
    for call in tool_calls(&message) {
        input.messages.push(Message::new(MessageKind::ToolResult {
            call_id: call.id.clone(),
            content: vec![InputContentPart::Image {
                blob: BlobRef([0; 32]),
            }],
            is_error: false,
        }));
    }
    assert!(
        matches!(model.generate(input), Err(Error::InvalidRequest(message)) if message.contains("only text"))
    );
}

#[test]
fn missing_user_images_fail_synchronously() {
    let model = client("http://127.0.0.1:1/chat/completions");
    let mut input = request();
    let reference = BlobRef([0; 32]);
    input.messages = vec![Message::new(MessageKind::User {
        content: vec![InputContentPart::Image { blob: reference }],
    })];
    assert!(
        matches!(model.generate(input), Err(Error::Blob(BlobError::Missing(id))) if id == reference)
    );
}

#[tokio::test]
async fn observation_only_reasoning_is_omitted_and_tool_only_history_stays_valid() {
    let reasoning = ContentPart::Reasoning {
        text: "display only".into(),
        signature: None,
    };
    let mut input = request();
    input.messages.extend([
        Message::new(MessageKind::Assistant {
            content: vec![reasoning.clone()],
        }),
        Message::new(MessageKind::Assistant {
            content: vec![
                reasoning,
                ContentPart::ToolCall(myco::gen_ai::ToolCall {
                    id: "logical/call".into(),
                    provider_id: None,
                    name: "read".into(),
                    arguments: Ok(json!({})),
                }),
            ],
        }),
        Message::new(MessageKind::ToolResult {
            call_id: "logical/call".into(),
            content: vec![InputContentPart::Text {
                content: "result".into(),
            }],
            is_error: false,
        }),
    ]);
    let body = encoded_request(&client("http://127.0.0.1:1/chat/completions"), input).await;
    assert_eq!(body["messages"].as_array().unwrap().len(), 3);
    let call = &body["messages"][1]["tool_calls"][0];
    assert_eq!(call["function"]["name"], "read");
    assert!(body["messages"][1]["content"].is_null());
    assert_eq!(call["id"], body["messages"][2]["tool_call_id"]);
    assert!(!body.to_string().contains("display only"));
    assert!(!body.to_string().contains("logical/call"));
}

//
// Streaming and completion
//

#[tokio::test]
async fn interleaved_calls_and_usage_survive_fragmented_sse() {
    let trace = run(&tools_stream()).await;
    let completion = completed(&trace);
    assert_eq!(completion.finish, Finish::ToolCalls);
    assert_eq!(
        completion.usage,
        Usage {
            input_tokens: Some(20),
            output_tokens: Some(6),
            cache_read_tokens: Some(5),
            cache_write_tokens: None
        }
    );
    let calls = tool_calls(&completion.message);
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].arguments, Ok(json!({"path":"note"})));
    assert_eq!(calls[1].arguments, Ok(json!({})));
    assert_ne!(calls[0].id, calls[1].id);
    let deltas: Vec<_> = trace
        .events
        .iter()
        .filter_map(|event| match event {
            Event::Delta(delta) => Some((delta.index, delta.part, delta.kind, delta.text.as_str())),
            _ => None,
        })
        .collect();
    assert_eq!(
        deltas,
        [
            (0, 0, DeltaKind::Text, "Checking 雪"),
            (1, 0, DeltaKind::ToolArguments, "{\"path\":"),
            (2, 0, DeltaKind::ToolArguments, "{"),
            (2, 0, DeltaKind::ToolArguments, "}"),
            (1, 0, DeltaKind::ToolArguments, "\"note\"}"),
        ]
    );
    assert!(
        matches!(&trace.events[trace.events.len()-2], Event::Progress { raw } if raw == "[DONE]")
    );
    assert!(matches!(trace.events.last(), Some(Event::Completed { .. })));
}

#[tokio::test]
async fn refusal_and_length_remain_distinct_without_usage() {
    let refusal = run(&stream(&[
        chunk(json!({"refusal":"Cannot "}), None),
        chunk(json!({"refusal":"help"}), Some("stop")),
    ]))
    .await;
    assert_eq!(completed(&refusal).finish, Finish::Refusal);
    assert_eq!(completed(&refusal).usage, Usage::default());
    assert_eq!(
        completed(&refusal).message.kind,
        MessageKind::Assistant {
            content: vec![ContentPart::Refusal("Cannot help".into())]
        }
    );
    for (reason, expected) in [
        ("stop", Finish::Stop),
        ("length", Finish::Length),
        ("content_filter", Finish::Refusal),
    ] {
        let trace = run(&stream(&[chunk(
            json!({"content":"partial"}),
            Some(reason),
        )]))
        .await;
        assert_eq!(completed(&trace).finish, expected);
    }
}

#[tokio::test]
async fn malformed_tool_arguments_are_only_retained_for_truncated_output() {
    for (reason, valid) in [("length", true), ("tool_calls", false)] {
        let trace = run(&stream(&[chunk(
            json!({"tool_calls":[call_delta(0, "call_x", "read", "{\"path\":")]}),
            Some(reason),
        )]))
        .await;
        if valid {
            assert_eq!(completed(&trace).finish, Finish::Length);
            assert!(tool_calls(&completed(&trace).message)[0].arguments.is_err());
        } else {
            assert!(matches!(trace.result, Err(Error::Protocol(_))));
        }
    }
}

#[tokio::test]
async fn finish_reason_and_done_are_both_required() {
    for body in [
        "data: [DONE]\n\n".into(),
        stream(&[chunk(json!({"content":"partial"}), None)]),
        events(&[chunk(json!({"content":"complete?"}), Some("stop"))]),
        format!("{}data: [DONE]", events(&[chunk(json!({}), Some("stop"))])),
        stream(&[chunk(json!({"content":"partial"}), None), json!("[DONE]")]),
    ] {
        let trace = run(&body).await;
        assert!(matches!(trace.result, Err(Error::Protocol(_))), "{trace:?}");
        assert!(
            !trace
                .events
                .iter()
                .any(|event| matches!(event, Event::Completed { .. }))
        );
    }
}

#[tokio::test]
async fn invalid_chunks_and_provider_failures_keep_the_raw_evidence() {
    let first = chunk(json!({"content":"start"}), None);
    let mut multiple = chunk(json!({}), Some("stop"));
    multiple["choices"]
        .as_array_mut()
        .unwrap()
        .push(json!({"index":1, "delta":{}, "finish_reason":"stop"}));
    let mut wrong_id = chunk(json!({}), Some("stop"));
    wrong_id["id"] = "other_request".into();
    for bad in [
        multiple,
        wrong_id,
        chunk(json!({"content":12}), Some("stop")),
        chunk(json!({"role":"user"}), None),
        chunk(
            json!({"function_call":{"name":"read", "arguments":"{}"}}),
            Some("function_call"),
        ),
        chunk(
            json!({"tool_calls":[{"index":0, "type":"custom"}]}),
            Some("tool_calls"),
        ),
        json!({"error":{"message":"rate limited"}}),
    ] {
        let trace = run(&stream(&[first.clone(), bad.clone()])).await;
        assert!(trace.result.is_err(), "{trace:?}");
        assert!(matches!(trace.events.last(), Some(Event::Progress { raw }) if raw == &bad));
        if bad.get("error").is_some() {
            assert!(matches!(trace.result, Err(Error::Provider(_))));
        }
    }
    let after_finish = chunk(json!({"content":"late"}), None);
    let trace = run(&stream(&[
        chunk(json!({}), Some("stop")),
        after_finish.clone(),
    ]))
    .await;
    assert!(matches!(trace.result, Err(Error::Protocol(_))));
    assert!(matches!(trace.events.last(), Some(Event::Progress { raw }) if raw == &after_finish));
}

#[tokio::test]
async fn invalid_tool_batches_never_complete_successfully() {
    for calls in [
        vec![
            call_delta(0, "same", "read", "{}"),
            call_delta(1, "same", "list", "{}"),
        ],
        vec![call_delta(1, "gap", "read", "{}")],
        vec![call_delta(0, "empty_name", "", "{}")],
        vec![call_delta(0, "array_arguments", "read", "[]")],
        vec![],
    ] {
        let trace = run(&stream(&[chunk(
            json!({"tool_calls":calls}),
            Some("tool_calls"),
        )]))
        .await;
        assert!(matches!(trace.result, Err(Error::Protocol(_))), "{trace:?}");
    }
}

#[tokio::test]
async fn completion_releases_the_request_without_waiting_for_http_eof() {
    let (endpoint, server) =
        unfinished_body(stream(&[chunk(json!({"content":"done"}), Some("stop"))])).await;
    let model = client(&endpoint);
    let mut generation = model.generate(request()).unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if matches!(
                generation.next().await.unwrap().unwrap(),
                Event::Completed { .. }
            ) {
                break;
            }
        }
    })
    .await
    .unwrap();
    assert!(generation.is_terminated());
    server.await.unwrap();
    assert!(generation.next().await.is_none());
}
