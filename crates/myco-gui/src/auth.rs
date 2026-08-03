//! Bearer-token auth for the browser client.
//!
//! Every `/api` request carries `Authorization: Bearer <token>`; the server
//! resolves it to a roster user and attributes whatever the request writes to
//! them. The token lives in `localStorage` so a reload does not log you out,
//! and in a thread-local so the fetch helpers can reach it without threading
//! a context parameter through every component.
//!
//! `EventSource` cannot set headers, so the SSE URL carries `?token=`
//! instead — see [`sse_url`].

use std::cell::RefCell;

use gloo_net::http::{Request, RequestBuilder};
use myco_api as api;

const STORAGE_KEY: &str = "myco.token";

thread_local! {
    static TOKEN: RefCell<Option<String>> = RefCell::new(load_stored());
}

fn storage() -> Option<web_sys::Storage> {
    web_sys::window()?.local_storage().ok().flatten()
}

fn load_stored() -> Option<String> {
    storage()?
        .get_item(STORAGE_KEY)
        .ok()
        .flatten()
        .filter(|t| !t.is_empty())
}

pub fn token() -> Option<String> {
    TOKEN.with(|t| t.borrow().clone())
}

pub fn set_token(value: &str) {
    let value = value.trim().to_string();
    if let Some(s) = storage() {
        let _ = s.set_item(STORAGE_KEY, &value);
    }
    TOKEN.with(|t| *t.borrow_mut() = Some(value));
}

pub fn clear_token() {
    if let Some(s) = storage() {
        let _ = s.remove_item(STORAGE_KEY);
    }
    TOKEN.with(|t| *t.borrow_mut() = None);
}

/// What went wrong with a request. [`Failure::Unauthorized`] is separate
/// because it is the one error the UI reacts to structurally: the token is
/// bad, so the only useful next step is to ask for another one.
#[derive(Debug, Clone, PartialEq)]
pub enum Failure {
    Unauthorized,
    Other(String),
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Failure::Unauthorized => write!(f, "not signed in"),
            Failure::Other(m) => write!(f, "{m}"),
        }
    }
}

/// The SSE endpoint for `id`, with the token in the query string.
pub fn sse_url(id: &str) -> String {
    let token = token().unwrap_or_default();
    format!("/api/sessions/{id}/events?token={}", encode(&token))
}

/// Percent-encode a query-string value. Tokens are hex in practice, but a
/// hand-written one may not be, and a stray `&` would silently truncate it.
fn encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// A builder for `path` with the bearer header already attached.
fn authorized(rb: RequestBuilder) -> RequestBuilder {
    match token() {
        Some(t) => rb.header("Authorization", &format!("Bearer {t}")),
        None => rb,
    }
}

async fn decode<T: serde::de::DeserializeOwned>(
    what: &str,
    sent: Result<gloo_net::http::Response, gloo_net::Error>,
) -> Result<T, Failure> {
    let resp = sent.map_err(|e| Failure::Other(format!("{what}: {e}")))?;
    if resp.status() == 401 {
        clear_token();
        return Err(Failure::Unauthorized);
    }
    if !resp.ok() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        let detail = serde_json::from_str::<api::ApiError>(&body)
            .map(|e| e.error)
            .unwrap_or(body);
        return Err(Failure::Other(format!("{what}: http {status}: {detail}")));
    }
    resp.json::<T>()
        .await
        .map_err(|e| Failure::Other(format!("{what}: decode: {e}")))
}

async fn run<T: serde::de::DeserializeOwned>(
    what: String,
    rb: RequestBuilder,
) -> Result<T, Failure> {
    let sent = authorized(rb).send().await;
    decode(&what, sent).await
}

async fn run_json<T: serde::de::DeserializeOwned, B: serde::Serialize>(
    what: String,
    rb: RequestBuilder,
    body: &B,
) -> Result<T, Failure> {
    let req = authorized(rb)
        .json(body)
        .map_err(|e| Failure::Other(format!("{what}: encode: {e}")))?;
    let sent = req.send().await;
    decode(&what, sent).await
}

pub async fn get<T: serde::de::DeserializeOwned>(path: &str) -> Result<T, Failure> {
    run(format!("GET {path}"), Request::get(path)).await
}

pub async fn post<T: serde::de::DeserializeOwned>(path: &str) -> Result<T, Failure> {
    run(format!("POST {path}"), Request::post(path)).await
}

pub async fn post_json<T: serde::de::DeserializeOwned, B: serde::Serialize>(
    path: &str,
    body: &B,
) -> Result<T, Failure> {
    run_json(format!("POST {path}"), Request::post(path), body).await
}

pub async fn patch_json<T: serde::de::DeserializeOwned, B: serde::Serialize>(
    path: &str,
    body: &B,
) -> Result<T, Failure> {
    run_json(format!("PATCH {path}"), Request::patch(path), body).await
}

/// Verify the stored token and learn who it belongs to.
pub async fn whoami() -> Result<api::Identity, Failure> {
    if token().is_none() {
        return Err(Failure::Unauthorized);
    }
    get("/api/whoami").await
}
