//! One-shot HTTP/1.1 server for provider-driver wire tests.
//!
//! Serves a canned response to exactly one request and hands back the request
//! it received. This covers what accumulator unit tests cannot reach: the URL
//! a driver posts to, the JSON it puts on the wire, and SSE reassembly through
//! a real client and socket. It is not a provider simulator — a test supplies
//! whatever bytes it wants to assert against.

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;

pub struct StubHttpServer {
    base_url: String,
    request: oneshot::Receiver<CapturedRequest>,
}

/// What the driver sent.
#[derive(Debug)]
pub struct CapturedRequest {
    pub path: String,
    pub body: serde_json::Value,
}

impl StubHttpServer {
    /// Serve `events` as an SSE stream (one `data:` payload each, then
    /// `[DONE]`). Every event is written in two pieces so the driver's parser
    /// has to reassemble a payload split across reads.
    pub async fn sse(events: Vec<serde_json::Value>) -> Self {
        let mut body =
            b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n"
                .to_vec();
        let mut writes = vec![std::mem::take(&mut body)];
        for event in events {
            let payload = format!("data: {event}\n\n").into_bytes();
            let mid = payload.len() / 2;
            writes.push(payload[..mid].to_vec());
            writes.push(payload[mid..].to_vec());
        }
        writes.push(b"data: [DONE]\n\n".to_vec());
        Self::spawn(writes).await
    }

    /// Serve an error status with `body` (unhappy-path tests).
    pub async fn status(status: u16, body: &str) -> Self {
        let response = format!(
            "HTTP/1.1 {status} Error\r\nContent-Type: application/json\r\n\
             Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        Self::spawn(vec![response.into_bytes()]).await
    }

    /// Base URL to configure a backend with (no trailing slash).
    pub fn base_url(&self) -> String {
        self.base_url.clone()
    }

    /// The request the driver sent. Panics if none arrived.
    pub async fn captured(self) -> CapturedRequest {
        self.request.await.expect("stub server received a request")
    }

    async fn spawn(writes: Vec<Vec<u8>>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let (tx, request) = oneshot::channel();

        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept");
            let captured = read_request(&mut stream).await;
            let _ = tx.send(captured);
            for write in writes {
                stream.write_all(&write).await.expect("write response");
                stream.flush().await.expect("flush");
            }
            let _ = stream.shutdown().await;
        });

        Self {
            base_url: format!("http://127.0.0.1:{port}"),
            request,
        }
    }
}

async fn read_request(stream: &mut TcpStream) -> CapturedRequest {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];

    let head_end = loop {
        let n = stream.read(&mut chunk).await.expect("read request");
        assert!(n > 0, "connection closed before request headers");
        buf.extend_from_slice(&chunk[..n]);
        if let Some(at) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break at + 4;
        }
    };

    let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
    let path = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .expect("request line")
        .to_string();
    let mut content_length = 0usize;
    for line in head.lines() {
        let lower = line.to_ascii_lowercase();
        if let Some(value) = lower.strip_prefix("content-length:") {
            content_length = value.trim().parse().expect("content-length");
        }
    }

    while buf.len() < head_end + content_length {
        let n = stream.read(&mut chunk).await.expect("read body");
        assert!(n > 0, "connection closed mid-body");
        buf.extend_from_slice(&chunk[..n]);
    }

    let body = serde_json::from_slice(&buf[head_end..head_end + content_length])
        .expect("request body is JSON");
    CapturedRequest { path, body }
}
