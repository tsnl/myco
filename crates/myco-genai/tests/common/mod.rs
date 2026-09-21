use std::time::Duration;

use myco_genai::{Client, Config, Error, Event, Message, Protocol, Request, Response};
use serde_json::Value;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::oneshot,
};

pub const OPENAI: &str = include_str!("../fixtures/openai.sse");
pub const ANTHROPIC: &str = include_str!("../fixtures/anthropic.sse");

pub struct Captured {
    pub headers: String,
    pub body: Value,
}

pub async fn read_request(socket: &mut TcpStream) -> Captured {
    let mut bytes = vec![];
    let (head_end, length) = loop {
        let mut buffer = [0; 4096];
        let count = socket.read(&mut buffer).await.unwrap();
        assert!(count > 0);
        bytes.extend_from_slice(&buffer[..count]);
        if let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = std::str::from_utf8(&bytes[..end]).unwrap();
            let length = head
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().unwrap())
                })
                .unwrap();
            break (end + 4, length);
        }
    };
    while bytes.len() < head_end + length {
        let mut buffer = [0; 4096];
        let count = socket.read(&mut buffer).await.unwrap();
        assert!(count > 0);
        bytes.extend_from_slice(&buffer[..count]);
    }
    Captured {
        headers: String::from_utf8(bytes[..head_end].into()).unwrap(),
        body: serde_json::from_slice(&bytes[head_end..head_end + length]).unwrap(),
    }
}

pub async fn fixture(
    body: &str,
    status: u16,
    extra_headers: &str,
) -> (String, oneshot::Receiver<Captured>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/inference", listener.local_addr().unwrap());
    let body = body.as_bytes().to_vec();
    let headers = format!(
        "HTTP/1.1 {status} Fixture\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n{extra_headers}\r\n"
    );
    let (send, receive) = oneshot::channel();
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        send.send(read_request(&mut socket).await).ok();
        if socket.write_all(headers.as_bytes()).await.is_err() {
            return;
        }
        // Deliberately split both UTF-8 and SSE records over HTTP chunks.
        for chunk in body.chunks(7) {
            let header = format!("{:x}\r\n", chunk.len());
            if socket.write_all(header.as_bytes()).await.is_err()
                || socket.write_all(chunk).await.is_err()
                || socket.write_all(b"\r\n").await.is_err()
            {
                return;
            }
        }
        socket.write_all(b"0\r\n\r\n").await.ok();
    });
    (url, receive)
}

pub fn request() -> Request {
    Request::new(
        "test-model",
        vec![Message::User("Read the note".into())],
        64,
    )
}

pub fn client(protocol: Protocol, endpoint: &str, key: &str) -> Result<Client, Error> {
    let endpoint = endpoint.into();
    let api_key = key.into();
    Client::new(match protocol {
        Protocol::OpenAiResponses => Config::OpenAi { endpoint, api_key },
        Protocol::AnthropicMessages => Config::Anthropic { endpoint, api_key },
    })
}

#[derive(Debug)]
pub struct Trace {
    pub events: Vec<Event>,
    pub result: Result<Response, Error>,
}

pub async fn collect(client: &Client, request: Request) -> Trace {
    let mut events = Vec::new();
    let generation = client.generate(request, |event| {
        events.push(event);
        std::future::ready(Ok(()))
    });
    let result = tokio::time::timeout(Duration::from_secs(5), generation)
        .await
        .unwrap();
    Trace { events, result }
}

pub async fn run(protocol: Protocol, body: &str) -> Trace {
    let (url, _capture) = fixture(body, 200, "").await;
    let client = client(protocol, &url, "test-key").unwrap();
    collect(&client, request()).await
}

pub fn completed(trace: &Trace) -> &Response {
    trace.result.as_ref().unwrap()
}

pub fn events(values: &[Value]) -> String {
    values.iter().map(|v| format!("data: {v}\n\n")).collect()
}

pub fn text_response(text: &str) -> Value {
    serde_json::json!({"status":"completed", "output":[{"type":"message", "role":"assistant", "content":[{"type":"output_text", "text":text}]}]})
}
