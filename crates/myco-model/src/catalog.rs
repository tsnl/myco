//! The resolved model catalog: wire protocols, thinking and effort settings,
//! retry policy, model specs, and the backend (gateway + credentials) that
//! serves each model.

use super::*;

/// Wire protocol a model is served over.
///
/// Serde strings are the config.toml `protocol` values.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    serde::Serialize,
    serde::Deserialize,
    schemars::JsonSchema,
)]
pub enum Protocol {
    #[serde(rename = "anthropic-messages")]
    AnthropicMessages,
    #[serde(rename = "openai-responses")]
    OpenAIResponses,
    #[serde(rename = "openai-completions")]
    OpenAICompletions,
}

impl std::fmt::Display for Protocol {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Protocol::AnthropicMessages => f.write_str("anthropic-messages"),
            Protocol::OpenAIResponses => f.write_str("openai-responses"),
            Protocol::OpenAICompletions => f.write_str("openai-completions"),
        }
    }
}

/// How thinking/reasoning is requested for a model.
///
/// Serde strings are the config.toml `thinking` values. Compatibility is
/// per-protocol (validated at catalog resolution): Anthropic Messages takes
/// `adaptive` | `budget` | `none`; OpenAI Responses takes `effort` | `none`.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    serde::Serialize,
    serde::Deserialize,
    schemars::JsonSchema,
)]
#[serde(rename_all = "lowercase")]
pub enum ThinkingMode {
    /// Anthropic `thinking.type: "adaptive"` + `output_config.effort`
    /// (frontier models; older models reject it).
    Adaptive,
    /// Anthropic `thinking.type: "enabled"` + a `budget_tokens` mapped from
    /// [`Effort`] (e.g. Haiku 4.5).
    Budget,
    /// OpenAI-style `reasoning.effort`.
    Effort,
    /// Do not request thinking.
    None,
}

impl std::fmt::Display for ThinkingMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            ThinkingMode::Adaptive => "adaptive",
            ThinkingMode::Budget => "budget",
            ThinkingMode::Effort => "effort",
            ThinkingMode::None => "none",
        })
    }
}

impl ThinkingMode {
    /// Default mode when a catalog entry does not set `thinking`.
    pub fn default_for(protocol: Protocol) -> Self {
        match protocol {
            Protocol::AnthropicMessages => ThinkingMode::Adaptive,
            Protocol::OpenAIResponses | Protocol::OpenAICompletions => ThinkingMode::Effort,
        }
    }

    /// Whether this mode is servable over `protocol`.
    pub fn compatible_with(self, protocol: Protocol) -> bool {
        match protocol {
            Protocol::AnthropicMessages => {
                matches!(
                    self,
                    ThinkingMode::Adaptive | ThinkingMode::Budget | ThinkingMode::None
                )
            }
            Protocol::OpenAIResponses | Protocol::OpenAICompletions => {
                matches!(self, ThinkingMode::Effort | ThinkingMode::None)
            }
        }
    }
}

/// Agent policy for retrying transient generation failures, discarding failed drafts.
/// Resolved from `[gateways.NAME.retry]` or `[models.KEY.retry]`.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct RetryPolicy {
    /// Total attempts including the first. `1` disables retry.
    pub max_attempts: u32,
    pub initial_backoff: std::time::Duration,
    /// Ceiling on one wait. A longer provider minimum must stop retries rather
    /// than cause an earlier request or an unbounded wait.
    pub max_backoff: std::time::Duration,
    pub backoff_multiplier: f64,
    /// Optional elapsed recovery budget, starting at the first retryable failure.
    /// Includes later waits and requests; the original request has no new deadline.
    #[serde(default)]
    pub max_elapsed: Option<std::time::Duration>,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 3,
            initial_backoff: std::time::Duration::from_millis(500),
            max_backoff: std::time::Duration::from_secs(30),
            backoff_multiplier: 2.0,
            max_elapsed: None,
        }
    }
}

impl RetryPolicy {
    /// Wait before `attempt` (1-based). Attempt 1 is the original send and
    /// never waits; `retry_after` is the provider's ask, honoured when it
    /// exceeds the capped computed backoff. Callers must stop if the resulting
    /// minimum exceeds [`Self::max_backoff`] or their remaining elapsed budget.
    ///
    /// No jitter: myco is one client per user, so there is no fleet to
    /// de-synchronise, and a deterministic schedule is one less thing to
    /// reason about when reading an overnight log.
    pub fn backoff(
        &self,
        attempt: u32,
        retry_after: Option<std::time::Duration>,
    ) -> std::time::Duration {
        if attempt <= 1 {
            return std::time::Duration::ZERO;
        }
        let steps = attempt.saturating_sub(2);
        let factor = self.backoff_multiplier.max(1.0).powi(steps.min(32) as i32);
        let millis = (self.initial_backoff.as_millis() as f64 * factor).min(u64::MAX as f64);
        let computed = std::time::Duration::from_millis(millis as u64);
        retry_after
            .unwrap_or(std::time::Duration::ZERO)
            .max(computed.min(self.max_backoff))
    }
}

/// A resolved model: everything the protocol drivers need, minus credentials
/// (those live in [`BackendConfig`]). Built by the application configuration from the
/// `[models]` / `[gateways]` catalog in config.toml — myco ships no built-in
/// models.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelSpec {
    /// Catalog key: what the user types after `--model` and what sessions
    /// record. Distinct from `api_id` so one wire model can appear under
    /// several keys (e.g. routed via different gateways).
    pub key: String,
    /// Wire id sent to the provider (the request `model` field).
    pub api_id: String,
    pub protocol: Protocol,
    pub thinking: ThinkingMode,
    /// Context window for UX (`USER n/m`) and auto-compact heuristics.
    pub context_window_tokens: u64,
    /// Largest image this model accepts, measured on the base64 payload.
    /// Enforced locally by `view_image` and by REPL `@path` attachments so an
    /// oversized image fails with a clear message instead of a provider 400.
    /// Always concrete: config resolution applies the `max_image_base64_bytes`
    /// entry or its default, and callers downstream take this value.
    pub max_image_base64_bytes: u64,
    /// How many consecutive `max_tokens` truncations one turn resumes through
    /// before handing control back (`0` never resumes). Resolved from the
    /// model's `max_truncated_resumes` or its default; the agent takes this
    /// value via the agent's continuation policy.
    pub max_truncated_resumes: u32,
    /// Prompt size at which the session runner compacts without being asked.
    /// Myco's config defaults to the full context window. Library callers can
    /// use `None` when they own compaction policy themselves.
    ///
    /// Resolved from the model's `auto_compact_at` *fraction* and output reserve against
    /// `context_window_tokens` so the comparison downstream is a plain token
    /// count, and the fraction is validated once, at startup.
    pub auto_compact_at_tokens: Option<u64>,
}

impl std::fmt::Display for ModelSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.key)
    }
}

/// One usable catalog entry: spec plus the backend (gateway + credentials)
/// that serves it.
#[derive(Debug, Clone)]
pub struct CatalogModel {
    pub spec: ModelSpec,
    pub backend: BackendConfig,
    /// Set when the auth source did not resolve (env var unset, auth file
    /// unreadable). Reported by [`ModelCatalog::get`] when the model is
    /// actually used — configuring a model without its credential is fine
    /// until then.
    pub auth_error: Option<String>,
}

/// Key → model catalog resolved from config.toml. Empty when the user has not
/// configured any models.
#[derive(Debug, Clone, Default)]
pub struct ModelCatalog {
    entries: std::collections::BTreeMap<String, CatalogModel>,
}

impl ModelCatalog {
    pub fn new(entries: std::collections::BTreeMap<String, CatalogModel>) -> Self {
        Self { entries }
    }

    /// Look up a usable model. Errors are user-actionable: unknown keys list
    /// the configured catalog; entries with unresolved credentials report the
    /// failing auth source (env var / file).
    pub fn get(&self, key: &str) -> Result<&CatalogModel, String> {
        let Some(entry) = self.entries.get(key) else {
            if self.entries.is_empty() {
                return Err(format!(
                    "unknown model {key:?}: no models configured — define [models] \
                     (and [gateways]) in config.toml"
                ));
            }
            return Err(format!(
                "unknown model {key:?}; configured models: [{}]",
                self.keys().join(", ")
            ));
        };
        if let Some(err) = &entry.auth_error {
            return Err(err.clone());
        }
        Ok(entry)
    }

    /// Key exists (regardless of whether its credential resolved).
    pub fn contains(&self, key: &str) -> bool {
        self.entries.contains_key(key)
    }

    /// Spec for `key`, ignoring credential state. For settings that must be
    /// read while merely *configuring* a run (the image cap host workers are
    /// spawned with) rather than using the model — [`Self::get`] is still the
    /// gate for that.
    pub fn spec(&self, key: &str) -> Option<&ModelSpec> {
        self.entries.get(key).map(|entry| &entry.spec)
    }

    pub fn keys(&self) -> Vec<&str> {
        self.entries.keys().map(String::as_str).collect()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Reasoning / extended-thinking effort level sent to providers.
///
/// Anthropic adaptive models map this to `output_config.effort`; Haiku-style models
/// map it onto a `thinking.budget_tokens` value. OpenAI/xAI gateways receive it as
/// `reasoning.effort`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Effort {
    Low,
    Medium,
    High,
    Max,
}

impl Effort {
    /// Wire string used by Anthropic `output_config.effort` and OpenAI `reasoning.effort`.
    pub fn as_str(self) -> &'static str {
        match self {
            Effort::Low => "low",
            Effort::Medium => "medium",
            Effort::High => "high",
            Effort::Max => "max",
        }
    }

    /// Approximate Anthropic extended-thinking token budget for non-adaptive models.
    pub fn budget_tokens(self) -> u32 {
        match self {
            Effort::Low => 1_024,
            Effort::Medium => 4_096,
            Effort::High => 16_000,
            Effort::Max => 64_000,
        }
    }

    /// Sensible default for interactive agent sessions.
    pub const DEFAULT: Effort = Effort::High;
}

impl std::fmt::Display for Effort {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for Effort {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "low" | "l" => Ok(Effort::Low),
            "medium" | "med" | "m" => Ok(Effort::Medium),
            "high" | "h" => Ok(Effort::High),
            "max" | "x" => Ok(Effort::Max),
            other => Err(format!(
                "unknown effort {other:?}; expected low|medium|high|max"
            )),
        }
    }
}

/// Provider backend settings: gateway base URL, credential, per-request knobs.
#[derive(Debug, Clone)]
pub enum BackendConfig {
    Anthropic(AnthropicBackendConfig),
    /// Responses API (`{base_url}/responses`).
    OpenAIResponses(OpenAIBackendConfig),
    /// Chat Completions API (`{base_url}/chat/completions`).
    OpenAICompletions(OpenAIBackendConfig),
}

impl BackendConfig {
    pub fn retry_policy(&self) -> RetryPolicy {
        match self {
            Self::Anthropic(config) => config.retry,
            Self::OpenAIResponses(config) | Self::OpenAICompletions(config) => config.retry,
        }
    }

    pub fn protocol(&self) -> Protocol {
        match self {
            BackendConfig::Anthropic(_) => Protocol::AnthropicMessages,
            BackendConfig::OpenAIResponses(_) => Protocol::OpenAIResponses,
            BackendConfig::OpenAICompletions(_) => Protocol::OpenAICompletions,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Backoff doubles from `initial_backoff`, stops at `max_backoff`, and a
    /// provider's `Retry-After` remains a minimum; the caller decides whether
    /// its budget permits waiting that long.
    #[test]
    fn retry_backoff_grows_caps_and_honours_retry_after() {
        use std::time::Duration;
        let policy = RetryPolicy {
            max_attempts: 6,
            initial_backoff: Duration::from_millis(100),
            max_backoff: Duration::from_millis(1000),
            backoff_multiplier: 2.0,
            max_elapsed: None,
        };

        // Attempt 1 is the original send; it never waits.
        assert_eq!(policy.backoff(1, None), Duration::ZERO);
        assert_eq!(policy.backoff(2, None), Duration::from_millis(100));
        assert_eq!(policy.backoff(3, None), Duration::from_millis(200));
        assert_eq!(policy.backoff(4, None), Duration::from_millis(400));
        // Growth stops at the cap rather than running away.
        assert_eq!(policy.backoff(9, None), Duration::from_millis(1000));

        // A longer Retry-After wins over the computed wait...
        assert_eq!(
            policy.backoff(2, Some(Duration::from_millis(500))),
            Duration::from_millis(500)
        );
        // A provider minimum is never shortened to the policy's wait cap.
        assert_eq!(
            policy.backoff(2, Some(Duration::from_secs(3600))),
            Duration::from_secs(3600)
        );
        // A shorter one does not shrink the backoff.
        assert_eq!(
            policy.backoff(3, Some(Duration::from_millis(1))),
            Duration::from_millis(200)
        );
    }

    fn spec(key: &str, protocol: Protocol) -> ModelSpec {
        ModelSpec {
            key: key.into(),
            api_id: key.into(),
            protocol,
            thinking: ThinkingMode::default_for(protocol),
            context_window_tokens: 1_000_000,
            max_image_base64_bytes: 5 * 1024 * 1024,
            max_truncated_resumes: 3,
            auto_compact_at_tokens: None,
        }
    }

    #[test]
    fn thinking_defaults_and_protocol_compatibility() {
        assert_eq!(
            ThinkingMode::default_for(Protocol::AnthropicMessages),
            ThinkingMode::Adaptive
        );
        assert_eq!(
            ThinkingMode::default_for(Protocol::OpenAIResponses),
            ThinkingMode::Effort
        );
        assert!(ThinkingMode::Budget.compatible_with(Protocol::AnthropicMessages));
        assert!(ThinkingMode::None.compatible_with(Protocol::AnthropicMessages));
        assert!(!ThinkingMode::Effort.compatible_with(Protocol::AnthropicMessages));
        assert!(ThinkingMode::None.compatible_with(Protocol::OpenAIResponses));
        // Both OpenAI dialects take the same effort-shaped thinking.
        assert_eq!(
            ThinkingMode::default_for(Protocol::OpenAICompletions),
            ThinkingMode::Effort
        );
        assert!(ThinkingMode::None.compatible_with(Protocol::OpenAICompletions));
        assert!(!ThinkingMode::Adaptive.compatible_with(Protocol::OpenAICompletions));
        assert!(!ThinkingMode::Adaptive.compatible_with(Protocol::OpenAIResponses));
        assert!(!ThinkingMode::Budget.compatible_with(Protocol::OpenAIResponses));
    }

    #[test]
    fn protocol_serde_uses_config_strings() {
        assert_eq!(
            serde_json::to_value(Protocol::AnthropicMessages).unwrap(),
            serde_json::json!("anthropic-messages")
        );
        assert_eq!(
            serde_json::from_value::<Protocol>(serde_json::json!("openai-responses")).unwrap(),
            Protocol::OpenAIResponses
        );
        assert_eq!(
            serde_json::from_value::<Protocol>(serde_json::json!("openai-completions")).unwrap(),
            Protocol::OpenAICompletions
        );
    }

    #[test]
    fn empty_catalog_get_says_no_models_configured() {
        let catalog = ModelCatalog::default();
        assert!(catalog.is_empty());
        let err = catalog.get("kimi-k3").unwrap_err();
        assert!(err.contains("no models configured"), "{err}");
        assert!(err.contains("[models]"), "{err}");
    }

    #[test]
    fn catalog_get_unknown_key_lists_configured_models() {
        let entry = CatalogModel {
            spec: spec("opus", Protocol::AnthropicMessages),
            backend: BackendConfig::Anthropic(AnthropicBackendConfig::default()),
            auth_error: None,
        };
        let catalog = ModelCatalog::new([("opus".to_string(), entry)].into());
        let err = catalog.get("opsu").unwrap_err();
        assert!(err.contains("unknown model \"opsu\""), "{err}");
        assert!(err.contains("[opus]"), "{err}");
        assert!(catalog.get("opus").is_ok());
    }

    #[test]
    fn catalog_get_reports_deferred_auth_error() {
        let entry = CatalogModel {
            spec: spec("kimi", Protocol::OpenAIResponses),
            backend: BackendConfig::OpenAIResponses(OpenAIBackendConfig::default()),
            auth_error: Some("model `kimi`: auth env:OPENROUTER_API_KEY is unset".into()),
        };
        let catalog = ModelCatalog::new([("kimi".to_string(), entry)].into());
        let err = catalog.get("kimi").unwrap_err();
        assert!(err.contains("OPENROUTER_API_KEY"), "{err}");
    }

    #[test]
    fn new_rejects_protocol_mismatch() {
        let result = new(GenerativeModelConfig {
            model: spec("grok", Protocol::OpenAIResponses),
            tools: vec![],
            system_prompt: String::new(),
            backend_config: BackendConfig::Anthropic(AnthropicBackendConfig {
                anthropic_auth_token: "dummy".into(),
                ..Default::default()
            }),
        });
        let err = match result {
            Ok(_) => panic!("expected mismatch"),
            Err(e) => e,
        };
        assert!(err.to_string().contains("speaks openai-responses"), "{err}");
    }

    #[test]
    fn older_backend_configs_default_to_a_thirty_megabyte_request_cap() {
        let mut anthropic = serde_json::to_value(AnthropicBackendConfig::default()).unwrap();
        let mut openai = serde_json::to_value(OpenAIBackendConfig::default()).unwrap();
        for config in [&mut anthropic, &mut openai] {
            assert_eq!(config["max_request_bytes"], 30_000_000);
            config.as_object_mut().unwrap().remove("max_request_bytes");
        }
        assert_eq!(
            serde_json::from_value::<AnthropicBackendConfig>(anthropic)
                .unwrap()
                .max_request_bytes,
            30_000_000
        );
        assert_eq!(
            serde_json::from_value::<OpenAIBackendConfig>(openai)
                .unwrap()
                .max_request_bytes,
            30_000_000
        );
    }
}
