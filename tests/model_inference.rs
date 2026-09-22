use futures_core::stream::FusedStream;
use futures_util::StreamExt;
use myco::model::{Event, Message, Request};

mod common;

use common::*;
use myco::model::{DeltaKind, Error, Finish, Output, Tool, ToolCall};
use serde_json::{Value, json};

#[test]
fn invalid_input_is_rejected_before_a_stream_is_returned() {
    let model = client(
        Backend::OpenAiResponses,
        "http://127.0.0.1:1/responses",
        "test",
    )
    .unwrap();
    for input in [
        Request::default(),
        Request {
            model: String::new(),
            ..request()
        },
    ] {
        assert!(matches!(
            model.generate(input),
            Err(Error::InvalidRequest(_))
        ));
    }
}

#[tokio::test]
async fn request_capture_precedes_any_network_io() {
    let model = client(
        Backend::OpenAiResponses,
        "http://127.0.0.1:1/responses",
        "test",
    )
    .unwrap();
    let trace = collect(
        &model,
        Request {
            messages: vec![Message::User("hello".into())],
            ..request()
        },
    )
    .await;
    let Event::Request { body } = &trace.events[0] else {
        panic!("expected request")
    };
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
    let client = client(Backend::OpenAiResponses, &url, "").unwrap();
    let mut generation = client.generate(request()).unwrap();
    assert!(matches!(
        generation.next().await,
        Some(Ok(Event::Request { .. }))
    ));
    assert!(matches!(generation.next().await,
        Some(Ok(Event::Progress { raw })) if raw == progress));
    assert!(matches!(generation.next().await,
        Some(Ok(Event::Delta(delta))) if delta.text == "hello" && delta.kind == DeltaKind::Text));
    assert!(matches!(generation.next().await,
        Some(Ok(Event::Progress { raw })) if raw == terminal));
    assert!(!generation.is_terminated());
    let Some(Ok(Event::Completed {
        message,
        finish,
        usage,
    })) = generation.next().await
    else {
        panic!("missing final response");
    };
    assert_eq!(
        message,
        Message::Assistant {
            output: vec![Output::Text("hello".into())]
        }
    );
    assert_eq!(finish, Finish::Stop);
    assert_eq!(usage.input_tokens, None);
    assert!(generation.is_terminated());
    assert!(generation.next().await.is_none());
    assert!(generation.next().await.is_none());
}

#[tokio::test]
async fn completed_messages_rebuild_full_history_including_reasoning_for_each_backend() {
    for (protocol, fixture_body) in [
        (Backend::OpenAiResponses, OPENAI),
        (Backend::AnthropicMessages, ANTHROPIC),
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
        let Message::Assistant { output } = &reply.message else {
            panic!("missing assistant message");
        };
        assert_eq!(reply.finish, Finish::ToolCalls);
        assert_eq!(reply.usage.output_tokens, Some(9));
        assert!(output.contains(&Output::Text("Ready 雪".into())));
        let call = output
            .iter()
            .find_map(|part| match part {
                Output::ToolCall(call) => Some(call),
                _ => None,
            })
            .unwrap();
        assert_eq!(call.id, "call_fixture");
        assert_eq!(call.arguments, Ok(json!({"path":"note.txt"})));
        let arguments: String = trace
            .events
            .iter()
            .filter_map(|e| match e {
                Event::Delta(delta) if delta.kind == DeltaKind::ToolArguments => {
                    Some(delta.text.as_str())
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            serde_json::from_str::<Value>(&arguments).unwrap(),
            *call.arguments.as_ref().unwrap()
        );
        let captured = capture.await.unwrap();
        let Event::Request { body, .. } = &trace.events[0] else {
            panic!()
        };
        assert_eq!(body, &captured.body);
        assert!(!body.to_string().contains("test-key"));
        assert!(captured.headers.starts_with("POST /inference HTTP/1.1"));

        let mut next = request();
        next.messages.extend([
            reply.message.clone(),
            Message::User("Continue.".into()),
            Message::ToolResult {
                call_id: call.id.clone(),
                output: "Note content".into(),
                is_error: false,
            },
        ]);
        assert_eq!(next.messages[1], reply.message);
        let restored = client(protocol, "http://127.0.0.1:1/inference", "").unwrap();
        let body = encoded_request(&restored, next).await;
        match protocol {
            Backend::OpenAiResponses => {
                assert!(captured.headers.contains("authorization: Bearer test-key"));
                assert_eq!(reply.usage.input_tokens, Some(12));
                assert_eq!(body["input"][1]["encrypted_content"], "opaque-reasoning");
                assert_eq!(body["input"][1]["id"], "rs_fixture");
                assert_eq!(
                    body["input"][1]["summary"],
                    json!([
                        {"type":"summary_text", "text":"Checking the note."}
                    ])
                );
                assert_eq!(body["input"][3]["call_id"], "call_fixture");
                assert_eq!(body["input"][5]["call_id"], "call_fixture");
            }
            Backend::AnthropicMessages => {
                assert!(captured.headers.contains("x-api-key: test-key"));
                assert!(captured.headers.contains("anthropic-version: 2023-06-01"));
                assert_eq!(reply.usage.input_tokens, Some(11));
                assert_eq!(
                    body["messages"][1]["content"][0]["signature"],
                    "signed-reasoning"
                );
                assert_eq!(
                    body["messages"][1]["content"][0]["thinking"],
                    "Checking the note."
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
async fn unknown_output_stays_in_raw_events_without_entering_history() {
    let mut raw = text_response("full response");
    raw["extra_provider_field"] = json!({"evidence":"retained"});
    raw["output"]
        .as_array_mut()
        .unwrap()
        .push(json!({"type":"future_output","payload":"retained"}));
    let trace = run(
        Backend::OpenAiResponses,
        &events(&[
            json!({"type":"future_event","payload":"retained"}),
            json!({"type":"response.completed","response":raw}),
        ]),
    )
    .await;
    let reply = completed(&trace);
    let Message::Assistant { output } = &reply.message else {
        panic!("missing assistant message");
    };
    assert_eq!(output, &[Output::Text("full response".into())]);
    assert_eq!(reply.usage.input_tokens, None);
    assert!(
        trace
            .events
            .iter()
            .any(|e| matches!(e, Event::Progress { raw: event, .. } if event["response"] == raw))
    );
    assert!(
        trace
            .events
            .iter()
            .any(|e| matches!(e, Event::Progress { raw } if raw["type"] == "future_event"))
    );
    let model = client(Backend::OpenAiResponses, "http://127.0.0.1:1/inference", "").unwrap();
    let mut next = request();
    next.messages.push(reply.message.clone());
    let body = encoded_request(&model, next).await;
    assert_eq!(
        &body["input"].as_array().unwrap()[1..],
        &[json!({"role":"assistant","content":"full response"})]
    );
}

#[tokio::test]
async fn output_limits_and_refusals_are_not_normal_completion() {
    let mut raw = text_response("partial");
    raw["status"] = "incomplete".into();
    raw["incomplete_details"] = json!({"reason":"max_output_tokens"});
    let trace = run(
        Backend::OpenAiResponses,
        &events(&[json!({"type":"response.incomplete","response":raw})]),
    )
    .await;
    assert_eq!(completed(&trace).finish, Finish::Length);
    let trace = run(
        Backend::AnthropicMessages,
        &ANTHROPIC.replace(
            "\"tool_use\",\"stop_sequence\"",
            "\"max_tokens\",\"stop_sequence\"",
        ),
    )
    .await;
    assert_eq!(completed(&trace).finish, Finish::Length);
    let raw = json!({"status":"completed","output":[{"type":"message","content":[{"type":"refusal","refusal":"Cannot comply"}]}]});
    let trace = run(
        Backend::OpenAiResponses,
        &events(&[json!({"type":"response.completed","response":raw})]),
    )
    .await;
    assert_eq!(completed(&trace).finish, Finish::Refusal);
}

#[tokio::test]
async fn a_closed_connection_or_done_marker_does_not_imply_completion() {
    for (protocol, body) in [
        (
            Backend::OpenAiResponses,
            "data: {\"type\":\"response.output_text.delta\",\"output_index\":0,\"delta\":\"partial\"}\n\n",
        ),
        (Backend::OpenAiResponses, "data: [DONE]\n\n"),
        (Backend::AnthropicMessages, "data: {\"type\":\"ping\"}\n\n"),
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
    let model = client(Backend::OpenAiResponses, &url, "test-key").unwrap();
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
    for protocol in [Backend::OpenAiResponses, Backend::AnthropicMessages] {
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
        Backend::OpenAiResponses,
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
        let trace = run(Backend::AnthropicMessages, &events(&sequence)).await;
        assert!(
            matches!(&trace.result, Err(Error::Protocol(_))),
            "{trace:?}"
        );
    }
}

#[test]
fn unmatched_and_duplicate_tool_results_are_rejected_before_dispatch() {
    for backend in [Backend::OpenAiResponses, Backend::AnthropicMessages] {
        let model = client(backend, "http://127.0.0.1:1/inference", "").unwrap();
        for id in ["missing", "call"] {
            let mut input = tool_history(Ok(json!({})));
            input.messages.push(Message::ToolResult {
                call_id: id.into(),
                output: "extra result".into(),
                is_error: false,
            });
            assert!(matches!(model.generate(input),
                Err(Error::InvalidRequest(error)) if error.contains("unmatched or duplicate")));
        }
    }
}

#[tokio::test]
async fn edited_text_history_is_rebuilt_from_the_callers_content() {
    for (protocol, fixture_body) in [
        (Backend::OpenAiResponses, OPENAI),
        (Backend::AnthropicMessages, ANTHROPIC),
    ] {
        let trace = run(protocol, fixture_body).await;
        let mut message = completed(&trace).message.clone();
        let Message::Assistant { output, .. } = &mut message else {
            panic!("missing assistant message");
        };
        let text = output
            .iter_mut()
            .find(|part| matches!(part, Output::Text(_)))
            .unwrap();
        *text = Output::Text("edited text".into());
        let mut input = request();
        input.messages.extend([
            message,
            Message::ToolResult {
                call_id: "call_fixture".into(),
                output: "note contents".into(),
                is_error: false,
            },
        ]);
        let model = client(protocol, "http://127.0.0.1:1/inference", "").unwrap();
        let body = encoded_request(&model, input).await.to_string();
        assert!(body.contains("edited text"));
        assert!(!body.contains("Ready 雪"));
    }
}

#[tokio::test]
async fn dropping_an_incomplete_generation_closes_the_http_request() {
    let body =
        events(&[json!({"type":"response.output_text.delta","output_index":0,"delta":"started"})]);
    let (endpoint, server) = unfinished_body(body).await;
    let model = client(Backend::OpenAiResponses, &endpoint, "").unwrap();
    let mut generation = model.generate(request()).unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        assert!(matches!(
            generation.next().await,
            Some(Ok(Event::Request { .. }))
        ));
        assert!(matches!(
            generation.next().await,
            Some(Ok(Event::Progress { .. }))
        ));
        assert!(matches!(generation.next().await, Some(Ok(Event::Delta(_)))));
    })
    .await
    .unwrap();
    drop(generation);
    server.await.unwrap();
}

#[tokio::test]
async fn completion_releases_the_http_request_without_dropping_or_polling_again() {
    for (protocol, body) in [
        (Backend::OpenAiResponses, OPENAI),
        (Backend::AnthropicMessages, ANTHROPIC),
    ] {
        let (endpoint, server) = unfinished_body(body.into()).await;
        let model = client(protocol, &endpoint, "").unwrap();
        let mut generation = model.generate(request()).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let event = generation
                    .next()
                    .await
                    .expect("missing completion")
                    .unwrap();
                if matches!(event, Event::Completed { .. }) {
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
    for protocol in [Backend::OpenAiResponses, Backend::AnthropicMessages] {
        concurrent_requests(protocol).await;
    }
}

async fn concurrent_requests(protocol: Backend) {
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
    let a = Request {
        messages: vec![Message::User("a".into())],
        ..request()
    };
    let b = Request {
        messages: vec![Message::User("b".into())],
        ..request()
    };
    let (a, b) = tokio::join!(collect(&model, a), collect(&model, b));
    assert!(matches!(&completed(&a).message,
        Message::Assistant { output, .. } if output == &[Output::Text("a".into())]));
    assert!(matches!(&completed(&b).message,
        Message::Assistant { output, .. } if output == &[Output::Text("b".into())]));
    server.await.unwrap();
}

fn echo(protocol: Backend, request: &Value) -> String {
    match protocol {
        Backend::OpenAiResponses => {
            let text = request["input"][0]["content"].as_str().unwrap();
            events(&[json!({"type":"response.completed","response":text_response(text)})])
        }
        Backend::AnthropicMessages => {
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
        Backend::OpenAiResponses,
        &events(&[json!({"type":"response.completed","response":raw})]),
    )
    .await;
    assert!(matches!(&trace.result, Err(Error::Protocol(_))));
}

#[tokio::test]
async fn driver_options_are_per_request_and_cannot_replace_context() {
    let model = client(Backend::OpenAiResponses, "http://127.0.0.1:1/responses", "").unwrap();
    let mut input = request();
    input
        .driver_options
        .insert("reasoning".into(), json!({"effort":"high"}));
    assert_eq!(
        encoded_request(&model, input).await["reasoning"],
        json!({"effort":"high"})
    );
    assert!(
        encoded_request(&model, request())
            .await
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
        input.driver_options.insert(key.into(), Value::Null);
        assert!(
            matches!(model.generate(input), Err(Error::InvalidRequest(_))),
            "{key}"
        );
    }
}

#[tokio::test]
async fn visible_reasoning_and_truncated_arguments_remain_observations() {
    let raw = json!({"status":"incomplete","incomplete_details":{"reason":"max_output_tokens"},"output":[
        {"type":"reasoning","summary":[],"content":[{"type":"reasoning_text","text":"visible reasoning"}]},
        {"type":"function_call","call_id":"c","name":"read","arguments":"{"}
    ]});
    let trace = run(
        Backend::OpenAiResponses,
        &events(&[json!({"type":"response.incomplete", "response":raw})]),
    )
    .await;
    let reply = completed(&trace);
    let Message::Assistant { output } = &reply.message else {
        panic!("missing assistant message");
    };
    assert_eq!(reply.finish, Finish::Length);
    assert_eq!(
        output[0],
        Output::Reasoning {
            text: "visible reasoning".into(),
            signature: None
        }
    );
    let Output::ToolCall(call) = &output[1] else {
        panic!("missing truncated call");
    };
    assert!(
        call.arguments
            .as_ref()
            .is_err_and(|error| !error.is_empty())
    );
    assert!(trace.events.iter().any(
        |e| matches!(e, Event::Progress { raw, .. } if raw["response"]["output"][1]["arguments"] == "{")
    ));
    let model = client(Backend::OpenAiResponses, "http://127.0.0.1:1/responses", "").unwrap();
    let mut input = request();
    input.messages.push(reply.message.clone());
    assert!(matches!(
        model.generate(input),
        Err(Error::InvalidRequest(_))
    ));
}

#[tokio::test]
async fn parsed_tool_arguments_encode_in_each_providers_wire_format() {
    let arguments = json!({"path":"note.txt", "lines":[1, 2]});
    for protocol in [Backend::OpenAiResponses, Backend::AnthropicMessages] {
        let model = client(protocol, "http://127.0.0.1:1/inference", "").unwrap();
        let body = encoded_request(&model, tool_history(Ok(arguments.clone()))).await;
        match protocol {
            Backend::OpenAiResponses => {
                let encoded = body["input"][1]["arguments"].as_str().unwrap();
                assert_eq!(serde_json::from_str::<Value>(encoded).unwrap(), arguments);
            }
            Backend::AnthropicMessages => {
                assert_eq!(body["messages"][1]["content"][0]["input"], arguments);
            }
        }
    }
}

#[test]
fn invalid_tool_arguments_cannot_be_used_in_history() {
    for protocol in [Backend::OpenAiResponses, Backend::AnthropicMessages] {
        let model = client(protocol, "http://127.0.0.1:1/inference", "").unwrap();
        for arguments in [Err("incomplete JSON".into()), Ok(json!([1, 2]))] {
            assert!(matches!(
                model.generate(tool_history(arguments)),
                Err(Error::InvalidRequest(_))
            ));
        }
    }
}

fn tool_history(arguments: Result<Value, String>) -> Request {
    let mut input = request();
    input.messages.extend([
        Message::Assistant {
            output: vec![Output::ToolCall(ToolCall {
                id: "call".into(),
                name: "read".into(),
                arguments,
            })],
        },
        Message::ToolResult {
            call_id: "call".into(),
            output: "note contents".into(),
            is_error: false,
        },
    ]);
    input
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

    let model = client(Backend::OpenAiResponses, &endpoint, "").unwrap();
    let mut generation = model.generate(request()).unwrap();
    assert!(matches!(
        generation.next().await,
        Some(Ok(Event::Request { .. }))
    ));
    assert!(matches!(
        timeout(Duration::from_secs(5), generation.next())
            .await
            .unwrap(),
        Some(Ok(Event::Progress { .. }))
    ));
    assert!(matches!(generation.next().await,
        Some(Ok(Event::Delta(delta))) if delta.text == "first"));
    assert!(
        timeout(Duration::from_millis(30), generation.next())
            .await
            .is_err()
    );
    release.send(()).unwrap();
    assert!(matches!(
        timeout(Duration::from_secs(5), generation.next())
            .await
            .unwrap(),
        Some(Ok(Event::Progress { .. }))
    ));
    assert!(matches!(generation.next().await,
        Some(Ok(Event::Delta(delta))) if delta.text == "雪"));
    assert!(matches!(
        generation.next().await,
        Some(Ok(Event::Progress { .. }))
    ));
    let Some(Ok(Event::Completed { message, .. })) = generation.next().await else {
        panic!("lost the resumed response");
    };
    assert_eq!(
        message,
        Message::Assistant {
            output: vec![Output::Text("first雪".into())]
        }
    );
    assert!(generation.next().await.is_none());
    server.await.unwrap();
}

#[tokio::test]
async fn interleaved_deltas_keep_part_coordinates_and_completion_supplies_the_whole_message() {
    let raw = json!({"status":"completed", "output":[{"type":"message", "content":[
        {"type":"output_text", "text":"a雪"}, {"type":"output_text", "text":"b"}
    ]}]});
    let trace = run(Backend::OpenAiResponses, &events(&[
        json!({"type":"response.output_text.delta", "output_index":0, "content_index":1, "delta":"b"}),
        json!({"type":"response.output_text.delta", "output_index":0, "content_index":0, "delta":"a"}),
        json!({"type":"response.completed", "response":raw}),
    ])).await;
    assert_eq!(
        completed(&trace).message,
        Message::Assistant {
            output: vec![Output::Text("a雪".into()), Output::Text("b".into())],
        }
    );
    let text: Vec<_> = trace
        .events
        .iter()
        .filter_map(|event| match event {
            Event::Delta(delta) => Some((delta.part, delta.text.as_str())),
            _ => None,
        })
        .collect();
    assert_eq!(text, [(1, "b"), (0, "a")]);
}

#[tokio::test]
async fn initial_thinking_and_signature_fragments_are_assembled_once() {
    let body = ANTHROPIC.replace(
        "\"thinking\":\"\",\"signature\":\"\"",
        "\"thinking\":\"Plan. \",\"signature\":\"prefix-\"",
    );
    let trace = run(Backend::AnthropicMessages, &body).await;
    let Message::Assistant { output } = &completed(&trace).message else {
        panic!()
    };
    assert_eq!(
        output[0],
        Output::Reasoning {
            text: "Plan. Checking the note.".into(),
            signature: Some("prefix-signed-reasoning".into()),
        }
    );
    assert_eq!(
        output[1],
        Output::RedactedReasoning("opaque-reasoning".into())
    );
}

#[tokio::test]
async fn encrypted_reasoning_survives_without_a_visible_summary() {
    let raw = json!({"status":"completed", "output":[{
        "type":"reasoning", "id":"rs_secret", "summary":[], "encrypted_content":"opaque",
        "content":[{"type":"reasoning_text", "text":"internal trace"}]
    }]});
    let trace = run(
        Backend::OpenAiResponses,
        &events(&[json!({"type":"response.completed", "response":raw})]),
    )
    .await;
    assert_eq!(
        completed(&trace).message,
        Message::Assistant {
            output: vec![Output::EncryptedReasoning {
                id: "rs_secret".into(),
                summary: vec![],
                data: "opaque".into(),
            }],
        }
    );
}

#[tokio::test]
async fn argument_whitespace_is_not_replaced_or_duplicated_at_completion() {
    let body = ANTHROPIC
        .replace("{\\\"path\\\":", "{ \\\"path\\\" : ")
        .replace("\\\"note.txt\\\"}", "\\\"note.txt\\\" }");
    let trace = run(Backend::AnthropicMessages, &body).await;
    let arguments: String = trace
        .events
        .iter()
        .filter_map(|event| match event {
            Event::Delta(delta) if delta.kind == DeltaKind::ToolArguments => {
                Some(delta.text.as_str())
            }
            _ => None,
        })
        .collect();
    assert_eq!(arguments, "{ \"path\" : \"note.txt\" }");
    assert!(trace.result.is_ok());
}

#[tokio::test]
async fn unsigned_reasoning_is_observation_only_in_history() {
    for backend in [Backend::OpenAiResponses, Backend::AnthropicMessages] {
        let model = client(backend, "http://127.0.0.1:1/inference", "").unwrap();
        let mut input = request();
        input.messages.extend([
            Message::Assistant {
                output: vec![Output::Reasoning {
                    text: "private observation".into(),
                    signature: None,
                }],
            },
            Message::User("next question".into()),
        ]);
        let body = encoded_request(&model, input).await;
        assert!(!body.to_string().contains("private observation"));
        match backend {
            Backend::OpenAiResponses => assert_eq!(body["input"].as_array().unwrap().len(), 2),
            Backend::AnthropicMessages => assert_eq!(body["messages"].as_array().unwrap().len(), 1),
        }
    }
}

#[test]
fn incompatible_reasoning_formats_fail_before_network_io() {
    for (backend, output) in [
        (
            Backend::OpenAiResponses,
            Output::Reasoning {
                text: "summary".into(),
                signature: Some("signed".into()),
            },
        ),
        (
            Backend::OpenAiResponses,
            Output::RedactedReasoning("opaque".into()),
        ),
        (
            Backend::AnthropicMessages,
            Output::EncryptedReasoning {
                id: "rs".into(),
                summary: vec![],
                data: "opaque".into(),
            },
        ),
    ] {
        let model = client(backend, "http://127.0.0.1:1/inference", "").unwrap();
        let mut input = request();
        input.messages.push(Message::Assistant {
            output: vec![output],
        });
        assert!(
            matches!(model.generate(input), Err(Error::InvalidRequest(error))
            if error.contains("another backend"))
        );
    }
}
