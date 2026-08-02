//! Protocol-independent scaffolding shared by the streaming drivers: HTTP
//! client construction, the spawned generate task (channel + request + stream
//! bridge), the SSE drive loop, and stream-index remapping.

use crate::core::*;

use super::*;

/// Build a driver's HTTP client: JSON content type, provider `extra_headers`,
/// and the pre-picked `auth` header. `None` auth = `auth = "none"` in the
/// catalog (local proxies); credential *presence* is the catalog's job
/// (`ModelCatalog::get`), not the driver's.
pub(super) fn build_client(
    auth: Option<(&'static str, String)>,
    extra_headers: &[(&'static str, &'static str)],
) -> Result<reqwest::Client, ModelCreationError> {
    let mut headers = reqwest::header::HeaderMap::from_iter([(
        reqwest::header::CONTENT_TYPE,
        "application/json".parse().unwrap(),
    )]);
    for (name, value) in extra_headers {
        headers.insert(
            reqwest::header::HeaderName::from_static(name),
            value.parse().unwrap(),
        );
    }
    if let Some((name, value)) = auth {
        headers.insert(
            reqwest::header::HeaderName::from_static(name),
            // Never echo the token into the error: it ends up in logs.
            value.parse().map_err(|e| {
                ModelCreationError::BadConfig(format!(
                    "auth token is not a valid HTTP header value: {e}"
                ))
            })?,
        );
    }
    reqwest::ClientBuilder::new()
        .default_headers(headers)
        .build()
        .map_err(|e| ModelCreationError::Uncategorized(format!("{e:?}")))
}

/// Accumulates one provider's SSE `data:` payloads into [`MessagePart`]s.
pub(super) trait SseAccumulator: Send + 'static {
    /// Parse one `data:` payload and return the parts it yields.
    fn handle_data(&mut self, data: &str) -> Result<Vec<MessagePart>, GenerateError>;
    /// True once the provider signalled end of message; the drive loop stops reading.
    fn finished(&self) -> bool;
    /// Validate that the stream completed properly (stop reason arrived, …).
    fn finish(self) -> Result<(), GenerateError>;
}

/// Shared end-of-stream validation behind every driver's
/// [`SseAccumulator::finish`]: a stop reason must have arrived, and each
/// accumulated tool-call argument string must parse as JSON (empty = `{}`).
pub(super) fn validate_finish<'a>(
    provider: &str,
    stop_reason_seen: bool,
    tool_args: impl IntoIterator<Item = (usize, &'a str)>,
) -> Result<(), GenerateError> {
    if !stop_reason_seen {
        return Err(GenerateError::MalformedResponseError(format!(
            "{provider} stream ended without a stop reason"
        )));
    }
    for (i, args) in tool_args {
        let json = if args.is_empty() { "{}" } else { args };
        if let Err(e) = serde_json::from_str::<serde_json::Value>(json) {
            return Err(GenerateError::MalformedResponseError(format!(
                "Malformed stream: {provider} tool call arguments at index {i} invalid: {e}"
            )));
        }
    }
    Ok(())
}

pub(super) fn error_stream(e: GenerateError) -> AsyncStream<Result<MessagePart, GenerateError>> {
    Box::pin(futures::stream::once(async move { Err(e) }))
}

/// Send `request` in a spawned task and bridge its SSE stream into the
/// [`GenerativeModel::generate`] stream shape. Dropping the returned stream
/// cancels generation: the task's channel sends fail, it returns, and the HTTP
/// body drops so the provider stops generating/billing.
pub(super) fn spawn_generate<A: SseAccumulator>(
    request: reqwest::RequestBuilder,
    acc: A,
    provider: &'static str,
    debug_dump_api_requests: bool,
    retry: RetryPolicy,
) -> AsyncStream<Result<MessagePart, GenerateError>> {
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<MessagePart, GenerateError>>(32);

    tokio::spawn(async move {
        let result = async {
            // Build first so the composed body can be measured: providers cap
            // the whole request, and images accumulate in history, so a session
            // can cross the cap turns after the attachment was sent. Rejecting
            // here beats a 413 after a multi-megabyte upload.
            let (client, request) = request.build_split();
            let request = request.map_err(|e| GenerateError::ExecutionError(format!("{e:?}")))?;
            if let Some(body) = request.body().and_then(|b| b.as_bytes()) {
                if debug_dump_api_requests {
                    // The exact serialized body, dumped before the size check
                    // so an over-limit request can still be inspected.
                    eprintln!("{}", String::from_utf8_lossy(body));
                }
                check_request_size(body.len(), provider)?;
            }
            let response = send_with_retry(
                &client,
                request,
                provider,
                retry,
                &tx,
                debug_dump_api_requests,
            )
            .await?;
            drive_sse_stream(response, &tx, acc, provider).await
        }
        .await;
        if let Err(e) = result {
            let _ = tx.send(Err(e)).await;
        }
    });

    Box::pin(futures::stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|item| (item, rx))
    }))
}

/// How one send attempt ended.
enum Attempt {
    Ok(reqwest::Response),
    /// Might succeed if sent again; carries the provider's `Retry-After` when
    /// it named one.
    Transient(GenerateError, Option<std::time::Duration>),
    /// Deterministic — the identical request fails the identical way.
    Fatal(GenerateError),
}

/// Send, retrying transient failures per `retry`.
///
/// Retry lives here, ahead of [`drive_sse_stream`], because this is the last
/// point at which nothing has reached the consumer yet. A failure *during* the
/// stream cannot be retried: parts already emitted would be replayed as
/// duplicates, so those still surface to the agent as a turn-ending error.
async fn send_with_retry(
    client: &reqwest::Client,
    request: reqwest::Request,
    provider: &str,
    retry: RetryPolicy,
    tx: &tokio::sync::mpsc::Sender<Result<MessagePart, GenerateError>>,
    debug: bool,
) -> Result<reqwest::Response, GenerateError> {
    let mut attempt: u32 = 1;
    loop {
        // A body reqwest cannot clone cannot be replayed either; send the
        // original and report whatever it gives.
        let Some(this_attempt) = request.try_clone() else {
            return match attempt_send(client, request, provider).await {
                Attempt::Ok(response) => Ok(response),
                Attempt::Transient(e, _) | Attempt::Fatal(e) => Err(e),
            };
        };

        let (error, retry_after) = match attempt_send(client, this_attempt, provider).await {
            Attempt::Ok(response) => return Ok(response),
            Attempt::Fatal(e) => return Err(e),
            Attempt::Transient(e, retry_after) => (e, retry_after),
        };

        if attempt >= retry.max_attempts {
            return Err(error);
        }
        let wait = retry.backoff(attempt + 1, retry_after);
        if debug {
            eprintln!(
                "{provider}: attempt {attempt}/{} failed ({error}); retrying in {wait:?}",
                retry.max_attempts
            );
        }
        // Ctrl-C during a long backoff drops the consumer; notice that instead
        // of sleeping the wait out before the turn can end.
        tokio::select! {
            _ = tokio::time::sleep(wait) => {}
            _ = tx.closed() => return Err(error),
        }
        attempt += 1;
    }
}

/// One send. A non-success status is mapped to an error carrying the response
/// body (providers put the actionable detail in a JSON body, not the status
/// line); a size rejection keeps its own variant so the caller rewinds rather
/// than resending a request that can only fail again.
async fn attempt_send(
    client: &reqwest::Client,
    request: reqwest::Request,
    provider: &str,
) -> Attempt {
    let response = match client.execute(request).await {
        Ok(response) => response,
        // Transport-level: DNS, connect, TLS, idle timeout. Nothing about the
        // request is known to be at fault, so another attempt is worthwhile.
        Err(e) => return Attempt::Transient(GenerateError::ExecutionError(format!("{e:?}")), None),
    };
    if response.status().is_success() {
        return Attempt::Ok(response);
    }
    let status = response.status();
    let retry_after = parse_retry_after(response.headers());
    let body = response
        .text()
        .await
        .unwrap_or_else(|e| format!("<failed to read body: {e:?}>"));
    let error = http_error(
        status,
        format!("{provider} API returned HTTP {status}: {body}"),
    );
    if is_transient_status(status) {
        Attempt::Transient(error, retry_after)
    } else {
        Attempt::Fatal(error)
    }
}

/// Statuses worth another attempt: the provider is rate-limiting, overloaded
/// (Anthropic's 529 lands in 5xx), or briefly broken. Everything else — 400
/// malformed, 401 auth, 413 too large — fails identically however often it is
/// sent, and retrying only delays the error the user needs to see.
fn is_transient_status(status: reqwest::StatusCode) -> bool {
    status.is_server_error()
        || status == reqwest::StatusCode::TOO_MANY_REQUESTS
        || status == reqwest::StatusCode::REQUEST_TIMEOUT
}

/// `Retry-After` in delta-seconds. The HTTP-date form is deliberately not
/// parsed: providers send seconds, and falling back to the computed backoff is
/// better than acting on a misread date.
fn parse_retry_after(headers: &reqwest::header::HeaderMap) -> Option<std::time::Duration> {
    let raw = headers.get(reqwest::header::RETRY_AFTER)?.to_str().ok()?;
    Some(std::time::Duration::from_secs(raw.trim().parse().ok()?))
}

async fn drive_sse_stream<A: SseAccumulator>(
    response: reqwest::Response,
    tx: &tokio::sync::mpsc::Sender<Result<MessagePart, GenerateError>>,
    mut acc: A,
    provider: &str,
) -> Result<(), GenerateError> {
    if tx.send(Ok(MessagePart::MessageStart)).await.is_err() {
        // Consumer dropped (turn cancelled): stop reading so the response
        // body drops and the provider stops generating/billing.
        return Ok(());
    }

    let mut byte_stream = response.bytes_stream();
    let mut sse = SseParser::default();

    while let Some(chunk) = byte_stream.next().await {
        let chunk = chunk.map_err(|e| {
            GenerateError::ExecutionError(format!("Error reading {provider} stream body: {e:?}"))
        })?;

        for data in sse.push(&chunk) {
            for item in acc.handle_data(&data)? {
                if tx.send(Ok(item)).await.is_err() {
                    return Ok(());
                }
            }

            if acc.finished() {
                break;
            }
        }

        if acc.finished() {
            break;
        }
    }

    acc.finish()
}

/// Maps a provider's unified stream indices (Anthropic content blocks, OpenAI
/// Responses output items) onto myco's separate content / tool-use index
/// spaces. Thinking shares the content index space.
#[derive(Default)]
pub(super) struct SlotMap {
    slots: Vec<Option<Slot>>,
}

/// What a provider stream slot turned out to be, with its remapped index.
#[derive(Clone, Copy)]
pub(super) enum Slot {
    Content { index: usize },
    Thinking { index: usize },
    ToolUse { index: usize },
    Ignored,
}

impl SlotMap {
    pub(super) fn get(&self, at: usize) -> Option<Slot> {
        self.slots.get(at).copied().flatten()
    }

    /// Open slot `at` as a text content block; returns its content index.
    pub(super) fn open_content(&mut self, at: usize) -> usize {
        let index = self.content_count();
        self.set(at, Slot::Content { index });
        index
    }

    /// Open slot `at` as a thinking block; returns its content index.
    pub(super) fn open_thinking(&mut self, at: usize) -> usize {
        let index = self.content_count();
        self.set(at, Slot::Thinking { index });
        index
    }

    /// Open slot `at` as a tool use; returns its tool-use index.
    pub(super) fn open_tool_use(&mut self, at: usize) -> usize {
        let index = self.tool_use_count();
        self.set(at, Slot::ToolUse { index });
        index
    }

    pub(super) fn ignore(&mut self, at: usize) {
        self.set(at, Slot::Ignored);
    }

    fn set(&mut self, at: usize, slot: Slot) {
        while self.slots.len() <= at {
            self.slots.push(None);
        }
        self.slots[at] = Some(slot);
    }

    fn content_count(&self) -> usize {
        self.slots
            .iter()
            .filter(|s| matches!(s, Some(Slot::Content { .. } | Slot::Thinking { .. })))
            .count()
    }

    fn tool_use_count(&self) -> usize {
        self.slots
            .iter()
            .filter(|s| matches!(s, Some(Slot::ToolUse { .. })))
            .count()
    }
}
