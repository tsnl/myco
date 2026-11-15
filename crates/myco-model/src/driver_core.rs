//! Protocol-independent scaffolding shared by the streaming drivers: HTTP
//! client construction, the one `generate` path (image and request-size
//! preflight, spawned task, HTTP status classification, stream bridge), the
//! SSE drive loop with end-of-stream validation, image-source parsing, and
//! stream-index remapping.

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

/// Decodes one provider's SSE `data:` payloads into [`MessagePart`]s. Message
/// validity (a stop reason arrived, tool arguments parse) is checked once, in
/// the drive loop, on the parts the decoder emits.
pub(super) trait SseAccumulator: Send + 'static {
    /// Parse one `data:` payload and return the parts it yields.
    fn handle_data(&mut self, data: &str) -> Result<Vec<MessagePart>, GenerationFailure>;
    /// True once the provider signalled end of message; the drive loop stops reading.
    fn finished(&self) -> bool;
}

/// `Authorization: Bearer` for a non-empty token; an empty one sends no auth.
pub(super) fn bearer(token: &str) -> Option<(&'static str, String)> {
    (!token.is_empty()).then(|| ("authorization", format!("Bearer {token}")))
}

/// Every driver's [`GenerativeModel::generate`]: check image sizes, build the
/// provider request from `input`, then send it in a spawned task and bridge
/// its SSE stream into the generate stream shape. The receiver owns the
/// request lifetime, including time spent awaiting headers or body bytes. It
/// never retries.
pub(super) fn generate<A: SseAccumulator + Default>(
    provider: &'static str,
    input: &[Message],
    max_image_base64_bytes: u64,
    request: impl FnOnce(&[Message]) -> Result<reqwest::RequestBuilder, GenerateError>,
    debug_dump_api_requests: bool,
    max_request_bytes: usize,
) -> AsyncStream<GenerationEvent> {
    let request =
        match validate_image_sizes(input, max_image_base64_bytes).and_then(|()| request(input)) {
            Ok(request) => request,
            Err(e) => {
                return Box::pin(futures::stream::once(async move {
                    GenerationEvent::Failure(GenerationFailure::terminal(e))
                }));
            }
        };
    let acc = A::default();
    let (tx, rx) = tokio::sync::mpsc::channel::<GenerationEvent>(32);

    tokio::spawn(async move {
        let generate = async {
            let (client, request) = prepare_request(
                request,
                provider,
                debug_dump_api_requests,
                max_request_bytes,
            )
            .map_err(GenerationFailure::terminal)?;
            let response = attempt_send(&client, request, provider).await?;
            drive_sse_stream(response, &tx, acc, provider).await
        };
        tokio::select! {
            _ = tx.closed() => {}
            result = generate => {
                if let Err(e) = result {
                    let _ = tx.send(GenerationEvent::Failure(e)).await;
                }
            }
        }
    });

    Box::pin(futures::stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|item| (item, rx))
    }))
}

/// Check inline and legacy base64 images before constructing a provider body.
/// Remote URLs carry no known payload size; the provider validates their
/// content. Local references must be checked while resolving and never sent
/// to a provider.
fn validate_image_sizes(input: &[Message], limit: u64) -> Result<(), GenerateError> {
    for part in input.iter().flat_map(Message::content) {
        let Content::Image { source } = part else {
            continue;
        };
        if source.starts_with("myco-image:") {
            return Err(GenerateError::ExecutionError(
                "resolve local image references before calling a provider".into(),
            ));
        }
        match ImageSource::parse(source) {
            ImageSource::Url(_) => {}
            ImageSource::DataUrl { data, .. } | ImageSource::RawBase64(data) => {
                check_image_size(data.len() as u64, limit)?;
            }
        }
    }
    Ok(())
}

/// How every driver reads the opaque [`Content::Image`] `source` string.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum ImageSource<'a> {
    /// An http(s) URL, sent by reference.
    Url(&'a str),
    /// `data:[<media type>][;base64],<data>`; the media type defaults to PNG.
    DataUrl { media_type: &'a str, data: &'a str },
    /// Anything else: raw base64, taken to be PNG.
    RawBase64(&'a str),
}

impl<'a> ImageSource<'a> {
    pub(super) fn parse(source: &'a str) -> Self {
        if source.starts_with("http://") || source.starts_with("https://") {
            return Self::Url(source);
        }
        if let Some((meta, data)) = source
            .strip_prefix("data:")
            .and_then(|rest| rest.split_once(','))
        {
            let media_type = meta
                .split(';')
                .next()
                .filter(|s| !s.is_empty())
                .unwrap_or("image/png");
            return Self::DataUrl { media_type, data };
        }
        Self::RawBase64(source)
    }
}

fn prepare_request(
    builder: reqwest::RequestBuilder,
    provider: &str,
    debug: bool,
    max_request_bytes: usize,
) -> Result<(reqwest::Client, reqwest::Request), GenerateError> {
    let (client, request) = builder.build_split();
    let request = request.map_err(|e| GenerateError::ExecutionError(format!("{e:?}")))?;
    if let Some(body) = request.body().and_then(|body| body.as_bytes()) {
        check_request_size(body.len(), max_request_bytes, provider)?;
        if debug {
            eprintln!("{}", String::from_utf8_lossy(body));
        }
    }
    Ok((client, request))
}

/// One send. A non-success status is mapped to an error carrying the response
/// body (providers put the actionable detail in a JSON body, not the status
/// line); a size rejection keeps its own variant so the caller rewinds rather
/// than resending a request that can only fail again.
async fn attempt_send(
    client: &reqwest::Client,
    request: reqwest::Request,
    provider: &str,
) -> Result<reqwest::Response, GenerationFailure> {
    let response = match client.execute(request).await {
        Ok(response) => response,
        // Transport-level: DNS, connect, TLS, idle timeout. Nothing about the
        // request is known to be at fault, so another attempt is worthwhile.
        Err(e) => {
            return Err(GenerationFailure::transient(
                GenerateError::ExecutionError(format!("{provider} request failed: {e:?}")),
                None,
            ));
        }
    };
    if response.status().is_success() {
        return Ok(response);
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
        Err(GenerationFailure::transient(error, retry_after))
    } else {
        Err(GenerationFailure::terminal(error))
    }
}

/// Refuse a composed request body over the configured endpoint limit.
///
/// Takes the already-serialized length so the checked size is exactly the size
/// uploaded — no second serialization pass over a multi-megabyte body.
fn check_request_size(len: usize, limit: usize, provider: &str) -> Result<(), GenerateError> {
    if len > limit {
        return Err(GenerateError::RequestTooLargeError(format!(
            "the {provider} request is {len} bytes; the configured limit is {limit} bytes \
             (max_request_bytes). Images accumulate in the conversation; reduce \
             attachments or compact the session before retrying. Raise the gateway's \
             max_request_bytes only if it supports larger requests",
        )));
    }
    Ok(())
}

/// Map a provider HTTP status to the right error variant: a size rejection is
/// not a generic failure.
///
/// 413 is unambiguous. Everything else has to be read out of the body, because
/// providers disagree on how they report an oversized request: some name a
/// machine-readable type (`request_too_large`), others return a 400 whose body
/// only describes the size in prose. Getting this wrong is expensive — the
/// identical body is resent on every later turn, so a size failure classified
/// as [`Recovery::Retry`] never triggers the rewind and the session fails
/// forever.
fn http_error(status: reqwest::StatusCode, message: String) -> GenerateError {
    if status == reqwest::StatusCode::PAYLOAD_TOO_LARGE || describes_a_size_rejection(&message) {
        GenerateError::RequestTooLargeError(message)
    } else {
        GenerateError::ExecutionError(message)
    }
}

/// Does this provider error body say the request was too big?
///
/// Matching prose is unavoidable, so it is kept to phrasings only a size
/// rejection produces: "too large" however the provider spells it, or a size
/// that "exceeds" a stated limit.
fn describes_a_size_rejection(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    message.contains("too_large")
        || message.contains("too large")
        || (message.contains("size") && message.contains("exceeds"))
        || (message.contains("image dimensions")
            && message.contains("exceed")
            && message.contains("max allowed size"))
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

/// Forward decoded parts while checking them with a [`MessageAccumulator`],
/// so a stream that would assemble into an invalid message fails here, as a
/// failure carrying the 200 response's `Retry-After`, before the invalid part
/// reaches the caller.
async fn drive_sse_stream<A: SseAccumulator>(
    response: reqwest::Response,
    tx: &tokio::sync::mpsc::Sender<GenerationEvent>,
    mut acc: A,
    provider: &str,
) -> Result<(), GenerationFailure> {
    let retry_after = parse_retry_after(response.headers());
    // The agent also retries malformed drafts. Keep the provider's minimum
    // for any caller that elects to retry this failure.
    let malformed = |cause| GenerationFailure {
        cause,
        retryable: false,
        retry_after,
    };
    let mut byte_stream = response.bytes_stream();
    let mut sse = SseParser::default();
    let mut message = MessageAccumulator::default();
    message
        .push(&MessagePart::MessageStart)
        .map_err(malformed)?;
    let mut started = false;

    while let Some(chunk) = byte_stream.next().await {
        let chunk = chunk.map_err(|e| {
            GenerationFailure::transient(
                GenerateError::ExecutionError(format!(
                    "Error reading {provider} stream body: {e:?}"
                )),
                retry_after,
            )
        })?;

        for data in sse.push(&chunk) {
            let mut parts = acc.handle_data(&data).map_err(|mut failure| {
                failure.retry_after = failure.retry_after.max(retry_after);
                failure
            })?;
            for part in &parts {
                message.push(part).map_err(malformed)?;
            }
            // HTTP headers and SSE keepalives are not model output. Delay the
            // synthetic start until there is a response part to consume.
            if !started && !parts.is_empty() {
                parts.insert(0, MessagePart::MessageStart);
                started = true;
            }
            for item in parts {
                if tx.send(GenerationEvent::Part(item)).await.is_err() {
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

    message.finish().map(drop).map_err(malformed)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_sources_parse_as_urls_data_urls_or_raw_png() {
        for (source, want) in [
            (
                "https://x.test/a.png",
                ImageSource::Url("https://x.test/a.png"),
            ),
            (
                "http://x.test/a.png",
                ImageSource::Url("http://x.test/a.png"),
            ),
            (
                "data:image/jpeg;base64,AA",
                ImageSource::DataUrl {
                    media_type: "image/jpeg",
                    data: "AA",
                },
            ),
            (
                "data:;base64,AA",
                ImageSource::DataUrl {
                    media_type: "image/png",
                    data: "AA",
                },
            ),
            ("data:AA", ImageSource::RawBase64("data:AA")),
            ("iVBOR", ImageSource::RawBase64("iVBOR")),
        ] {
            assert_eq!(ImageSource::parse(source), want, "{source}");
        }
    }

    #[test]
    fn image_checks_size_inline_payloads_and_refuse_local_references() {
        let input = |source: &str| {
            [Message::UserMessage {
                content: vec![Content::Image {
                    source: source.into(),
                }],
            }]
        };
        assert!(validate_image_sizes(&input("data:image/png;base64,AAAA"), 4).is_ok());
        assert!(validate_image_sizes(&input("AAAA"), 4).is_ok());
        assert!(validate_image_sizes(&input("https://x.test/a.png"), 4).is_ok());
        for source in ["data:image/png;base64,AAAAA", "AAAAA"] {
            assert!(matches!(
                validate_image_sizes(&input(source), 4),
                Err(GenerateError::RequestTooLargeError(_))
            ));
        }
        assert!(matches!(
            validate_image_sizes(&input("myco-image:sha256:abc"), 1024),
            Err(GenerateError::ExecutionError(_))
        ));
    }

    /// The preflight is the whole point of the rewind path: a request that is
    /// too big must be refused *before* upload, and must say so in a way the
    /// top level can act on.
    #[test]
    fn oversized_request_is_refused_before_upload() {
        for limit in [1024, MAX_REQUEST_BYTES, 60_000_000] {
            assert!(check_request_size(limit, limit, "Anthropic").is_ok());
            let err = check_request_size(limit + 1, limit, "Anthropic").unwrap_err();
            assert!(
                matches!(err, GenerateError::RequestTooLargeError(_)),
                "{err:?}"
            );
            assert_eq!(err.recovery(), Recovery::OmitLastMessage);
            assert!(
                err.to_string().contains(&format!("limit is {limit} bytes")),
                "{err}"
            );
        }
    }

    /// A provider that rejects the size itself lands on the same variant, so
    /// the caller rewinds whether the cap was caught locally or remotely.
    #[test]
    fn provider_size_rejections_map_to_the_same_recovery() {
        let too_large = http_error(
            reqwest::StatusCode::PAYLOAD_TOO_LARGE,
            "HTTP 413: too big".into(),
        );
        assert_eq!(too_large.recovery(), Recovery::OmitLastMessage);

        // Anthropic reports it as a 400 whose body names the error type.
        let named = http_error(
            reqwest::StatusCode::BAD_REQUEST,
            r#"HTTP 400: {"error":{"type":"request_too_large"}}"#.into(),
        );
        assert_eq!(named.recovery(), Recovery::OmitLastMessage);

        // Others name no type and only describe the size in prose. Read as a
        // generic failure this is resent unchanged on every later turn.
        let described = http_error(
            reqwest::StatusCode::BAD_REQUEST,
            r#"HTTP 400: {"error":{"code":400,"message":"The message size (31271377 bytes) \
               exceeds 30.000MB limit.","status":"FAILED_PRECONDITION"}}"#
                .into(),
        );
        assert_eq!(described.recovery(), Recovery::OmitLastMessage);

        let extent = http_error(
            reqwest::StatusCode::BAD_REQUEST,
            r#"{"type":"error","error":{"type":"invalid_request_error","message":"messages.9.content.13.image.source.base64.data: At least one of the image dimensions exceed max allowed size for many-image requests: 2576 pixels"}}"#.into(),
        );
        assert_eq!(extent.recovery(), Recovery::OmitLastMessage);

        let unrelated = http_error(
            reqwest::StatusCode::INTERNAL_SERVER_ERROR,
            "HTTP 500: overloaded".into(),
        );
        assert_eq!(unrelated.recovery(), Recovery::Retry);
    }

    /// Reading the body is how a size rejection is recognized, so an ordinary
    /// failure must not be mistaken for one: rewinding drops a user message,
    /// which is destructive when the request was never too big.
    #[test]
    fn ordinary_failures_are_not_read_as_size_rejections() {
        for body in [
            r#"HTTP 400: {"error":{"message":"max_tokens exceeds the model's limit"}}"#,
            r#"HTTP 400: {"error":{"message":"messages: unexpected role"}}"#,
            r#"HTTP 401: {"error":{"message":"invalid x-api-key"}}"#,
        ] {
            let err = http_error(reqwest::StatusCode::BAD_REQUEST, body.into());
            assert_eq!(err.recovery(), Recovery::Retry, "{body}");
        }
    }
}
