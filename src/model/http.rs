use std::{collections::VecDeque, time::Duration};

use async_stream::try_stream;
use futures_core::Stream;
use futures_util::StreamExt;
use reqwest::{
    Client, Url,
    header::{HeaderMap, HeaderValue},
};
use serde_json::Value;

use crate::model::driver::{Decoded, EventStream};
use crate::model::{Error, Event, Protocol, Response, sse::Sse};

pub(crate) struct Transport {
    client: Client,
    endpoint: Url,
}

impl Transport {
    pub fn new(protocol: Protocol, endpoint: &str, api_key: &str) -> Result<Self, Error> {
        let endpoint = endpoint_url(endpoint)?;
        let client = Client::builder()
            .default_headers(headers(protocol, api_key)?)
            .connect_timeout(Duration::from_secs(30))
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .build()?;
        Ok(Self { client, endpoint })
    }

    pub fn generate<'a>(
        &'a self,
        protocol: Protocol,
        body: Value,
        decode: impl FnMut(&Value) -> Result<Decoded, Error> + Send + 'a,
    ) -> EventStream<'a> {
        Box::pin(try_stream! {
            let request = self.request(&body)?;
            yield Event::Request { protocol, body };
            let response = self.send(request).await?;
            let mut events = std::pin::pin!(response_events(response, protocol, decode));
            while let Some(event) = events.next().await {
                yield event?;
            }
        })
    }

    fn request(&self, body: &Value) -> Result<reqwest::Request, Error> {
        Ok(self.client.post(self.endpoint.clone()).json(body).build()?)
    }

    async fn send(&self, request: reqwest::Request) -> Result<reqwest::Response, Error> {
        let response = self.client.execute(request).await?;
        if !response.status().is_success() {
            return Err(http_error(response).await?);
        }
        validate_content_type(response.headers())?;
        Ok(response)
    }
}

fn endpoint_url(endpoint: &str) -> Result<Url, Error> {
    let url = Url::parse(endpoint)
        .map_err(|e| Error::InvalidRequest(format!("invalid endpoint: {e}")))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(Error::InvalidRequest(
            "endpoint must use HTTP or HTTPS".into(),
        ));
    }
    Ok(url)
}

fn headers(protocol: Protocol, api_key: &str) -> Result<HeaderMap, Error> {
    let mut headers = HeaderMap::new();
    headers.insert("accept", HeaderValue::from_static("text/event-stream"));
    if !api_key.is_empty() {
        authenticate(&mut headers, protocol, api_key)?;
    }
    if protocol == Protocol::AnthropicMessages {
        headers.insert("anthropic-version", HeaderValue::from_static("2023-06-01"));
    }
    Ok(headers)
}

fn authenticate(headers: &mut HeaderMap, protocol: Protocol, key: &str) -> Result<(), Error> {
    let (name, value) = match protocol {
        Protocol::OpenAiResponses => ("authorization", format!("Bearer {key}")),
        Protocol::AnthropicMessages => ("x-api-key", key.into()),
    };
    let mut value = HeaderValue::from_str(&value)
        .map_err(|_| Error::InvalidRequest("API key is not a valid HTTP header value".into()))?;
    value.set_sensitive(true);
    headers.insert(name, value);
    Ok(())
}

fn header(headers: &HeaderMap, name: &str) -> Option<String> {
    headers.get(name)?.to_str().ok().map(str::to_owned)
}

async fn http_error(response: reqwest::Response) -> Result<Error, Error> {
    Ok(Error::Http {
        status: response.status().as_u16(),
        request_id: header(response.headers(), "x-request-id")
            .or_else(|| header(response.headers(), "request-id")),
        retry_after: header(response.headers(), "retry-after"),
        body: response.text().await?,
    })
}

fn validate_content_type(headers: &HeaderMap) -> Result<(), Error> {
    let valid = header(headers, "content-type").is_some_and(|value| {
        value
            .split(';')
            .next()
            .is_some_and(|v| v.trim().eq_ignore_ascii_case("text/event-stream"))
    });
    if !valid {
        return Err(Error::Protocol(
            "expected text/event-stream content type".into(),
        ));
    }
    Ok(())
}

fn response_events(
    response: reqwest::Response,
    protocol: Protocol,
    mut decode: impl FnMut(&Value) -> Result<Decoded, Error> + Send,
) -> impl Stream<Item = Result<Event, Error>> + Send {
    try_stream! {
        let mut events = Events::new(response);
        while let Some(raw) = events.next().await? {
            let decoded = decode(&raw);
            let delta = decoded.as_ref().ok().and_then(Decoded::delta);
            // Expose valid JSON before any decoding or normalization failure.
            yield Event::Progress { raw, delta };
            if let Decoded::Completed(body) = decoded? {
                yield Event::Completed(Response::from_provider(protocol, body)?);
                return;
            }
        }
        Err(Error::Protocol("stream ended before its terminal event".into()))?;
    }
}

struct Events {
    response: reqwest::Response,
    sse: Sse,
    frames: VecDeque<String>,
}

impl Events {
    fn new(response: reqwest::Response) -> Self {
        Self {
            response,
            sse: Sse::default(),
            frames: VecDeque::new(),
        }
    }

    async fn next(&mut self) -> Result<Option<Value>, Error> {
        loop {
            if let Some(frame) = self.frames.pop_front() {
                if frame.is_empty() {
                    continue;
                }
                return decode_frame(&frame).map(Some);
            }
            if !self.read_chunk().await? && self.frames.is_empty() {
                return Ok(None);
            }
        }
    }

    async fn read_chunk(&mut self) -> Result<bool, Error> {
        let chunk = self.response.chunk().await?;
        let eof = chunk.is_none();
        self.frames
            .extend(self.sse.push(chunk.as_deref().unwrap_or_default(), eof)?);
        Ok(!eof)
    }
}

fn decode_frame(frame: &str) -> Result<Value, Error> {
    serde_json::from_str(frame).map_err(|e| Error::Protocol(format!("invalid SSE JSON: {e}")))
}
