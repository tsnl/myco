//! HTTP/1.1 server for provider-driver wire tests.
//!
//! Serves canned responses and hands back the request it received. This covers
//! what accumulator unit tests cannot reach: the URL a driver posts to, the
//! JSON it puts on the wire, and SSE reassembly through a real client and
//! socket. It is not a provider simulator — a test supplies whatever bytes it
//! wants to assert against.
//!
//! Most constructors serve exactly one request. [`StubHttpServer::sequence`]
//! serves several, one per connection, which is what retry tests need.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;

pub struct StubHttpServer {
    base_url: String,
    request: oneshot::Receiver<CapturedRequest>,
    connections: Arc<AtomicUsize>,
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

    /// An error status carrying a `Retry-After` header, for testing that the
    /// driver honours the provider's own pacing.
    pub async fn status_with_retry_after(status: u16, body: &str, retry_after_secs: u64) -> Self {
        let response = format!(
            "HTTP/1.1 {status} Error\r\nContent-Type: application/json\r\n\
             Retry-After: {retry_after_secs}\r\n\
             Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        Self::spawn_sequence(vec![response.into_bytes()]).await
    }

    /// Serve one canned response per connection, in order: connection 1 gets
    /// `responses[0]`, and so on. A driver that retries opens a fresh
    /// connection each attempt (`Connection: close`), so the response index is
    /// the attempt number. Extra connections beyond the list are refused by
    /// closing without a write, which surfaces as a transport error.
    ///
    /// [`Self::connections`] counts what actually arrived.
    pub async fn sequence(responses: Vec<Vec<u8>>) -> Self {
        Self::spawn_sequence(responses).await
    }

    /// An SSE success response, as the bytes [`Self::sequence`] takes.
    pub fn sse_response(events: Vec<serde_json::Value>) -> Vec<u8> {
        let mut out =
            b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n"
                .to_vec();
        for event in events {
            out.extend_from_slice(format!("data: {event}\n\n").as_bytes());
        }
        out.extend_from_slice(b"data: [DONE]\n\n");
        out
    }

    /// An error response, as the bytes [`Self::sequence`] takes.
    pub fn status_response(status: u16, body: &str) -> Vec<u8> {
        format!(
            "HTTP/1.1 {status} Error\r\nContent-Type: application/json\r\n\
             Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .into_bytes()
    }

    /// Base URL to configure a backend with (no trailing slash).
    pub fn base_url(&self) -> String {
        self.base_url.clone()
    }

    /// How many connections the driver has opened so far — the attempt count.
    pub fn connections(&self) -> usize {
        self.connections.load(Ordering::SeqCst)
    }

    /// The request the driver sent. Panics if none arrived.
    pub async fn captured(self) -> CapturedRequest {
        self.request.await.expect("stub server received a request")
    }

    async fn spawn(writes: Vec<Vec<u8>>) -> Self {
        // One connection, whose response is written in several pieces.
        Self::spawn_inner(vec![writes]).await
    }

    async fn spawn_sequence(responses: Vec<Vec<u8>>) -> Self {
        Self::spawn_inner(responses.into_iter().map(|r| vec![r]).collect()).await
    }

    /// `connections[i]` is the list of writes served to the i-th connection.
    async fn spawn_inner(connections: Vec<Vec<Vec<u8>>>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let (tx, request) = oneshot::channel();
        let counter = Arc::new(AtomicUsize::new(0));
        let served = counter.clone();

        tokio::spawn(async move {
            let mut tx = Some(tx);
            for writes in connections {
                let (mut stream, _) = listener.accept().await.expect("accept");
                served.fetch_add(1, Ordering::SeqCst);
                let captured = read_request(&mut stream).await;
                // Only the first request is reported; retries resend it verbatim.
                if let Some(tx) = tx.take() {
                    let _ = tx.send(captured);
                }
                for write in writes {
                    stream.write_all(&write).await.expect("write response");
                    stream.flush().await.expect("flush");
                }
                let _ = stream.shutdown().await;
            }
        });

        Self {
            base_url: format!("http://127.0.0.1:{port}"),
            request,
            connections: counter,
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
