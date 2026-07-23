//! On-disk config file shape (`~/.myco/config.toml`).
//!
//! Parse only: the model catalog (`[gateways]` / `[models]`, default `model`)
//! and scalar knobs (`attach_timeout_secs`). Scalar knobs parse as `Option`
//! so "unset" stays distinguishable from "explicitly set"; every default is
//! applied once, at resolve time in [`crate::config::Config`]. Remote hosts
//! are not configured here — they come from `Host` aliases in `~/.ssh/config`
//! ([`crate::harness`]).

use std::collections::BTreeMap;
use std::path::Path;

use crate::generative_model::{Protocol, ThinkingMode};

/// On-disk config file shape (`~/.myco/config.toml`). Hosts come from
/// `~/.ssh/config`; models come from the `[gateways]` / `[models]` catalog
/// here — myco ships no built-in models. Catalog *resolution* (auth, overlay,
/// validation) lives in [`crate::config::Config`].
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
}

/// `[gateways.NAME]`: one place models are served from.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GatewayEntry {
    /// Wire protocol: `"anthropic-messages"` or `"openai-responses"`.
    pub protocol: Protocol,
    /// Base URL including any path prefix, e.g. `https://openrouter.ai/api/v1`.
    pub base_url: String,
    /// Credential (see [`AuthEntry`]). Absent → no auth header.
    #[serde(default)]
    pub auth: Option<AuthEntry>,
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
    /// Default per protocol: anthropic-messages → adaptive, openai-responses → effort.
    #[serde(default)]
    pub thinking: Option<ThinkingMode>,
    /// Per-generate output token cap (default 8192).
    #[serde(default)]
    pub max_output_tokens: Option<usize>,
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
/// [`crate::config::Config`]; failures there are deferred to model use.
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
/// `~/.myco/config.toml`) lives in [`crate::config::Config`].
pub fn load_file_config(path: &Path) -> Result<FileConfig, String> {
    if !path.exists() {
        return Ok(FileConfig::default());
    }
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("read config {}: {e}", path.display()))?;
    parse_file_config_str(&text).map_err(|e| format!("parse config {}: {e}", path.display()))
}

/// Example config written by docs / first-run hints.
pub fn example_config_toml() -> String {
    r#"# Myco config (~/.myco/config.toml)
# Override path with MYCO_CONFIG or myco --config.
#
# The local host is always enabled in-process. Remote hosts are NOT listed
# here: every concrete `Host` alias in ~/.ssh/config (no wildcards; Includes
# are followed) is a remote host of the same name, attached lazily as
# `ssh <alias> myco --mode host`. Put user / port / identity / ProxyJump in
# ~/.ssh/config; `myco` must be on the remote PATH non-interactive SSH uses.
#
# Models are configured here — myco ships none built in. A [gateways.*] entry
# holds protocol + base_url + auth; a [models.*] entry is a model key you pass
# to --model. The auth value is either the token itself ("sk-...") or a
# source table: { source = "env", var_name = "NAME" },
# { source = "file", path = "~/.secrets/x.token" }, { source = "none" }.
# Omitting auth sends no auth header (fine for local servers).
#
# Top-level keys must come before the [gateways]/[models] tables (TOML).

# Default model key when more than one model is configured (--model overrides).
model = "grok-4.5-build"

# Per-remote connect timeout in seconds on first tool use (0 disables).
# Remotes connect lazily; startup does not wait for them.
attach_timeout_secs = 10

[gateways.anthropic]
protocol = "anthropic-messages"
base_url = "https://api.anthropic.com"
auth = { source = "env", var_name = "ANTHROPIC_API_KEY" }

[gateways.xai]
protocol = "openai-responses"
base_url = "https://api.x.ai/v1"
auth = { source = "env", var_name = "XAI_API_KEY" }

[gateways.openrouter]
protocol = "openai-responses"
base_url = "https://openrouter.ai/api/v1"
auth = { source = "env", var_name = "OPENROUTER_API_KEY" }

[models.claude-opus-4-8]
gateway = "anthropic"
context_window = 1_000_000

[models.claude-haiku-4-5]
gateway = "anthropic"
thinking = "budget"          # older models reject adaptive thinking
context_window = 200_000

# Keys with dots need quoting (TOML): [models."grok-4.5-build"]
[models."grok-4.5-build"]
gateway = "xai"
context_window = 500_000

[models.kimi-k3]
gateway = "openrouter"
api_id = "moonshotai/kimi-k3"  # wire id differs from the short key
context_window = 1_000_000
"#
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scalar_knobs_parse_as_set_or_unset() {
        assert_eq!(parse_file_config_str("").unwrap().attach_timeout_secs, None);
        assert_eq!(FileConfig::default().attach_timeout_secs, None);
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
        let text = r#"
[models.a]
protocol = "openai-responses"
base_url = "https://h"
auth = "sk-literal-token"
context_window = 1000

[models.b]
protocol = "openai-responses"
base_url = "https://h"
auth = { source = "file", path = "~/.secrets/x.token" }
context_window = 1000

[models.c]
protocol = "openai-responses"
base_url = "https://h"
auth = { source = "none" }
context_window = 1000
"#;
        let file = parse_file_config_str(text).unwrap();
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
        let bad_source = "[models.x]\nprotocol = \"openai-responses\"\nbase_url = \"https://h\"\n\
                          auth = { source = \"keychain\" }\ncontext_window = 1000\n";
        let err = parse_file_config_str(bad_source).unwrap_err();
        assert!(err.contains("unknown source \"keychain\""), "{err}");

        let missing_field = "[models.x]\nprotocol = \"openai-responses\"\nbase_url = \"https://h\"\n\
                             auth = { source = \"env\" }\ncontext_window = 1000\n";
        let err = parse_file_config_str(missing_field).unwrap_err();
        assert!(err.contains("`var_name`"), "{err}");

        let extra_field = "[models.x]\nprotocol = \"openai-responses\"\nbase_url = \"https://h\"\n\
                           auth = { source = \"none\", token = \"x\" }\ncontext_window = 1000\n";
        let err = parse_file_config_str(extra_field).unwrap_err();
        assert!(err.contains("unknown field `token`"), "{err}");

        let bad_type = "[models.x]\nprotocol = \"openai-responses\"\nbase_url = \"https://h\"\n\
                        auth = 42\ncontext_window = 1000\n";
        let err = parse_file_config_str(bad_type).unwrap_err();
        assert!(err.contains("invalid type"), "{err}");
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
        let file = parse_file_config_str(&example_config_toml()).unwrap();
        assert_eq!(file.model.as_deref(), Some("grok-4.5-build"));
        assert_eq!(file.attach_timeout_secs, Some(10));
        assert_eq!(file.gateways.len(), 3);
        assert_eq!(file.models.len(), 4);
        assert_eq!(
            file.models["kimi-k3"].api_id.as_deref(),
            Some("moonshotai/kimi-k3")
        );
    }
}
