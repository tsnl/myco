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

/// Send `request` in a spawned task and bridge its SSE stream into the
/// [`GenerativeModel::generate`] stream shape. Dropping the returned stream
/// cancels generation: the task's channel sends fail, it returns, and the HTTP
/// body drops so the provider stops generating/billing.
pub(super) fn spawn_generate<A: SseAccumulator>(
    request: reqwest::RequestBuilder,
    acc: A,
    provider: &'static str,
) -> AsyncStream<Result<MessagePart, GenerateError>> {
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<MessagePart, GenerateError>>(32);

    tokio::spawn(async move {
        let result = async {
            let response = request
                .send()
                .await
                .map_err(|e| GenerateError::ExecutionError(format!("{e:?}")))?;
            let response = check_status(response, provider).await?;
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

/// Map a non-success HTTP status to an error carrying the response body
/// (providers put the actionable detail in a JSON body, not the status line).
async fn check_status(
    response: reqwest::Response,
    provider: &str,
) -> Result<reqwest::Response, GenerateError> {
    if response.status().is_success() {
        return Ok(response);
    }
    let status = response.status();
    let body = response
        .text()
        .await
        .unwrap_or_else(|e| format!("<failed to read body: {e:?}>"));
    Err(GenerateError::ExecutionError(format!(
        "{provider} API returned HTTP {status}: {body}"
    )))
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
