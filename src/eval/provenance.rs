//! Inspectable configuration and prompt evidence without copying authentication.

use serde_json::{Value, json};

use crate::Config;
use crate::generative_model::{BackendConfig, GenerativeModelConfig};

pub(super) fn runtime_settings(config: &Config) -> Value {
    json!({
        "max_prelude_bytes": config.max_prelude_bytes,
        "compaction_max_requests": config.compaction_max_requests,
    })
}

fn endpoint(value: &str) -> Option<String> {
    let mut url = url::Url::parse(value).ok()?;
    url.set_username("").ok()?;
    url.set_password(None).ok()?;
    url.set_query(None);
    url.set_fragment(None);
    Some(url.into())
}

fn backend(settings: &BackendConfig) -> Value {
    let (base_url, max_output_tokens, max_request_bytes, effort) = match settings {
        BackendConfig::Anthropic(value) => (
            &value.anthropic_base_url,
            Some(value.max_tokens_per_generate),
            value.max_request_bytes,
            value.effort,
        ),
        BackendConfig::OpenAIResponses(value) | BackendConfig::OpenAICompletions(value) => (
            &value.base_url,
            value.max_output_tokens,
            value.max_request_bytes,
            value.effort,
        ),
    };
    // An allowlist keeps new credential-bearing backend fields out of artifacts.
    json!({
        "endpoint": endpoint(base_url),
        "max_output_tokens": max_output_tokens,
        "max_request_bytes": max_request_bytes,
        "retry": settings.retry_policy(),
        "effort": effort,
    })
}

pub(super) fn configuration(config: &Config, model: &GenerativeModelConfig) -> Value {
    let spec = &model.model;
    json!({
        "runtime": runtime_settings(config),
        "model": {
            "key": spec.key, "api_id": spec.api_id,
            "protocol": spec.protocol, "thinking": spec.thinking,
            "context_window_tokens": spec.context_window_tokens,
            "auto_compact_at_tokens": spec.auto_compact_at_tokens,
            "max_image_base64_bytes": spec.max_image_base64_bytes,
            "max_truncated_resumes": spec.max_truncated_resumes,
        },
        "backend": backend(&model.backend_config),
        "system_prompt": model.system_prompt,
        "tools": model.tools,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::generative_model::OpenAIBackendConfig;

    #[test]
    fn backend_metadata_excludes_tokens_and_url_credentials() {
        let value = backend(&BackendConfig::OpenAIResponses(OpenAIBackendConfig {
            base_url: "https://user:password@example.com/v1?key=query-secret#fragment-secret"
                .into(),
            auth_token: "auth-secret".into(),
            ..Default::default()
        }));
        assert_eq!(value["endpoint"], "https://example.com/v1");
        let text = value.to_string();
        for secret in [
            "user",
            "password",
            "query-secret",
            "fragment-secret",
            "auth-secret",
        ] {
            assert!(!text.contains(secret));
        }
    }
}
