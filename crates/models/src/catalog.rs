//! The model catalog: `[gateways]` / `[models]` tables → usable
//! [`CatalogModel`]s. Ported from v2 (`main-v2:src/config/{file,mod}.rs`),
//! reshaped only in where the tables live: v3 embeds them in
//! `server.toml` (v2 had a separate `config.toml`; v1 still owns that
//! filename in the shared `~/.myco`).
//!
//! Hard errors are config-shape problems (unknown gateway, missing
//! protocol/base_url, incompatible thinking mode) and fail resolution.
//! Credential *lookups* that fail (unset env var, unreadable file) are
//! soft: recorded per-entry and reported when the model is actually used
//! ([`ModelCatalog::get`]) — configuring a model without its credential is
//! fine until then.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::{
    AnthropicBackendConfig, BackendConfig, CatalogModel, Effort, ModelCatalog, ModelSpec,
    OpenAIBackendConfig, Protocol, RetryPolicy, ThinkingMode,
};

pub const DEFAULT_MAX_OUTPUT_TOKENS: usize = 8192;
pub const DEFAULT_MAX_IMAGE_BASE64_BYTES: u64 = 5 * 1024 * 1024;
pub const DEFAULT_MAX_TRUNCATED_RESUMES: u32 = 3;
pub const DEFAULT_AUTO_COMPACT_FRACTION: f64 = 0.85;

/// The catalog's on-disk shape: the `[gateways]` and `[models]` tables plus
/// the default `model` key. In v3 these are sections of `server.toml`;
/// embed this struct `#[serde(flatten)]`-style or as fields of the file
/// shape that hosts it.
#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct CatalogFile {
    /// Default model **key** for new chats. Optional when exactly one
    /// `[models]` entry exists.
    #[serde(default)]
    pub model: Option<String>,
    /// `[gateways.NAME]`: places models are served from (protocol + base
    /// URL + auth). Referenced by `[models.*].gateway`.
    #[serde(default)]
    pub gateways: BTreeMap<String, GatewayEntry>,
    /// `[models.KEY]`: the model catalog. The key is what chats record and
    /// what clients offer.
    #[serde(default)]
    pub models: BTreeMap<String, ModelEntry>,
}

/// `[gateways.NAME]`: one place models are served from.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GatewayEntry {
    /// Wire protocol: `"anthropic-messages"`, `"openai-responses"`, or
    /// `"openai-completions"`.
    pub protocol: Protocol,
    /// Base URL including any path prefix, e.g. `https://openrouter.ai/api/v1`.
    pub base_url: String,
    /// Credential (see [`AuthEntry`]). Absent → no auth header.
    #[serde(default)]
    pub auth: Option<AuthEntry>,
    /// Transient-failure retry knobs (see [`RetryEntry`]). Retry behavior
    /// is a property of the *endpoint* — a flaky local proxy wants patience
    /// an official API does not need — which is why it sits on the gateway.
    #[serde(default)]
    pub retry: Option<RetryEntry>,
}

/// `[gateways.NAME.retry]` / `[models.KEY.retry]`: transient-failure retry
/// knobs, each overlaying the built-in default individually — setting one
/// does not silently reset the others.
#[derive(Debug, Clone, Copy, PartialEq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetryEntry {
    /// Total attempts including the first; `1` disables retry.
    #[serde(default)]
    pub max_attempts: Option<u32>,
    #[serde(default)]
    pub initial_backoff_ms: Option<u64>,
    #[serde(default)]
    pub max_backoff_ms: Option<u64>,
    #[serde(default)]
    pub backoff_multiplier: Option<f64>,
}

/// `[models.KEY]`: one catalog entry. `gateway` pulls `protocol` /
/// `base_url` / `auth` from a `[gateways.*]` entry; fields set here
/// override it.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelEntry {
    /// Name of a `[gateways.*]` entry supplying protocol / base_url / auth.
    #[serde(default)]
    pub gateway: Option<String>,
    #[serde(default)]
    pub protocol: Option<Protocol>,
    #[serde(default)]
    pub base_url: Option<String>,
    /// Credential override (see [`AuthEntry`]). Absent → the gateway's.
    #[serde(default)]
    pub auth: Option<AuthEntry>,
    /// Wire id sent to the provider (request `model` field). Defaults to
    /// the catalog key, so it is only needed when they differ
    /// (e.g. key `kimi-k3` → `api_id = "moonshotai/kimi-k3"`).
    #[serde(default)]
    pub api_id: Option<String>,
    /// Required: context window in tokens (drives context UX and
    /// auto-compact heuristics — a wrong silent default would corrupt
    /// both).
    pub context_window: u64,
    /// `"adaptive"` | `"budget"` | `"effort"` | `"none"`.
    /// Default per protocol: anthropic-messages → adaptive, both OpenAI
    /// dialects → effort.
    #[serde(default)]
    pub thinking: Option<ThinkingMode>,
    /// Per-generate output token cap (default 8192).
    #[serde(default)]
    pub max_output_tokens: Option<usize>,
    #[serde(default)]
    pub max_image_base64_bytes: Option<u64>,
    /// Reasoning/thinking effort requested from the provider:
    /// `"low"` | `"medium"` | `"high"` | `"max"`. Unset → high.
    #[serde(default)]
    pub effort: Option<Effort>,
    /// Auto-compact when a turn ends with the prompt at this share of the
    /// model's `context_window` (0 < f < 1). Unset → 0.85.
    #[serde(default)]
    pub auto_compact_at: Option<f64>,
    /// Consecutive `max_tokens` truncations one turn resumes through before
    /// handing back the partial answer (`0` disables the resume).
    #[serde(default)]
    pub max_truncated_resumes: Option<u32>,
    /// Retry override (see [`RetryEntry`]). Like `auth`, the model's table
    /// replaces the gateway's wholesale — a field unset here falls back to
    /// the built-in default, not to the gateway's value.
    #[serde(default)]
    pub retry: Option<RetryEntry>,
}

/// The `auth` value on a gateway or model entry.
///
/// - a bare string is the credential itself: `auth = "sk-…"`
/// - a table names a source:
///   `auth = { source = "env", var_name = "OPENROUTER_API_KEY" }`,
///   `auth = { source = "file", path = "~/.secrets/openrouter.token" }`
///   (trimmed file contents), or `auth = { source = "none" }` (explicitly
///   credential-less — useful to override a gateway's auth on one model).
///
/// Source *lookup* (env read, file read) happens at [`resolve_catalog`];
/// failures there are deferred to model use.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(try_from = "toml::Value")]
pub enum AuthEntry {
    /// The credential itself, inline.
    Token(String),
    /// Read the named environment variable.
    Env { var_name: String },
    /// Read (and trim) the file's contents; leading `~/` expands to home.
    File { path: String },
    /// Explicitly no credential (no auth header sent).
    None,
}

impl TryFrom<toml::Value> for AuthEntry {
    type Error = String;

    // Hand-rolled rather than an untagged serde enum: untagged parse
    // failures report "did not match any variant", which is useless in a
    // config error.
    fn try_from(v: toml::Value) -> Result<Self, String> {
        const SHAPE: &str = "expected a string (the token itself) or a table like \
                             { source = \"env\", var_name = \"NAME\" } / \
                             { source = \"file\", path = \"…\" } / { source = \"none\" }";
        let require_str = |t: &toml::Table, field: &str, source: &str| -> Result<String, String> {
            t.get(field)
                .and_then(|f| f.as_str())
                .map(str::to_string)
                .ok_or_else(|| format!("auth source \"{source}\" needs a string field `{field}`"))
        };
        let reject_extras = |t: &toml::Table, allowed: &[&str]| -> Result<(), String> {
            for key in t.keys() {
                if !allowed.contains(&key.as_str()) {
                    return Err(format!("auth: unknown field `{key}`; {SHAPE}"));
                }
            }
            Ok(())
        };
        match v {
            toml::Value::String(s) => Ok(AuthEntry::Token(s)),
            toml::Value::Table(t) => {
                let source = t
                    .get("source")
                    .and_then(|s| s.as_str())
                    .ok_or_else(|| format!("auth table needs a string `source`; {SHAPE}"))?
                    .to_string();
                match source.as_str() {
                    "env" => {
                        reject_extras(&t, &["source", "var_name"])?;
                        Ok(AuthEntry::Env {
                            var_name: require_str(&t, "var_name", "env")?,
                        })
                    }
                    "file" => {
                        reject_extras(&t, &["source", "path"])?;
                        Ok(AuthEntry::File {
                            path: require_str(&t, "path", "file")?,
                        })
                    }
                    "none" => {
                        reject_extras(&t, &["source"])?;
                        Ok(AuthEntry::None)
                    }
                    other => Err(format!(
                        "auth: unknown source {other:?}; expected \"env\", \"file\", or \"none\""
                    )),
                }
            }
            other => Err(format!("auth: invalid type {}; {SHAPE}", other.type_str())),
        }
    }
}

/// Read and trim a credential file; leading `~/` expands to `$HOME`.
pub fn read_auth_file(path: &Path) -> Result<String, String> {
    let expanded: PathBuf = match path.strip_prefix("~") {
        Ok(rest) => std::env::var_os("HOME")
            .map(|h| PathBuf::from(h).join(rest))
            .ok_or_else(|| "could not resolve home directory".to_string())?,
        Err(_) => path.to_path_buf(),
    };
    let text =
        std::fs::read_to_string(&expanded).map_err(|e| format!("{}: {e}", expanded.display()))?;
    let token = text.trim();
    if token.is_empty() {
        return Err(format!("{}: file is empty", expanded.display()));
    }
    Ok(token.to_string())
}

/// Build the model catalog from a parsed [`CatalogFile`], and with it the
/// default model for new chats — one call, because the default is only
/// meaningful against the catalog it was checked against, and two calls
/// invited a caller to check it against a different one.
///
/// `env` is injected so tests never read the process environment; the
/// binary passes `|k| std::env::var(k).ok()` and [`read_auth_file`].
pub fn resolve_catalog(
    file: &CatalogFile,
    env: &impl Fn(&str) -> Option<String>,
    read_auth: &impl Fn(&Path) -> Result<String, String>,
) -> Result<(ModelCatalog, Option<String>), String> {
    let mut entries = BTreeMap::new();

    for (key, entry) in &file.models {
        let gateway = match &entry.gateway {
            Some(name) => Some(file.gateways.get(name).ok_or_else(|| {
                format!(
                    "model `{key}`: unknown gateway `{name}` (configured: [{}])",
                    file.gateways.keys().cloned().collect::<Vec<_>>().join(", ")
                )
            })?),
            None => None,
        };

        let protocol = entry
            .protocol
            .or(gateway.map(|g| g.protocol))
            .ok_or_else(|| {
                format!("model `{key}`: no protocol — set `protocol` or reference a `gateway`")
            })?;
        let base_url = entry
            .base_url
            .clone()
            .or_else(|| gateway.map(|g| g.base_url.clone()))
            .ok_or_else(|| {
                format!("model `{key}`: no base_url — set `base_url` or reference a `gateway`")
            })?;
        if base_url.trim().is_empty() {
            return Err(format!(
                "model `{key}`: base_url is empty — give the gateway's URL, \
                 e.g. `https://api.anthropic.com`"
            ));
        }
        // Model-level auth overrides the gateway's; absent everywhere → no
        // auth header (same as `{ source = "none" }`).
        let auth = entry
            .auth
            .clone()
            .or_else(|| gateway.and_then(|g| g.auth.clone()));

        let thinking = entry
            .thinking
            .unwrap_or_else(|| ThinkingMode::default_for(protocol));
        if !thinking.compatible_with(protocol) {
            return Err(format!(
                "model `{key}`: thinking `{thinking}` is not valid for protocol `{protocol}` \
                 (anthropic-messages: adaptive|budget|none; \
                 openai-responses / openai-completions: effort|none)"
            ));
        }

        let (token, auth_error) = match auth {
            None | Some(AuthEntry::None) => (String::new(), None),
            Some(AuthEntry::Token(token)) => (token, None),
            Some(AuthEntry::Env { var_name }) => match env(&var_name) {
                Some(v) => (v, None),
                None => (
                    String::new(),
                    Some(format!(
                        "model `{key}`: auth env var `{var_name}` is unset or empty"
                    )),
                ),
            },
            Some(AuthEntry::File { path }) => match read_auth(Path::new(&path)) {
                Ok(v) => (v, None),
                Err(e) => (String::new(), Some(format!("model `{key}`: auth file {e}"))),
            },
        };

        // The fraction becomes a token count here, so downstream compares
        // plain numbers and a bad value is a startup error rather than a
        // surprise hours into an unattended run.
        let auto_compact_fraction = entry
            .auto_compact_at
            .unwrap_or(DEFAULT_AUTO_COMPACT_FRACTION);
        if !(auto_compact_fraction > 0.0 && auto_compact_fraction < 1.0) {
            return Err(format!(
                "model `{key}`: auto_compact_at must be greater than 0 and less than 1 \
                 (got {auto_compact_fraction})"
            ));
        }
        let spec = ModelSpec {
            key: key.clone(),
            api_id: entry.api_id.clone().unwrap_or_else(|| key.clone()),
            protocol,
            thinking,
            context_window_tokens: entry.context_window,
            max_image_base64_bytes: entry
                .max_image_base64_bytes
                .unwrap_or(DEFAULT_MAX_IMAGE_BASE64_BYTES),
            auto_compact_at_tokens: (entry.context_window as f64 * auto_compact_fraction) as u64,
            max_truncated_resumes: entry
                .max_truncated_resumes
                .unwrap_or(DEFAULT_MAX_TRUNCATED_RESUMES),
        };
        let max_output = entry.max_output_tokens.unwrap_or(DEFAULT_MAX_OUTPUT_TOKENS);
        // Like `auth`, the model's retry table replaces the gateway's
        // wholesale; the overlay onto defaults happens per field inside
        // `resolve_retry`.
        let retry = resolve_retry(entry.retry.or_else(|| gateway.and_then(|g| g.retry)));
        let effort = Some(entry.effort.unwrap_or(Effort::DEFAULT));
        let backend = match protocol {
            Protocol::AnthropicMessages => BackendConfig::Anthropic(AnthropicBackendConfig {
                anthropic_base_url: base_url,
                anthropic_auth_token: token,
                max_tokens_per_generate: max_output,
                retry,
                effort,
                ..Default::default()
            }),
            // Both OpenAI dialects take the same settings; the variant only
            // records which wire format the gateway serves.
            Protocol::OpenAIResponses | Protocol::OpenAICompletions => {
                let openai = OpenAIBackendConfig {
                    base_url,
                    auth_token: token,
                    max_output_tokens: Some(max_output),
                    retry,
                    effort,
                    ..Default::default()
                };
                match protocol {
                    Protocol::OpenAICompletions => BackendConfig::OpenAICompletions(openai),
                    _ => BackendConfig::OpenAIResponses(openai),
                }
            }
        };

        entries.insert(
            key.clone(),
            CatalogModel {
                spec,
                backend,
                auth_error,
            },
        );
    }

    let catalog = ModelCatalog::new(entries);
    let default = default_model(file, &catalog)?;
    Ok((catalog, default))
}

/// The default model for new chats: the file's `model` key, or the sole
/// catalog entry when the file names none. `Ok(None)` is the modelless
/// workspace (an empty catalog); a named key that does not exist is a
/// startup error — silently running modelless would hide the typo.
fn default_model(file: &CatalogFile, catalog: &ModelCatalog) -> Result<Option<String>, String> {
    match &file.model {
        Some(key) => {
            if !catalog.contains(key) {
                return Err(format!(
                    "default model {key:?} is not in the catalog (configured: [{}])",
                    catalog.keys().join(", ")
                ));
            }
            Ok(Some(key.clone()))
        }
        None => {
            let keys = catalog.keys();
            match keys.as_slice() {
                [] => Ok(None),
                [sole] => Ok(Some(sole.to_string())),
                _ => Err(format!(
                    "several models are configured ([{}]) — set `model = \"…\"` to pick the \
                     default for new chats",
                    keys.join(", ")
                )),
            }
        }
    }
}

/// Overlay a config `[retry]` table onto the built-in policy, field by
/// field — setting one knob does not silently reset the others.
///
/// `max_attempts` is clamped to at least 1: `0` reads as "do not retry",
/// not "never send the request".
fn resolve_retry(entry: Option<RetryEntry>) -> RetryPolicy {
    let base = RetryPolicy::default();
    let Some(entry) = entry else { return base };
    RetryPolicy {
        max_attempts: entry.max_attempts.unwrap_or(base.max_attempts).max(1),
        initial_backoff: entry
            .initial_backoff_ms
            .map(std::time::Duration::from_millis)
            .unwrap_or(base.initial_backoff),
        max_backoff: entry
            .max_backoff_ms
            .map(std::time::Duration::from_millis)
            .unwrap_or(base.max_backoff),
        backoff_multiplier: entry.backoff_multiplier.unwrap_or(base.backoff_multiplier),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_env(_: &str) -> Option<String> {
        None
    }
    fn no_file(_: &Path) -> Result<String, String> {
        Err("no files in tests".into())
    }

    fn parse(text: &str) -> CatalogFile {
        toml::from_str(text).expect("catalog parses")
    }

    /// Two models behind one gateway. It names its default, because
    /// resolution now settles the default too: a file with a choice to make
    /// and no choice made does not resolve.
    const GATEWAY_CATALOG: &str = r#"
model = "fast"

[gateways.g]
protocol = "openai-completions"
base_url = "https://example.test/v1"
auth = "sk-inline"

[models.fast]
gateway = "g"
context_window = 100000

[models.smart]
gateway = "g"
api_id = "vendor/smart-1"
context_window = 200000
"#;

    #[test]
    fn a_gateway_supplies_protocol_url_and_auth_to_its_models() {
        let (catalog, _) =
            resolve_catalog(&parse(GATEWAY_CATALOG), &no_env, &no_file).expect("resolve");
        let fast = catalog.get("fast").expect("usable");
        assert_eq!(fast.spec.api_id, "fast", "api_id defaults to the key");
        let smart = catalog.get("smart").expect("usable");
        assert_eq!(smart.spec.api_id, "vendor/smart-1");
        match &smart.backend {
            BackendConfig::OpenAICompletions(b) => {
                assert_eq!(b.base_url, "https://example.test/v1");
                assert_eq!(b.auth_token, "sk-inline");
            }
            other => panic!("wrong backend: {other:?}"),
        }
    }

    #[test]
    fn a_missing_env_credential_is_soft_until_the_model_is_used() {
        let text = r#"
[models.m]
protocol = "anthropic-messages"
base_url = "https://api.anthropic.com"
auth = { source = "env", var_name = "NOPE_KEY" }
context_window = 100000
"#;
        let (catalog, _) =
            resolve_catalog(&parse(text), &no_env, &no_file).expect("resolve succeeds");
        let err = catalog.get("m").expect_err("use is refused");
        assert!(err.contains("NOPE_KEY"), "{err}");
        assert!(catalog.contains("m"), "the entry still exists");
    }

    #[test]
    fn shape_errors_fail_resolution_by_name() {
        let unknown_gateway = r#"
[models.m]
gateway = "nope"
context_window = 1000
"#;
        let err = resolve_catalog(&parse(unknown_gateway), &no_env, &no_file).unwrap_err();
        assert!(err.contains("unknown gateway"), "{err}");

        let bad_thinking = r#"
[models.m]
protocol = "anthropic-messages"
base_url = "https://x.test"
thinking = "effort"
context_window = 1000
"#;
        let err = resolve_catalog(&parse(bad_thinking), &no_env, &no_file).unwrap_err();
        assert!(err.contains("thinking"), "{err}");
    }

    /// The default arrives from the same call that built the catalog, so
    /// it cannot have been checked against a different one.
    #[test]
    fn the_default_model_comes_back_with_the_catalog_that_validated_it() {
        let sole = r#"
[models.only]
protocol = "openai-completions"
base_url = "https://x.test"
context_window = 1000
"#;
        let (_, default) = resolve_catalog(&parse(sole), &no_env, &no_file).expect("resolve");
        assert_eq!(
            default.as_deref(),
            Some("only"),
            "one model needs no naming"
        );

        let (_, default) = resolve_catalog(&parse(""), &no_env, &no_file).expect("resolve");
        assert_eq!(
            default, None,
            "no models is a modelless workspace, not an error"
        );

        // Several with no pick, and a pick that names nothing, are both
        // startup errors: guessing would land in stored history.
        let unpicked = GATEWAY_CATALOG.replace("model = \"fast\"", "");
        let err = resolve_catalog(&parse(&unpicked), &no_env, &no_file).unwrap_err();
        assert!(err.contains("several models"), "{err}");

        let typo = GATEWAY_CATALOG.replace("\"fast\"", "\"nope\"");
        let err = resolve_catalog(&parse(&typo), &no_env, &no_file).unwrap_err();
        assert!(err.contains("nope"), "{err}");

        let (_, default) =
            resolve_catalog(&parse(GATEWAY_CATALOG), &no_env, &no_file).expect("resolve");
        assert_eq!(default.as_deref(), Some("fast"));
    }

    #[test]
    fn retry_tables_overlay_field_by_field() {
        let text = r#"
[gateways.g]
protocol = "openai-completions"
base_url = "https://x.test"

[gateways.g.retry]
max_attempts = 5

[models.m]
gateway = "g"
context_window = 1000
"#;
        let (catalog, _) = resolve_catalog(&parse(text), &no_env, &no_file).expect("resolve");
        let m = catalog.get("m").expect("usable");
        let retry = match &m.backend {
            BackendConfig::OpenAICompletions(b) => b.retry,
            other => panic!("wrong backend: {other:?}"),
        };
        assert_eq!(retry.max_attempts, 5);
        // Unset knobs keep the built-in defaults, not zeros.
        assert_eq!(
            retry.backoff_multiplier,
            RetryPolicy::default().backoff_multiplier
        );
    }
}
