//! REST client + shared wire types (mirrors `src/server/{wire,routes}.rs`).

use serde::Deserialize;

/// Base path for API calls. Relative so it works behind Trunk's `/api` proxy
/// (dev) and when served directly by `myco --mode server` (production).
const API: &str = "/api";

// ---------------------------------------------------------------------------
// Types (subset of server DTOs the GUI consumes)
// ---------------------------------------------------------------------------

#[derive(Clone, PartialEq, Deserialize)]
pub struct Health {
    pub version: String,
    pub hosts: usize,
}

#[derive(Clone, PartialEq, Deserialize)]
pub struct WireContent {
    pub kind: String,
    #[serde(default)]
    pub text: String,
}

#[derive(Clone, PartialEq, Deserialize)]
pub struct ToolUseView {
    pub id: String,
    pub name: String,
    pub input: serde_json::Value,
}

#[derive(Clone, PartialEq, Deserialize)]
pub struct MessageView {
    pub role: String,
    #[serde(default)]
    pub content: Vec<WireContent>,
    #[serde(default)]
    pub tool_uses: Vec<ToolUseView>,
}

#[derive(Clone, PartialEq, Deserialize)]
pub struct SessionDetail {
    pub id: String,
    pub model: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub messages: Vec<MessageView>,
    pub running: bool,
    #[serde(default)]
    pub context_window_tokens: u64,
}

/// Live event frames pushed over SSE (mirror of `WireEvent`).
#[derive(Clone, PartialEq, Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WireEvent {
    TextDelta {
        text: String,
    },
    ThinkingDelta {
        text: String,
    },
    ToolStarted {
        tool_use_id: String,
        name: String,
        input: serde_json::Value,
    },
    ToolFinished {
        tool_use_id: String,
        is_error: bool,
    },
    TurnFinished {},
    Usage {
        usage: TokenUsage,
    },
    TurnError {
        message: String,
    },
    Lagged {
        skipped: u64,
    },
    #[serde(other)]
    Unknown,
}

#[derive(Clone, PartialEq, Debug, Default, Deserialize)]
pub struct TokenUsage {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default)]
    pub cache_read_tokens: Option<u64>,
    #[serde(default)]
    pub cache_creation_tokens: Option<u64>,
}

impl TokenUsage {
    pub fn context_tokens(&self) -> u64 {
        self.input_tokens
            .saturating_add(self.cache_read_tokens.unwrap_or(0))
            .saturating_add(self.cache_creation_tokens.unwrap_or(0))
    }
}

// ---------------------------------------------------------------------------
// Fetch helpers
// ---------------------------------------------------------------------------

use gloo_net::http::Request;

async fn get_json<T: for<'de> Deserialize<'de>>(path: &str) -> Result<T, String> {
    let resp = Request::get(path).send().await.map_err(|e| e.to_string())?;
    if !resp.ok() {
        return Err(format!("{} {}", resp.status(), resp.status_text()));
    }
    resp.json::<T>().await.map_err(|e| e.to_string())
}

async fn send_json<T: for<'de> Deserialize<'de>>(
    method: &str,
    path: &str,
    body: serde_json::Value,
) -> Result<T, String> {
    let builder = match method {
        "POST" => Request::post(path),
        "PATCH" => Request::patch(path),
        _ => Request::post(path),
    };
    let resp = builder
        .header("content-type", "application/json")
        .body(body.to_string())
        .map_err(|e| e.to_string())?
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.ok() {
        let status = format!("{} {}", resp.status(), resp.status_text());
        if let Ok(v) = resp.json::<serde_json::Value>().await
            && let Some(msg) = v.get("error").and_then(|e| e.as_str())
        {
            return Err(msg.to_string());
        }
        return Err(status);
    }
    resp.json::<T>().await.map_err(|e| e.to_string())
}

pub async fn health() -> Result<Health, String> {
    get_json(&format!("{API}/health")).await
}

pub async fn get_session(id: &str) -> Result<SessionDetail, String> {
    get_json(&format!("{API}/sessions/{id}")).await
}

/// Create a session on the server's configured default model (no key sent).
pub async fn create_session() -> Result<SessionDetail, String> {
    send_json("POST", &format!("{API}/sessions"), serde_json::json!({})).await
}

pub async fn send_message(id: &str, text: &str) -> Result<(), String> {
    let _: serde_json::Value = send_json(
        "POST",
        &format!("{API}/sessions/{id}/messages"),
        serde_json::json!({ "text": text }),
    )
    .await?;
    Ok(())
}

pub async fn cancel(id: &str) -> Result<(), String> {
    let _: serde_json::Value = send_json(
        "POST",
        &format!("{API}/sessions/{id}/cancel"),
        serde_json::json!({}),
    )
    .await?;
    Ok(())
}

/// URL of the per-session SSE feed (consumed via `web_sys::EventSource`).
pub fn events_url(id: &str) -> String {
    format!("{API}/sessions/{id}/events")
}
