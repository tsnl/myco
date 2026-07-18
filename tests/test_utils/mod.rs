#![allow(dead_code, unused_imports)]
use std::sync::Once;

use myco::generative_model::{
    AnthropicBackendConfig, BackendConfig, ModelSpec, OpenAIResponsesBackendConfig, Protocol,
    ThinkingMode,
};

mod scripted_model;
mod transcript;

pub use scripted_model::ScriptedModel;
pub use transcript::format_transcript;

static INIT: Once = Once::new();

/// Load `.env` once (for live-model integration tests that need API keys).
#[allow(dead_code)]
pub fn load_dotenv() {
    INIT.call_once(|| {
        dotenvy::dotenv().ok();
    });
}

// Live-test model wiring. Myco ships no built-in catalog, so live tests build
// their spec + backend directly from the environment (mirrors what a user's
// config.toml entry would resolve to).

pub fn live_anthropic_haiku() -> (ModelSpec, BackendConfig) {
    let spec = ModelSpec {
        key: "claude-haiku-4-5".into(),
        api_id: "claude-haiku-4-5".into(),
        protocol: Protocol::AnthropicMessages,
        thinking: ThinkingMode::Budget,
        context_window_tokens: 200_000,
    };
    let backend = BackendConfig::Anthropic(AnthropicBackendConfig {
        anthropic_base_url: std::env::var("ANTHROPIC_BASE_URL")
            .unwrap_or_else(|_| "https://api.anthropic.com".into()),
        anthropic_auth_token: std::env::var("ANTHROPIC_AUTH_TOKEN")
            .or_else(|_| std::env::var("ANTHROPIC_API_KEY"))
            .unwrap_or_default(),
        ..Default::default()
    });
    (spec, backend)
}

pub fn live_xai_grok() -> (ModelSpec, BackendConfig) {
    let spec = ModelSpec {
        key: "grok-4.5-build".into(),
        api_id: "grok-4.5-build".into(),
        protocol: Protocol::OpenAIResponses,
        thinking: ThinkingMode::Effort,
        context_window_tokens: 500_000,
    };
    let backend = BackendConfig::OpenAIResponses(OpenAIResponsesBackendConfig {
        base_url: std::env::var("XAI_API_BASE_URL")
            .or_else(|_| std::env::var("OPENAI_BASE_URL"))
            .unwrap_or_else(|_| "https://api.x.ai/v1".into()),
        auth_token: std::env::var("XAI_API_KEY")
            .or_else(|_| std::env::var("OPENAI_API_KEY"))
            .unwrap_or_default(),
        ..Default::default()
    });
    (spec, backend)
}

pub fn live_openrouter_kimi() -> (ModelSpec, BackendConfig) {
    let spec = ModelSpec {
        key: "kimi-k3".into(),
        api_id: "moonshotai/kimi-k3".into(),
        protocol: Protocol::OpenAIResponses,
        thinking: ThinkingMode::Effort,
        context_window_tokens: 1_000_000,
    };
    let backend = BackendConfig::OpenAIResponses(OpenAIResponsesBackendConfig {
        base_url: std::env::var("OPENROUTER_BASE_URL")
            .unwrap_or_else(|_| "https://openrouter.ai/api/v1".into()),
        auth_token: std::env::var("OPENROUTER_API_KEY").unwrap_or_default(),
        ..Default::default()
    });
    (spec, backend)
}
