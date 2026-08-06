//! Sign-in for the browser client.
//!
//! [`login`] performs the OAuth 2.0 password grant against
//! `POST /api/auth/token` and keeps the returned access token; every later
//! `/api` request carries it as `Authorization: Bearer <token>`. The token
//! lives in `localStorage` so a reload does not log you out, and in a
//! thread-local so the fetch helpers can reach it without threading a context
//! parameter through every component.
//!
//! Tokens expire server-side, so a 401 is routine rather than exceptional:
//! it clears the stored token and surfaces as [`Failure::Unauthorized`], which
//! the app turns back into the sign-in screen.
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

/// The shell WebSocket for `(id, host, shell)`, absolute (`ws://`/`wss://`
/// from the page's own origin — the socket opener does not resolve relative
/// URLs) with the token in the query string, the same concession the SSE
/// route makes: a browser socket cannot set headers.
pub fn shell_ws_url(id: &str, host: &str, shell: &str) -> String {
    let token = token().unwrap_or_default();
    let location = web_sys::window().map(|w| w.location());
    let origin_host = location
        .as_ref()
        .and_then(|l| l.host().ok())
        .unwrap_or_default();
    let scheme = match location.and_then(|l| l.protocol().ok()) {
        Some(p) if p == "https:" => "wss",
        _ => "ws",
    };
    format!(
        "{scheme}://{origin_host}/api/sessions/{id}/shells/{host}/{shell}/ws?token={}",
        encode(&token)
    )
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

/// The password grant. On success the token is stored and the caller gets the
/// identity it speaks for.
pub async fn login(username: &str, password: &str) -> Result<api::Identity, Failure> {
    let body = format!(
        "grant_type=password&username={}&password={}",
        encode(username.trim()),
        encode(password)
    );
    let req = Request::post("/api/auth/token")
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(body)
        .map_err(|e| Failure::Other(format!("sign in: {e}")))?;
    let issued: api::AccessToken = decode("sign in", req.send().await).await?;
    set_token(&issued.access_token);
    Ok(issued.user)
}

/// Redeem an operator-minted one-time code for a session.
pub async fn code_login(username: &str, code: &str) -> Result<api::Identity, Failure> {
    let body = format!(
        "grant_type=code&username={}&code={}",
        encode(username.trim()),
        encode(code.trim())
    );
    let req = Request::post("/api/auth/token")
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(body)
        .map_err(|e| Failure::Other(format!("redeem code: {e}")))?;
    let issued: api::AccessToken = decode("redeem code", req.send().await).await?;
    set_token(&issued.access_token);
    Ok(issued.user)
}

// The WebAuthn ceremonies: the server speaks the standard JSON that the
// browser's own `PublicKeyCredential.parse*OptionsFromJSON` and
// `credential.toJSON()` understand, so the bridge is two thin JS functions
// and the JSON passes through this module untouched.
#[wasm_bindgen::prelude::wasm_bindgen(inline_js = "
export async function myco_passkey_create(options_json) {
  const opts = PublicKeyCredential.parseCreationOptionsFromJSON(
    JSON.parse(options_json).publicKey);
  const cred = await navigator.credentials.create({ publicKey: opts });
  return JSON.stringify(cred.toJSON());
}
export async function myco_passkey_get(options_json) {
  const opts = PublicKeyCredential.parseRequestOptionsFromJSON(
    JSON.parse(options_json).publicKey);
  const cred = await navigator.credentials.get({ publicKey: opts });
  return JSON.stringify(cred.toJSON());
}
")]
extern "C" {
    #[wasm_bindgen(catch)]
    async fn myco_passkey_create(
        options_json: String,
    ) -> Result<wasm_bindgen::JsValue, wasm_bindgen::JsValue>;
    #[wasm_bindgen(catch)]
    async fn myco_passkey_get(
        options_json: String,
    ) -> Result<wasm_bindgen::JsValue, wasm_bindgen::JsValue>;
}

fn ceremony_err(what: &str, e: wasm_bindgen::JsValue) -> Failure {
    // `NotAllowedError` is the browser's spelling of "dismissed or timed
    // out" — the person changed their mind, not a fault to alarm about.
    let text = e
        .as_string()
        .or_else(|| {
            js_sys::Reflect::get(&e, &"message".into())
                .ok()
                .and_then(|m| m.as_string())
        })
        .unwrap_or_else(|| "cancelled".into());
    Failure::Other(format!("{what}: {text}"))
}

/// Enroll a passkey for the signed-in user (authenticated ceremony).
pub async fn register_passkey() -> Result<(), Failure> {
    let options: serde_json::Value = post("/api/auth/passkey/register/start").await?;
    let created = myco_passkey_create(options.to_string())
        .await
        .map_err(|e| ceremony_err("passkey enrollment", e))?;
    let credential: serde_json::Value = created
        .as_string()
        .and_then(|s| serde_json::from_str(&s).ok())
        .ok_or_else(|| Failure::Other("passkey enrollment: bad credential".into()))?;
    let _: serde_json::Value = post_json("/api/auth/passkey/register/finish", &credential).await?;
    Ok(())
}

/// Sign in with a passkey: challenge, authenticator, token.
pub async fn passkey_login(username: &str) -> Result<api::Identity, Failure> {
    let challenge: serde_json::Value = {
        let req = Request::post("/api/auth/passkey/login/start")
            .json(&serde_json::json!({ "username": username.trim() }))
            .map_err(|e| Failure::Other(format!("passkey: {e}")))?;
        decode("passkey", req.send().await).await?
    };
    let ticket = challenge["ticket"].as_str().unwrap_or_default().to_string();
    let asserted = myco_passkey_get(challenge["options"].to_string())
        .await
        .map_err(|e| ceremony_err("passkey", e))?;
    let credential: serde_json::Value = asserted
        .as_string()
        .and_then(|s| serde_json::from_str(&s).ok())
        .ok_or_else(|| Failure::Other("passkey: bad assertion".into()))?;
    let issued: api::AccessToken = {
        let req = Request::post("/api/auth/passkey/login/finish")
            .json(&serde_json::json!({ "ticket": ticket, "credential": credential }))
            .map_err(|e| Failure::Other(format!("passkey: {e}")))?;
        decode("passkey", req.send().await).await?
    };
    set_token(&issued.access_token);
    Ok(issued.user)
}

/// End the session server-side, then locally. The local clear happens either
/// way — a network failure must not leave the UI claiming to be signed in.
pub async fn logout() {
    let _: Result<api::Identity, _> = post("/api/auth/logout").await;
    clear_token();
}
