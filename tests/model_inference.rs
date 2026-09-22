use futures_core::stream::FusedStream;
use futures_util::StreamExt;
use myco::model::{Event, Message, Protocol, Request};

mod common;

use common::*;
use myco::model::{DeltaKind, Error, Finish, Output, Response, Tool};
use serde_json::{Value, json};

#[tokio::test]
async fn an_invalid_request_finishes_without_contacting_the_endpoint() {
    let model = client(
        Protocol::OpenAiResponses,
        "http://127.0.0.1:1/responses",
        "test",
    )
    .unwrap();
    let mut request = Request::new("test-model", vec![Message::User("hello".into())], 64);
    request
        .provider_options
        .insert("stream".into(), false.into());
    let trace = collect(&model, request).await;
    assert!(matches!(trace.result, Err(Error::InvalidRequest(_))));
    assert!(trace.events.is_empty());
}

#[tokio::test]
async fn request_capture_precedes_any_network_io() {
    let model = client(
        Protocol::OpenAiResponses,
        "http://127.0.0.1:1/responses",
        "test",
    )
    .unwrap();
    let trace = collect(
        &model,
        Request::new("test-model", vec![Message::User("hello".into())], 64),
    )
    .await;
    let Event::Request { protocol, body } = &trace.events[0] else {
        panic!("expected request")
    };
    assert_eq!(*protocol, Protocol::OpenAiResponses);
    assert_eq!(body["store"], false);
    assert_eq!(body["stream"], true);
    assert_eq!(body["input"][0]["content"], "hello");
    assert!(matches!(trace.result, Err(Error::Transport(_))));
}

#[tokio::test]
async fn ordered_progress_precedes_one_final_response_and_permanent_exhaustion() {
    let progress = json!({"type":"response.output_text.delta","output_index":0,"delta":"hello"});
    let terminal = json!({"type":"response.completed","response":text_response("hello")});
    let (url, _capture) = fixture(&events(&[progress.clone(), terminal.clone()]), 200, "").await;
    let client = client(Protocol::OpenAiResponses, &url, "").unwrap();
    let mut generation = client.generate(request());
    assert!(matches!(
        generation.next().await,
        Some(Ok(Event::Request { .. }))
    ));
    for expected in [progress, terminal] {
        assert!(matches!(generation.next().await,
            Some(Ok(Event::Progress { raw, .. })) if raw == expected));
        assert!(!generation.is_terminated());
    }
    let Some(Ok(Event::Completed(response))) = generation.next().await else {
        panic!("missing final response");
    };
    assert_eq!(response.output(), &[Output::Text("hello".into())]);
    assert!(generation.is_terminated());
    assert!(generation.next().await.is_none());
    assert!(generation.next().await.is_none());
}

#[tokio::test]
async fn both_protocols_stream_text_and_tools_and_retain_native_continuations() {
    for (protocol, fixture_body) in [
        (Protocol::OpenAiResponses, OPENAI),
        (Protocol::AnthropicMessages, ANTHROPIC),
    ] {
        let (url, capture) = fixture(fixture_body, 200, "").await;
        let model = client(protocol, &url, "test-key").unwrap();
        let mut initial = request();
        initial.instructions = "Use the supplied tools.".into();
        initial.tools.push(Tool {
            name: "read_note".into(),
            description: "Read a note".into(),
            parameters: json!({"type":"object","properties":{"path":{"type":"string"}}}),
        });
        let trace = collect(&model, initial).await;
        let reply = completed(&trace);
        assert_eq!(reply.finish(), &Finish::ToolCalls);
        assert_eq!(reply.usage().output_tokens, Some(9));
        assert_eq!(
            reply.output()[0],
            Output::Reasoning("Checking the note.".into())
        );
        assert_eq!(reply.output()[1], Output::Text("Ready 雪".into()));
        let Output::ToolCall(call) = &reply.output()[2] else {
            panic!()
        };
        assert_eq!(call.id, "call_fixture");
        assert_eq!(
            serde_json::from_str::<Value>(&call.arguments).unwrap(),
            json!({"path":"note.txt"})
        );
        let arguments: String = trace
            .events
            .iter()
            .filter_map(|e| match e {
                Event::Progress {
                    delta: Some(delta), ..
                } if delta.kind == DeltaKind::ToolArguments => Some(delta.text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(arguments, call.arguments);
        let captured = capture.await.unwrap();
        let Event::Request { body, .. } = &trace.events[0] else {
            panic!()
        };
        assert_eq!(body, &captured.body);
        assert!(!body.to_string().contains("test-key"));
        assert!(captured.headers.starts_with("POST /inference HTTP/1.1"));

        let native = reply.provider().unwrap();
        assert_eq!(
            Response::from_provider(protocol, native.body.clone()).unwrap(),
            *reply
        );
        let mut next = request();
        next.messages.extend([
            Message::Assistant(reply.clone()),
            Message::User("Continue.".into()),
            Message::ToolResult {
                call_id: call.id.clone(),
                output: "Note content".into(),
                is_error: false,
            },
        ]);
        let body = model.request_body(&next).unwrap();
        match protocol {
            Protocol::OpenAiResponses => {
                assert!(captured.headers.contains("authorization: Bearer test-key"));
                assert_eq!(reply.usage().input_tokens, Some(12));
                assert_eq!(body["input"][1], native.body["output"][0]);
                assert_eq!(body["input"][1]["encrypted_content"], "opaque-reasoning");
                assert_eq!(body["input"][3]["call_id"], "call_fixture");
                assert_eq!(body["input"][5]["call_id"], "call_fixture");
            }
            Protocol::AnthropicMessages => {
                assert!(captured.headers.contains("x-api-key: test-key"));
                assert!(captured.headers.contains("anthropic-version: 2023-06-01"));
                assert_eq!(reply.usage().input_tokens, Some(11));
                assert_eq!(body["messages"][1]["content"], native.body["content"]);
                assert_eq!(
                    body["messages"][1]["content"][0]["signature"],
                    "signed-reasoning"
                );
                assert_eq!(
                    body["messages"][1]["content"][1]["data"],
                    "opaque-reasoning"
                );
                assert_eq!(
                    body["messages"][2]["content"][0]["tool_use_id"],
                    "call_fixture"
                );
                assert_eq!(body["messages"][2]["content"][1]["text"], "Continue.");
            }
        }
    }
}

#[tokio::test]
async fn terminal_response_is_authoritative_even_without_text_deltas() {
    let mut raw = text_response("full response");
    raw["extra_provider_field"] = json!({"evidence":"retained"});
    let trace = run(
        Protocol::OpenAiResponses,
        &events(&[
            json!({"type":"future_event","payload":"retained"}),
            json!({"type":"response.completed","response":raw}),
        ]),
    )
    .await;
    let reply = completed(&trace);
    assert_eq!(reply.output(), &[Output::Text("full response".into())]);
    assert_eq!(reply.usage().input_tokens, None);
    assert_eq!(reply.provider().unwrap().body, raw);
    assert!(trace.events.iter().any(
        |e| matches!(e, Event::Progress { raw, delta: None } if raw["type"] == "future_event")
    ));
}

#[tokio::test]
async fn output_limits_and_refusals_are_not_normal_completion() {
    let mut raw = text_response("partial");
    raw["status"] = "incomplete".into();
    raw["incomplete_details"] = json!({"reason":"max_output_tokens"});
    let trace = run(
        Protocol::OpenAiResponses,
        &events(&[json!({"type":"response.incomplete","response":raw})]),
    )
    .await;
    assert_eq!(completed(&trace).finish(), &Finish::Length);
    let trace = run(
        Protocol::AnthropicMessages,
        &ANTHROPIC.replace(
            "\"tool_use\",\"stop_sequence\"",
            "\"max_tokens\",\"stop_sequence\"",
        ),
    )
    .await;
    assert_eq!(completed(&trace).finish(), &Finish::Length);
    let raw = json!({"status":"completed","output":[{"type":"message","content":[{"type":"refusal","refusal":"Cannot comply"}]}]});
    let trace = run(
        Protocol::OpenAiResponses,
        &events(&[json!({"type":"response.completed","response":raw})]),
    )
    .await;
    assert_eq!(completed(&trace).finish(), &Finish::Refusal);
}

#[tokio::test]
async fn a_closed_connection_or_done_marker_does_not_imply_completion() {
    for (protocol, body) in [
        (
            Protocol::OpenAiResponses,
            "data: {\"type\":\"response.output_text.delta\",\"output_index\":0,\"delta\":\"partial\"}\n\n",
        ),
        (Protocol::OpenAiResponses, "data: [DONE]\n\n"),
        (Protocol::AnthropicMessages, "data: {\"type\":\"ping\"}\n\n"),
    ] {
        let trace = run(protocol, body).await;
        assert!(
            matches!(&trace.result, Err(Error::Protocol(_))),
            "{trace:?}"
        );
    }
}

#[tokio::test]
async fn http_failures_keep_status_request_id_and_retry_advice() {
    let (url, capture) = fixture(
        "rate limit",
        429,
        "x-request-id: req_fixture\r\nRetry-After: 3\r\n",
    )
    .await;
    let model = client(Protocol::OpenAiResponses, &url, "test-key").unwrap();
    let trace = collect(&model, request()).await;
    assert_eq!(trace.events.len(), 1);
    let Err(Error::Http {
        status,
        body,
        request_id,
        retry_after,
    }) = &trace.result
    else {
        panic!("{trace:?}")
    };
    assert_eq!(*status, 429);
    assert_eq!(body, "rate limit");
    assert_eq!(request_id.as_deref(), Some("req_fixture"));
    assert_eq!(retry_after.as_deref(), Some("3"));
    capture.await.unwrap();
}

#[tokio::test]
async fn provider_failures_preserve_the_last_event_and_never_complete() {
    for protocol in [Protocol::OpenAiResponses, Protocol::AnthropicMessages] {
        let error = json!({"type":"error","error":{"type":"overloaded_error","message":"busy"}});
        let trace = run(protocol, &events(std::slice::from_ref(&error))).await;
        assert!(matches!(&trace.result, Err(Error::Provider(raw)) if raw == &error));
        assert!(
            trace
                .events
                .iter()
                .any(|e| matches!(e, Event::Progress { raw, .. } if raw == &error))
        );
    }
}

#[tokio::test]
async fn malformed_terminal_output_still_has_a_trace_record() {
    let event = json!({"type":"response.completed","response":{"status":"completed","output":[{"type":"function_call","call_id":"c","name":"read","arguments":"{"}]}});
    let trace = run(
        Protocol::OpenAiResponses,
        &events(std::slice::from_ref(&event)),
    )
    .await;
    assert!(matches!(&trace.result, Err(Error::Protocol(_))));
    assert!(
        trace
            .events
            .iter()
            .any(|e| matches!(e, Event::Progress { raw, .. } if raw == &event))
    );
}

#[tokio::test]
async fn malformed_anthropic_sequences_fail_without_a_completed_message() {
    let start = json!({"type":"message_start","message":{"id":"m","type":"message","role":"assistant","content":[],"stop_reason":null}});
    let block =
        json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}});
    for sequence in [
        vec![start.clone(), start.clone()],
        vec![start.clone(), block.clone(), block.clone()],
        vec![start.clone(), block.clone(), json!({"type":"message_stop"})],
        vec![
            start.clone(),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"lost"}}),
        ],
        vec![
            start.clone(),
            block,
            json!({"type":"content_block_stop","index":0}),
            json!({"type":"message_stop"}),
        ],
    ] {
        let trace = run(Protocol::AnthropicMessages, &events(&sequence)).await;
        assert!(
            matches!(&trace.result, Err(Error::Protocol(_))),
            "{trace:?}"
        );
    }
}

#[tokio::test]
async fn continuation_protocol_and_tool_links_are_checked_before_dispatch() {
    let trace = run(Protocol::AnthropicMessages, ANTHROPIC).await;
    let response = completed(&trace).clone();
    let model = client(
        Protocol::OpenAiResponses,
        "http://127.0.0.1:1/responses",
        "",
    )
    .unwrap();
    let mut input = request();
    input.messages.push(Message::Assistant(response));
    assert!(matches!(
        model.request_body(&input),
        Err(Error::InvalidRequest(_))
    ));
    input.messages = vec![Message::ToolResult {
        call_id: "missing".into(),
        output: "orphan".into(),
        is_error: false,
    }];
    assert!(matches!(
        model.request_body(&input),
        Err(Error::InvalidRequest(_))
    ));
}

#[tokio::test]
async fn dropping_an_incomplete_generation_closes_the_http_request() {
    let body =
        events(&[json!({"type":"response.output_text.delta","output_index":0,"delta":"started"})]);
    let (endpoint, server) = unfinished_body(body).await;
    let model = client(Protocol::OpenAiResponses, &endpoint, "").unwrap();
    let mut generation = model.generate(request());
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        assert!(matches!(
            generation.next().await,
            Some(Ok(Event::Request { .. }))
        ));
        assert!(matches!(
            generation.next().await,
            Some(Ok(Event::Progress { delta: Some(_), .. }))
        ));
    })
    .await
    .unwrap();
    drop(generation);
    server.await.unwrap();
}

#[tokio::test]
async fn completion_releases_the_http_request_without_dropping_or_polling_again() {
    for (protocol, body) in [
        (Protocol::OpenAiResponses, OPENAI),
        (Protocol::AnthropicMessages, ANTHROPIC),
    ] {
        let (endpoint, server) = unfinished_body(body.into()).await;
        let model = client(protocol, &endpoint, "").unwrap();
        let mut generation = model.generate(request());
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let event = generation
                    .next()
                    .await
                    .expect("missing completion")
                    .unwrap();
                if matches!(event, Event::Completed(_)) {
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
}

#[tokio::test]
async fn two_requests_share_a_client_without_serializing_or_mixing_outputs() {
    for protocol in [Protocol::OpenAiResponses, Protocol::AnthropicMessages] {
        concurrent_requests(protocol).await;
    }
}

async fn concurrent_requests(protocol: Protocol) {
    use std::sync::Arc;
    use tokio::{io::AsyncWriteExt, net::TcpListener, sync::Barrier};

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/responses", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let barrier = Arc::new(Barrier::new(2));
        let mut handlers = vec![];
        for _ in 0..2 {
            let (mut socket, _) = listener.accept().await.unwrap();
            let barrier = barrier.clone();
            handlers.push(tokio::spawn(async move {
                let captured = read_request(&mut socket).await;
                barrier.wait().await;
                let body = echo(protocol, &captured.body);
                let headers = format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\n\r\n", body.len());
                socket.write_all(headers.as_bytes()).await.unwrap();
                socket.write_all(body.as_bytes()).await.unwrap();
            }));
        }
        for handler in handlers {
            handler.await.unwrap();
        }
    });
    let model = client(protocol, &endpoint, "").unwrap();
    let a = Request::new("test-model", vec![Message::User("a".into())], 64);
    let b = Request::new("test-model", vec![Message::User("b".into())], 64);
    let (a, b) = tokio::join!(collect(&model, a), collect(&model, b));
    assert_eq!(completed(&a).output(), &[Output::Text("a".into())]);
    assert_eq!(completed(&b).output(), &[Output::Text("b".into())]);
    server.await.unwrap();
}

fn echo(protocol: Protocol, request: &Value) -> String {
    match protocol {
        Protocol::OpenAiResponses => {
            let text = request["input"][0]["content"].as_str().unwrap();
            events(&[json!({"type":"response.completed","response":text_response(text)})])
        }
        Protocol::AnthropicMessages => {
            let text = &request["messages"][0]["content"][0]["text"];
            events(&[
                json!({"type":"message_start","message":{"type":"message","role":"assistant","content":[]}}),
                json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
                json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":text}}),
                json!({"type":"content_block_stop","index":0}),
                json!({"type":"message_delta","delta":{"stop_reason":"end_turn"}}),
                json!({"type":"message_stop"}),
            ])
        }
    }
}

#[tokio::test]
async fn duplicate_calls_cannot_be_mistaken_for_distinct_operations() {
    let call = json!({"type":"function_call","call_id":"same","name":"read","arguments":"{}"});
    let raw = json!({"status":"completed","output":[call.clone(),call]});
    let trace = run(
        Protocol::OpenAiResponses,
        &events(&[json!({"type":"response.completed","response":raw})]),
    )
    .await;
    assert!(matches!(&trace.result, Err(Error::Protocol(_))));
}

#[tokio::test]
async fn provider_settings_are_per_request_and_cannot_replace_context() {
    let model = client(
        Protocol::OpenAiResponses,
        "http://127.0.0.1:1/responses",
        "",
    )
    .unwrap();
    let mut input = request();
    input
        .provider_options
        .insert("reasoning".into(), json!({"effort":"high"}));
    assert_eq!(
        model.request_body(&input).unwrap()["reasoning"],
        json!({"effort":"high"})
    );
    assert!(
        model
            .request_body(&request())
            .unwrap()
            .get("reasoning")
            .is_none()
    );
    for key in [
        "model",
        "messages",
        "input",
        "tools",
        "system",
        "instructions",
        "store",
        "stream",
        "previous_response_id",
        "conversation",
        "background",
    ] {
        let mut input = request();
        input.provider_options.insert(key.into(), Value::Null);
        assert!(
            matches!(model.request_body(&input), Err(Error::InvalidRequest(_))),
            "{key}"
        );
    }
}

#[test]
fn visible_reasoning_and_truncated_arguments_remain_observations() {
    let raw = json!({"status":"incomplete","incomplete_details":{"reason":"max_output_tokens"},"output":[
        {"type":"reasoning","summary":[],"content":[{"type":"reasoning_text","text":"visible reasoning"}]},
        {"type":"function_call","call_id":"c","name":"read","arguments":"{"}
    ]});
    let reply = Response::from_provider(Protocol::OpenAiResponses, raw).unwrap();
    assert_eq!(reply.finish(), &Finish::Length);
    assert_eq!(
        reply.output()[0],
        Output::Reasoning("visible reasoning".into())
    );
    let model = client(
        Protocol::OpenAiResponses,
        "http://127.0.0.1:1/responses",
        "",
    )
    .unwrap();
    let mut input = request();
    input.messages.push(Message::Assistant(reply));
    assert!(matches!(
        model.request_body(&input),
        Err(Error::InvalidRequest(_))
    ));
}

#[tokio::test]
async fn dropping_a_pending_next_wait_preserves_the_attempt_and_partial_frame() {
    use std::time::Duration;
    use tokio::{io::AsyncWriteExt, net::TcpListener, sync::oneshot, time::timeout};

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/responses", listener.local_addr().unwrap());
    let (release, resume) = oneshot::channel();
    let initial =
        events(&[json!({"type":"response.output_text.delta","output_index":0,"delta":"first"})]);
    let rest = events(&[
        json!({"type":"response.output_text.delta","output_index":0,"delta":"雪"}),
        json!({"type":"response.completed","response":text_response("first雪")}),
    ]);
    let split = rest.find('雪').unwrap() + 1;
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        read_request(&mut socket).await;
        let headers = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\n\r\n",
            initial.len() + rest.len()
        );
        socket.write_all(headers.as_bytes()).await.unwrap();
        socket.write_all(initial.as_bytes()).await.unwrap();
        socket.write_all(&rest.as_bytes()[..split]).await.unwrap();
        resume.await.unwrap();
        socket.write_all(&rest.as_bytes()[split..]).await.unwrap();
    });

    let model = client(Protocol::OpenAiResponses, &endpoint, "").unwrap();
    let mut generation = model.generate(request());
    assert!(matches!(
        generation.next().await,
        Some(Ok(Event::Request { .. }))
    ));
    assert!(
        matches!(timeout(Duration::from_secs(5), generation.next()).await.unwrap(),
        Some(Ok(Event::Progress { delta: Some(delta), .. })) if delta.text == "first")
    );
    assert!(
        timeout(Duration::from_millis(30), generation.next())
            .await
            .is_err()
    );
    release.send(()).unwrap();
    assert!(
        matches!(timeout(Duration::from_secs(5), generation.next()).await.unwrap(),
        Some(Ok(Event::Progress { delta: Some(delta), .. })) if delta.text == "雪")
    );
    assert!(matches!(
        generation.next().await,
        Some(Ok(Event::Progress { .. }))
    ));
    let Some(Ok(Event::Completed(response))) = generation.next().await else {
        panic!("lost the resumed response");
    };
    assert_eq!(response.output(), &[Output::Text("first雪".into())]);
    assert!(generation.next().await.is_none());
    server.await.unwrap();
}
