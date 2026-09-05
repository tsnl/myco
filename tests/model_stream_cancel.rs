use std::time::Duration;

use futures::StreamExt;
use myco::generative_model::{
    BackendConfig, GenerativeModelConfig, Message, MessagePart, ModelSpec, OpenAIBackendConfig,
    Protocol, ThinkingMode,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;

#[tokio::test]
async fn dropping_generation_closes_request_waiting_for_headers() {
    assert_cancel_closes_connection("").await;
}

#[tokio::test]
async fn dropping_generation_closes_stalled_error_body() {
    assert_cancel_closes_connection("HTTP/1.1 503 Unavailable\r\nContent-Length: 100\r\n\r\n")
        .await;
}

#[tokio::test]
async fn dropping_generation_closes_stalled_event_stream() {
    assert_cancel_closes_connection(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n",
    )
    .await;
}

async fn assert_cancel_closes_connection(headers: &'static str) {
    tokio::time::timeout(Duration::from_secs(5), run_cancellation_case(headers))
        .await
        .expect("provider cancellation case stalled");
}

async fn run_cancellation_case(headers: &'static str) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base_url = format!("http://{}", listener.local_addr().unwrap());
    let (ready_tx, ready_rx) = oneshot::channel();
    let server = tokio::spawn(observe_connection_close(listener, headers, ready_tx));
    let mut generation = model(&base_url).generate(&[Message::UserMessage { content: vec![] }]);
    ready_rx.await.unwrap();
    if headers.starts_with("HTTP/1.1 200") {
        assert!(matches!(
            generation.next().await,
            Some(Ok(MessagePart::MessageStart))
        ));
    }
    drop(generation);
    let read = tokio::time::timeout(Duration::from_secs(1), server)
        .await
        .expect("cancelled generation kept the provider connection alive")
        .unwrap();
    assert!(
        matches!(read, Ok(0)),
        "expected connection closure, got {read:?}"
    );
}

async fn observe_connection_close(
    listener: TcpListener,
    headers: &str,
    ready: oneshot::Sender<()>,
) -> std::io::Result<usize> {
    let (mut socket, _) = listener.accept().await?;
    read_request(&mut socket).await;
    socket.write_all(headers.as_bytes()).await?;
    ready.send(()).unwrap();
    socket.read(&mut [0; 1]).await
}

async fn read_request(socket: &mut TcpStream) {
    let mut received = Vec::new();
    let mut byte = [0; 1];
    while !received.ends_with(b"\r\n\r\n") {
        socket.read_exact(&mut byte).await.unwrap();
        received.push(byte[0]);
    }
    let length = request_length(&received);
    socket.read_exact(&mut vec![0; length]).await.unwrap();
}

fn request_length(headers: &[u8]) -> usize {
    String::from_utf8_lossy(headers)
        .lines()
        .find_map(|line| {
            line.to_ascii_lowercase()
                .strip_prefix("content-length:")
                .map(str::to_owned)
        })
        .expect("request has a content length")
        .trim()
        .parse()
        .unwrap()
}

fn model(base_url: &str) -> std::sync::Arc<dyn myco::generative_model::GenerativeModel> {
    myco::generative_model::new(GenerativeModelConfig {
        model: ModelSpec {
            key: "test".into(),
            api_id: "test".into(),
            protocol: Protocol::OpenAICompletions,
            thinking: ThinkingMode::None,
            context_window_tokens: 4096,
            max_image_base64_bytes: 1024,
            max_truncated_resumes: 0,
            auto_compact_at_tokens: None,
        },
        tools: vec![],
        system_prompt: String::new(),
        backend_config: BackendConfig::OpenAICompletions(OpenAIBackendConfig {
            base_url: base_url.into(),
            ..Default::default()
        }),
    })
    .unwrap()
}
