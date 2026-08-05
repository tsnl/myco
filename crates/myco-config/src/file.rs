//! On-disk config file shape (`~/.myco/config.toml`).
//!
//! Parse only: the model catalog (`[gateways]` / `[models]`, default `model`)
//! and scalar knobs (`attach_timeout_secs`, `max_prelude_bytes`). Scalar knobs parse as `Option`
//! so "unset" stays distinguishable from "explicitly set"; every default is
//! applied once, at resolve time in [`crate::Config`]. Remote hosts
//! are not configured here — they come from `Host` aliases in `~/.ssh/config`
//! ([`myco_machines::harness`]).

use std::collections::BTreeMap;
use std::path::Path;

use myco_models::{Protocol, ThinkingMode};

/// On-disk config file shape (`~/.myco/config.toml`). Hosts come from
/// `~/.ssh/config`; models come from the `[gateways]` / `[models]` catalog
/// here — myco ships no built-in models. Catalog *resolution* (auth, overlay,
/// validation) lives in [`crate::Config`].
#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct FileConfig {
    /// Default model **key** for the interactive CLI (`--model` overrides).
    /// Optional when exactly one `[models]` entry exists.
    #[serde(default)]
    pub model: Option<String>,
    /// `[gateways.NAME]`: places models are served from (protocol + base URL
    /// + auth). Referenced by `[models.*].gateway`.
    #[serde(default)]
    pub gateways: BTreeMap<String, GatewayEntry>,
    /// `[models.KEY]`: the model catalog. The key is what `--model` takes and
    /// what sessions record.
    #[serde(default)]
    pub models: BTreeMap<String, ModelEntry>,
    /// Per-remote-host connect timeout in seconds on first tool use (lazy
    /// spawn + hello). `0` disables the timeout; unset → default applied at
    /// resolve. (Config key kept as `attach_timeout_secs`.)
    #[serde(default)]
    pub attach_timeout_secs: Option<u64>,
    /// Cap on the rendered prelude (`workspace/prelude/` entries) appended to
    /// every agent system prompt. Enforced, not clamped: the `prelude` tool
    /// refuses an edit that would cross it and startup exits against a prelude
    /// already over it. `None` = unset;
    /// [`myco_prompts::DEFAULT_MAX_PRELUDE_BYTES`] applies at resolve.
    #[serde(default)]
    pub max_prelude_bytes: Option<usize>,
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
    /// Transient-failure retry knobs (see [`RetryEntry`]). Retry behavior is
    /// a property of the *endpoint* — a flaky local proxy wants patience an
    /// official API does not need — which is why it sits on the gateway.
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

/// `[models.KEY]`: one catalog entry. `gateway` pulls `protocol` / `base_url`
/// / `auth` from a `[gateways.*]` entry; fields set here override it.
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
    /// Wire id sent to the provider (request `model` field). Defaults to the
    /// catalog key, so it is only needed when they differ
    /// (e.g. key `kimi-k3` → `api_id = "moonshotai/kimi-k3"`).
    #[serde(default)]
    pub api_id: Option<String>,
    /// Required: context window in tokens (drives `USER n/m` and
    /// auto-compact heuristics — a wrong silent default would corrupt both).
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
    /// Auto-compact when a turn ends with the prompt at this share of the
    /// model's `context_window` (0 < f < 1). Per model because the trigger is
    /// a share of *this* model's window. Unset -> the built-in default
    /// ([`crate::DEFAULT_AUTO_COMPACT_FRACTION`]).
    #[serde(default)]
    pub auto_compact_at: Option<f64>,
    /// Consecutive `max_tokens` truncations one turn resumes through before
    /// handing back the partial answer (`0` disables the resume). Unset ->
    /// [`crate::DEFAULT_MAX_TRUNCATED_RESUMES`].
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
/// Source *lookup* (env read, file read) happens at catalog resolution in
/// [`crate::Config`]; failures there are deferred to model use.
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

    // Hand-rolled rather than an untagged serde enum: untagged parse failures
    // report "did not match any variant", which is useless in a config error.
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

/// Parse `config.toml` text. Rejects the removed `[[remote_hosts]]` section
/// rather than silently ignoring it.
pub fn parse_file_config_str(text: &str) -> Result<FileConfig, String> {
    let value: toml::Value =
        toml::from_str(text).map_err(|e| format!("invalid config TOML: {e}"))?;
    if value.get("remote_hosts").is_some() {
        return Err(
            "`[[remote_hosts]]` is no longer supported: remote hosts now come from \
             `Host` aliases in ~/.ssh/config — remove the section"
                .into(),
        );
    }
    value
        .try_into()
        .map_err(|e| format!("invalid config TOML: {e}"))
}

/// Load the on-disk knobs/model config from `path`. Missing file →
/// [`FileConfig::default`]. Path defaulting (`--config` → `$MYCO_CONFIG` →
/// `~/.myco/config.toml`) lives in [`crate::Config`].
pub fn load_file_config(path: &Path) -> Result<FileConfig, String> {
    if !path.exists() {
        return Ok(FileConfig::default());
    }
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("read config {}: {e}", path.display()))?;
    parse_file_config_str(&text).map_err(|e| format!("parse config {}: {e}", path.display()))
}

/// `[models.KEY]` table with the boilerplate the shape tests retype: inline
/// `openai-responses` protocol, a dummy base_url, `context_window = 1000`.
/// `extra_lines` (e.g. an `auth = …` form) land between them.
#[cfg(test)]
pub(crate) fn model_toml(key: &str, extra_lines: &[&str]) -> String {
    let mut out =
        format!("[models.{key}]\nprotocol = \"openai-responses\"\nbase_url = \"https://h\"\n");
    for line in extra_lines {
        out.push_str(line);
        out.push('\n');
    }
    out.push_str("context_window = 1000\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scalar_knobs_parse_as_set_or_unset() {
        assert_eq!(parse_file_config_str("").unwrap().attach_timeout_secs, None);
        assert_eq!(FileConfig::default().attach_timeout_secs, None);
        assert_eq!(parse_file_config_str("").unwrap().max_prelude_bytes, None);
        assert_eq!(
            parse_file_config_str("max_prelude_bytes = 4096")
                .unwrap()
                .max_prelude_bytes,
            Some(4096)
        );
        assert_eq!(
            parse_file_config_str("attach_timeout_secs = 5")
                .unwrap()
                .attach_timeout_secs,
            Some(5)
        );
    }

    #[test]
    fn legacy_remote_hosts_section_rejected() {
        let text = r#"
[[remote_hosts]]
name = "devbox"
ssh = "devbox"
"#;
        let err = parse_file_config_str(text).unwrap_err();
        assert!(err.contains("no longer supported"), "{err}");
        assert!(err.contains(".ssh/config"), "{err}");
    }

    #[test]
    fn model_key_is_a_free_string() {
        let file = parse_file_config_str("model = \"anything-goes\"").unwrap();
        assert_eq!(file.model.as_deref(), Some("anything-goes"));
        assert_eq!(FileConfig::default().model, None);
    }

    #[test]
    fn gateway_and_model_tables_parse() {
        let text = r#"
model = "kimi-k3"

[gateways.openrouter]
protocol = "openai-responses"
base_url = "https://openrouter.ai/api/v1"
auth = { source = "env", var_name = "OPENROUTER_API_KEY" }

[models.kimi-k3]
gateway = "openrouter"
api_id = "moonshotai/kimi-k3"
context_window = 1_000_000

[models.local-qwen]
protocol = "openai-responses"
base_url = "http://localhost:11434/v1"
thinking = "none"
context_window = 32768
"#;
        let file = parse_file_config_str(text).unwrap();
        assert_eq!(file.model.as_deref(), Some("kimi-k3"));
        let gw = &file.gateways["openrouter"];
        assert_eq!(gw.protocol, Protocol::OpenAIResponses);
        assert_eq!(
            gw.auth,
            Some(AuthEntry::Env {
                var_name: "OPENROUTER_API_KEY".into()
            })
        );
        let kimi = &file.models["kimi-k3"];
        assert_eq!(kimi.gateway.as_deref(), Some("openrouter"));
        assert_eq!(kimi.api_id.as_deref(), Some("moonshotai/kimi-k3"));
        assert_eq!(kimi.context_window, 1_000_000);
        assert_eq!(kimi.thinking, None);
        let local = &file.models["local-qwen"];
        assert_eq!(local.protocol, Some(Protocol::OpenAIResponses));
        assert_eq!(local.auth, None);
        assert_eq!(local.thinking, Some(ThinkingMode::None));
    }

    #[test]
    fn auth_entry_forms_parse() {
        let text = [
            model_toml("a", &[r#"auth = "sk-literal-token""#]),
            model_toml(
                "b",
                &[r#"auth = { source = "file", path = "~/.secrets/x.token" }"#],
            ),
            model_toml("c", &[r#"auth = { source = "none" }"#]),
        ]
        .join("\n");
        let file = parse_file_config_str(&text).unwrap();
        assert_eq!(
            file.models["a"].auth,
            Some(AuthEntry::Token("sk-literal-token".into()))
        );
        assert_eq!(
            file.models["b"].auth,
            Some(AuthEntry::File {
                path: "~/.secrets/x.token".into()
            })
        );
        assert_eq!(file.models["c"].auth, Some(AuthEntry::None));
    }

    #[test]
    fn auth_entry_shape_errors_are_actionable() {
        let err_for =
            |auth_line| parse_file_config_str(&model_toml("x", &[auth_line])).unwrap_err();

        let err = err_for(r#"auth = { source = "keychain" }"#);
        assert!(err.contains("unknown source \"keychain\""), "{err}");

        let err = err_for(r#"auth = { source = "env" }"#);
        assert!(err.contains("`var_name`"), "{err}");

        let err = err_for(r#"auth = { source = "none", token = "x" }"#);
        assert!(err.contains("unknown field `token`"), "{err}");

        let err = err_for("auth = 42");
        assert!(err.contains("invalid type"), "{err}");
    }

    #[test]
    fn max_image_base64_bytes_parses_as_set_or_unset() {
        let text = [
            model_toml("capped", &["max_image_base64_bytes = 12_582_912"]),
            model_toml("stock", &[]),
        ]
        .join("\n");
        let file = parse_file_config_str(&text).unwrap();
        assert_eq!(
            file.models["capped"].max_image_base64_bytes,
            Some(12_582_912)
        );
        assert_eq!(file.models["stock"].max_image_base64_bytes, None);
    }

    #[test]
    fn model_entry_requires_context_window() {
        let err = parse_file_config_str(
            "[models.x]\nprotocol = \"openai-responses\"\nbase_url = \"https://h\"\n",
        )
        .unwrap_err();
        assert!(err.contains("context_window"), "{err}");
    }

    #[test]
    fn unknown_entry_fields_are_rejected() {
        let err = parse_file_config_str("[models.x]\ncontext_window = 1000\nbase_uri = \"typo\"\n")
            .unwrap_err();
        assert!(err.contains("base_uri"), "{err}");
    }

    #[test]
    fn example_config_parses() {
        // Compact cut of the documented format (`src/manual/articles/
        // overview.md`): env-auth and literal-auth gateways, a gateway model
        // with its own wire id, and a gateway-less local model.
        let text = r#"
model = "grok-4.5-build"
attach_timeout_secs = 10

[gateways.xai]
protocol = "openai-responses"
base_url = "https://api.x.ai/v1"
auth = { source = "env", var_name = "XAI_API_KEY" }

[gateways.proxy]
protocol = "anthropic-messages"
base_url = "https://claude-proxy.corp"
auth = "sk-corp-token"

[models."grok-4.5-build"]
gateway = "xai"
context_window = 500_000

[models.kimi-k3]
gateway = "xai"
api_id = "moonshotai/kimi-k3"
context_window = 1_000_000

[models.qwen-local]
protocol = "openai-completions"
base_url = "http://localhost:11434/v1"
thinking = "none"
context_window = 32_768
"#;
        let file = parse_file_config_str(text).unwrap();
        assert_eq!(file.model.as_deref(), Some("grok-4.5-build"));
        assert_eq!(file.attach_timeout_secs, Some(10));
        assert_eq!(file.gateways.len(), 2);
        assert_eq!(file.models.len(), 3);
        assert_eq!(
            file.models["qwen-local"].protocol,
            Some(Protocol::OpenAICompletions)
        );
        assert_eq!(
            file.models["kimi-k3"].api_id.as_deref(),
            Some("moonshotai/kimi-k3")
        );
    }
}
