//! Startup configuration resolved from config.toml and the process environment.
//!
//! Runs once at application startup: [`Config::resolve`] takes optional
//! [`ConfigUserSettings`] overrides (CLI flags, embedder choices), loads the
//! config file (`--config` → `$MYCO_CONFIG` → `~/.myco/config.toml`), and
//! produces fully resolved settings — the **model catalog**, the host pool
//! (remote hosts from `~/.ssh/config` `Host` aliases), the default model key
//! (`--model` → config file `model` → sole catalog entry), and the color
//! decision for stdout rendering. Downstream code reads the resolved fields;
//! nothing else reads these environment variables or files.
//!
//! ## Model catalog
//!
//! Myco ships **no built-in models or gateways**: `[gateways.*]` and
//! `[models.*]` in config.toml are the entire catalog (see
//! [`crate::harness::example_config_toml`]). A model entry names a gateway (or
//! inlines `protocol` / `base_url` / `auth`) plus per-model metadata
//! (`api_id`, required `context_window`, `thinking`, `max_output_tokens`).
//!
//! Auth mechanisms: `env:VAR` (process environment), `token:NAME` (looked up
//! in a flat `tokens.toml` next to config.toml — keeps secrets out of a
//! shareable config), or `none` (local servers; no auth header sent).
//! A missing credential is **not** a resolve error: it is reported when the
//! model is actually used ([`ModelCatalog::get`]).
//!
//! Out of scope, deliberately: `.env` loading (dotenvy runs in `main` before
//! resolution so `env:VAR` sees its effect), `MYCO_HOME` (session storage
//! root; read by session code that also runs in `--mode host` workers where
//! no `Config` exists), and per-tool lookups like `MYCO_LYNX` (resolved at
//! tool-call time on whichever host runs the tool).

use std::collections::BTreeMap;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};

use crate::generative_model::{
    AnthropicBackendConfig, BackendConfig, CatalogModel, ModelCatalog, ModelSpec,
    OpenAIResponsesBackendConfig, Protocol, ThinkingMode,
};
use crate::harness::{FileConfig, HarnessConfig, load_file_config, load_ssh_host_aliases};

/// Default per-generate output token cap when a model entry sets none.
pub const DEFAULT_MAX_OUTPUT_TOKENS: usize = 8192;

// ---------------------------------------------------------------------------
// Auth mechanisms
// ---------------------------------------------------------------------------

/// Parsed `auth` string from a gateway/model entry.
#[derive(Debug, Clone, PartialEq, Eq)]
enum AuthRef {
    /// `env:VAR` — read the process environment.
    Env(String),
    /// `token:NAME` — look up `NAME` in tokens.toml.
    Token(String),
    /// `none` — no credential; no auth header is sent.
    None,
}

impl AuthRef {
    fn parse(s: &str) -> Result<Self, String> {
        if let Some(var) = s.strip_prefix("env:") {
            if var.is_empty() {
                return Err("auth \"env:\" is missing a variable name".into());
            }
            return Ok(AuthRef::Env(var.to_string()));
        }
        if let Some(name) = s.strip_prefix("token:") {
            if name.is_empty() {
                return Err("auth \"token:\" is missing a tokens.toml key".into());
            }
            return Ok(AuthRef::Token(name.to_string()));
        }
        if s == "none" {
            return Ok(AuthRef::None);
        }
        Err(format!(
            "invalid auth {s:?}: expected \"env:VAR\", \"token:NAME\" (tokens.toml), or \"none\""
        ))
    }
}

/// Load `tokens.toml` (flat `NAME = "secret"` table). Missing file → empty:
/// it only becomes an error when a model references `token:NAME`.
pub fn load_tokens_file(path: &Path) -> Result<BTreeMap<String, String>, String> {
    if !path.exists() {
        return Ok(BTreeMap::new());
    }
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("read tokens {}: {e}", path.display()))?;
    toml::from_str::<BTreeMap<String, String>>(&text).map_err(|e| {
        format!(
            "parse tokens {}: {e} (expected flat `NAME = \"secret\"` entries)",
            path.display()
        )
    })
}

// ---------------------------------------------------------------------------
// Colors
// ---------------------------------------------------------------------------

/// Color choice for stdout rendering (CLI `--color`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ColorMode {
    /// Colors when stdout is a TTY; `NO_COLOR` / `CLICOLOR_FORCE` / `TERM=dumb` respected.
    #[default]
    Auto,
    Always,
    Never,
}

impl std::fmt::Display for ColorMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            ColorMode::Auto => "auto",
            ColorMode::Always => "always",
            ColorMode::Never => "never",
        })
    }
}

impl std::str::FromStr for ColorMode {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "auto" => Ok(ColorMode::Auto),
            "always" | "on" => Ok(ColorMode::Always),
            "never" | "off" => Ok(ColorMode::Never),
            other => Err(format!(
                "unknown color mode {other:?}; expected auto|always|never"
            )),
        }
    }
}

// ---------------------------------------------------------------------------
// Config resolution
// ---------------------------------------------------------------------------

/// Startup-time overrides supplied by the embedding application (CLI flags,
/// tests). Any field set here wins over file/environment resolution.
#[derive(Debug, Clone, Default)]
pub struct ConfigUserSettings {
    pub color: ColorMode,
    /// Override TTY detection (tests / embedders). `None` → detect from stdout.
    pub stdout_is_tty: Option<bool>,
    /// Config file override (CLI `--config`).
    /// `None` → `$MYCO_CONFIG` → `~/.myco/config.toml`.
    pub harness_config_path: Option<PathBuf>,
    /// Model key override (CLI `--model`).
    /// `None` → config file `model` → sole catalog entry.
    pub model: Option<String>,
}

/// Fully resolved application configuration. Build once at startup with
/// [`Config::resolve`]; everything downstream reads these fields instead of
/// the environment or config files.
#[derive(Debug, Clone)]
pub struct Config {
    /// Whether stdout is a terminal (or the [`ConfigUserSettings`] override).
    pub stdout_is_tty: bool,
    /// Final color decision for stdout rendering ([`ColorMode`] + env overrides + TTY).
    pub colors_enabled: bool,
    /// Path the config file was loaded from
    /// (override → `$MYCO_CONFIG` → `~/.myco/config.toml`).
    pub harness_config_path: PathBuf,
    /// Sibling `tokens.toml` holding literal credentials (`token:NAME`).
    pub tokens_path: PathBuf,
    /// Host pool: knobs from the config file (missing file → defaults) plus
    /// remote hosts from `~/.ssh/config` `Host` aliases.
    pub harness: HarnessConfig,
    /// Model catalog resolved from `[gateways]` / `[models]`.
    pub models: ModelCatalog,
    /// Default model key (`--model` → config file `model` → sole entry).
    /// Always present in the catalog; credential presence is still checked at
    /// use time via [`ModelCatalog::get`].
    pub model: String,
}

impl Config {
    /// Resolve from the real process environment, stdout TTY state, the
    /// config + tokens files, and `~/.ssh/config` host aliases. Errors carry
    /// the offending path / entry name.
    pub fn resolve(settings: ConfigUserSettings) -> Result<Self, String> {
        let stdout_is_tty = std::io::stdout().is_terminal();
        Self::resolve_with(
            settings,
            |k| std::env::var(k).ok(),
            stdout_is_tty,
            load_file_config,
            load_ssh_host_aliases,
            load_tokens_file,
        )
    }

    /// Resolution against injected environment / loaders (tests, embedders).
    /// Empty environment values are treated as unset.
    pub fn resolve_with(
        settings: ConfigUserSettings,
        env: impl Fn(&str) -> Option<String>,
        stdout_is_tty: bool,
        load_file: impl FnOnce(&Path) -> Result<FileConfig, String>,
        ssh_aliases: impl FnOnce() -> Result<Vec<String>, String>,
        load_tokens: impl FnOnce(&Path) -> Result<BTreeMap<String, String>, String>,
    ) -> Result<Self, String> {
        let env = |key: &str| env(key).filter(|v| !v.is_empty());
        let ConfigUserSettings {
            color,
            stdout_is_tty: tty_override,
            harness_config_path,
            model: model_override,
        } = settings;

        let stdout_is_tty = tty_override.unwrap_or(stdout_is_tty);
        let harness_config_path = resolve_harness_config_path(harness_config_path, &env)?;
        let tokens_path = harness_config_path
            .parent()
            .unwrap_or(Path::new("."))
            .join("tokens.toml");
        let file = load_file(&harness_config_path)?;
        let tokens = load_tokens(&tokens_path)?;

        let models = resolve_catalog(&file, &env, &tokens, &tokens_path)?;
        let model = resolve_default_model(model_override, file.model.clone(), &models)?;

        let mut harness = file.into_harness_config(ssh_aliases()?);
        harness.models = models.clone();
        let colors_enabled = resolve_colors(color, &env, stdout_is_tty);

        Ok(Self {
            stdout_is_tty,
            colors_enabled,
            harness_config_path,
            tokens_path,
            harness,
            models,
            model,
        })
    }
}

/// `--config` override → `$MYCO_CONFIG` → `~/.myco/config.toml`.
fn resolve_harness_config_path(
    override_path: Option<PathBuf>,
    env: &impl Fn(&str) -> Option<String>,
) -> Result<PathBuf, String> {
    if let Some(p) = override_path {
        return Ok(p);
    }
    if let Some(p) = env("MYCO_CONFIG") {
        return Ok(PathBuf::from(p));
    }
    let home = dirs::home_dir().ok_or_else(|| "could not resolve home directory".to_string())?;
    Ok(home.join(".myco").join("config.toml"))
}

/// Build the model catalog from `[gateways]` / `[models]`.
///
/// Hard errors here are config-shape problems (unknown gateway, missing
/// protocol/base_url/auth, invalid auth string, incompatible thinking mode).
/// Missing *credentials* are soft: recorded per-entry and reported when the
/// model is actually used.
fn resolve_catalog(
    file: &FileConfig,
    env: &impl Fn(&str) -> Option<String>,
    tokens: &BTreeMap<String, String>,
    tokens_path: &Path,
) -> Result<ModelCatalog, String> {
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
        let auth_str = entry
            .auth
            .clone()
            .or_else(|| gateway.map(|g| g.auth.clone()))
            .ok_or_else(|| {
                format!(
                    "model `{key}`: no auth — set `auth = \"env:VAR\" | \"token:NAME\" | \
                     \"none\"` on the model or its gateway"
                )
            })?;
        let auth = AuthRef::parse(&auth_str).map_err(|e| format!("model `{key}`: {e}"))?;

        let thinking = entry
            .thinking
            .unwrap_or_else(|| ThinkingMode::default_for(protocol));
        if !thinking.compatible_with(protocol) {
            return Err(format!(
                "model `{key}`: thinking `{thinking}` is not valid for protocol `{protocol}` \
                 (anthropic-messages: adaptive|budget|none; openai-responses: effort|none)"
            ));
        }

        let (token, auth_error) = match auth {
            AuthRef::None => (String::new(), None),
            AuthRef::Env(var) => match env(&var) {
                Some(v) => (v, None),
                None => (
                    String::new(),
                    Some(format!(
                        "model `{key}`: auth env var `{var}` is unset or empty"
                    )),
                ),
            },
            AuthRef::Token(name) => match tokens.get(&name) {
                Some(v) => (v.clone(), None),
                None => (
                    String::new(),
                    Some(format!(
                        "model `{key}`: token `{name}` not found in {}",
                        tokens_path.display()
                    )),
                ),
            },
        };

        let spec = ModelSpec {
            key: key.clone(),
            api_id: entry.api_id.clone().unwrap_or_else(|| key.clone()),
            protocol,
            thinking,
            context_window_tokens: entry.context_window,
        };
        let max_output = entry.max_output_tokens.unwrap_or(DEFAULT_MAX_OUTPUT_TOKENS);
        let backend = match protocol {
            Protocol::AnthropicMessages => BackendConfig::Anthropic(AnthropicBackendConfig {
                anthropic_base_url: base_url,
                anthropic_auth_token: token,
                max_tokens_per_generate: max_output,
                ..Default::default()
            }),
            Protocol::OpenAIResponses => {
                BackendConfig::OpenAIResponses(OpenAIResponsesBackendConfig {
                    base_url,
                    auth_token: token,
                    max_output_tokens: Some(max_output),
                    ..Default::default()
                })
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

    Ok(ModelCatalog::new(entries))
}

/// `--model` → config file `model` → sole catalog entry. The chosen key must
/// exist in the catalog (credentials are checked later, at use).
fn resolve_default_model(
    override_key: Option<String>,
    file_key: Option<String>,
    catalog: &ModelCatalog,
) -> Result<String, String> {
    if let Some(key) = override_key.or(file_key) {
        if !catalog.contains(&key) {
            if catalog.is_empty() {
                return Err(format!(
                    "model {key:?} selected but no models are configured — define \
                     [models] (and [gateways]) in config.toml"
                ));
            }
            return Err(format!(
                "unknown model {key:?}; configured models: [{}]",
                catalog.keys().join(", ")
            ));
        }
        return Ok(key);
    }
    match catalog.keys().as_slice() {
        [] => Err(
            "no models configured — define [models] (and [gateways]) in config.toml; \
             see `myco --help overview` for the format"
                .into(),
        ),
        [only] => Ok(only.to_string()),
        keys => Err(format!(
            "no model selected — set `model = \"<key>\"` in config.toml or pass --model \
             (configured: [{}])",
            keys.join(", ")
        )),
    }
}

/// `Never`/`Always` win; `Auto` consults `NO_COLOR` (non-empty disables),
/// `CLICOLOR_FORCE` (non-empty, non-`"0"` forces), `TERM=dumb`, then the TTY.
fn resolve_colors(
    mode: ColorMode,
    env: &impl Fn(&str) -> Option<String>,
    stdout_is_tty: bool,
) -> bool {
    match mode {
        ColorMode::Always => true,
        ColorMode::Never => false,
        ColorMode::Auto => {
            if env("NO_COLOR").is_some() {
                return false;
            }
            if env("CLICOLOR_FORCE").is_some_and(|v| v != "0") {
                return true;
            }
            if env("TERM").as_deref() == Some("dumb") {
                return false;
            }
            stdout_is_tty
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::parse_file_config_str;

    fn env_of<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |key| {
            pairs
                .iter()
                .find(|(name, _)| *name == key)
                .map(|(_, value)| value.to_string())
        }
    }

    const CATALOG: &str = r#"
model = "kimi-k3"

[gateways.openrouter]
protocol = "openai-responses"
base_url = "https://openrouter.ai/api/v1"
auth = "env:OPENROUTER_API_KEY"

[gateways.anthropic]
protocol = "anthropic-messages"
base_url = "https://api.anthropic.com"
auth = "env:ANTHROPIC_API_KEY"

[models.kimi-k3]
gateway = "openrouter"
api_id = "moonshotai/kimi-k3"
context_window = 1_000_000

[models.opus]
gateway = "anthropic"
api_id = "claude-opus-4-8"
context_window = 1_000_000

[models.haiku-local]
protocol = "anthropic-messages"
base_url = "http://localhost:8080"
auth = "none"
thinking = "budget"
api_id = "claude-haiku-4-5"
context_window = 200_000
"#;

    fn resolve_toml(
        toml_text: &'static str,
        settings: ConfigUserSettings,
        env: impl Fn(&str) -> Option<String>,
        tokens: BTreeMap<String, String>,
    ) -> Result<Config, String> {
        Config::resolve_with(
            settings,
            env,
            false,
            move |_| parse_file_config_str(toml_text),
            || Ok(Vec::new()),
            move |_| Ok(tokens),
        )
    }

    fn resolve_catalog_cfg(env_pairs: &[(&str, &str)]) -> Config {
        resolve_toml(
            CATALOG,
            ConfigUserSettings::default(),
            env_of(env_pairs),
            BTreeMap::new(),
        )
        .unwrap()
    }

    #[test]
    fn gateway_ref_supplies_protocol_base_url_and_auth() {
        let cfg = resolve_catalog_cfg(&[("OPENROUTER_API_KEY", "or-key")]);
        let kimi = cfg.models.get("kimi-k3").unwrap();
        assert_eq!(kimi.spec.key, "kimi-k3");
        assert_eq!(kimi.spec.api_id, "moonshotai/kimi-k3");
        assert_eq!(kimi.spec.protocol, Protocol::OpenAIResponses);
        assert_eq!(kimi.spec.thinking, ThinkingMode::Effort);
        assert_eq!(kimi.spec.context_window_tokens, 1_000_000);
        match &kimi.backend {
            BackendConfig::OpenAIResponses(b) => {
                assert_eq!(b.base_url, "https://openrouter.ai/api/v1");
                assert_eq!(b.auth_token, "or-key");
                assert_eq!(b.max_output_tokens, Some(DEFAULT_MAX_OUTPUT_TOKENS));
            }
            other => panic!("expected OpenAI Responses backend, got {other:?}"),
        }
    }

    #[test]
    fn inline_model_needs_no_gateway_and_auth_none_is_usable() {
        let cfg = resolve_catalog_cfg(&[]);
        let local = cfg.models.get("haiku-local").unwrap();
        assert_eq!(local.spec.protocol, Protocol::AnthropicMessages);
        assert_eq!(local.spec.thinking, ThinkingMode::Budget);
        match &local.backend {
            BackendConfig::Anthropic(b) => {
                assert_eq!(b.anthropic_base_url, "http://localhost:8080");
                assert_eq!(b.anthropic_auth_token, "");
            }
            other => panic!("expected Anthropic backend, got {other:?}"),
        }
    }

    #[test]
    fn missing_env_credential_defers_until_use() {
        // Resolves fine without the env vars…
        let cfg = resolve_catalog_cfg(&[]);
        // …the default model is still selected…
        assert_eq!(cfg.model, "kimi-k3");
        // …and the error surfaces on use, naming the mechanism.
        let err = cfg.models.get("kimi-k3").unwrap_err();
        assert!(err.contains("OPENROUTER_API_KEY"), "{err}");
        assert!(err.contains("kimi-k3"), "{err}");
    }

    #[test]
    fn token_auth_reads_tokens_toml_and_missing_key_defers() {
        let toml_text = r#"
[models.proxy]
protocol = "openai-responses"
base_url = "https://proxy.corp/v1"
auth = "token:corp"
context_window = 100_000
"#;
        let cfg = resolve_toml(
            toml_text,
            ConfigUserSettings::default(),
            env_of(&[]),
            [("corp".to_string(), "sekrit".to_string())].into(),
        )
        .unwrap();
        match &cfg.models.get("proxy").unwrap().backend {
            BackendConfig::OpenAIResponses(b) => assert_eq!(b.auth_token, "sekrit"),
            other => panic!("unexpected backend {other:?}"),
        }

        let cfg = resolve_toml(
            toml_text,
            ConfigUserSettings::default(),
            env_of(&[]),
            BTreeMap::new(),
        )
        .unwrap();
        let err = cfg.models.get("proxy").unwrap_err();
        assert!(err.contains("token `corp` not found"), "{err}");
        assert!(err.contains("tokens.toml"), "{err}");
    }

    #[test]
    fn config_shape_errors_name_the_model() {
        let unknown_gateway = r#"
[models.x]
gateway = "nope"
context_window = 1000
"#;
        let err = resolve_toml(
            unknown_gateway,
            ConfigUserSettings::default(),
            env_of(&[]),
            BTreeMap::new(),
        )
        .unwrap_err();
        assert!(err.contains("model `x`"), "{err}");
        assert!(err.contains("unknown gateway `nope`"), "{err}");

        let no_protocol = r#"
[models.x]
base_url = "https://h"
auth = "none"
context_window = 1000
"#;
        let err = resolve_toml(
            no_protocol,
            ConfigUserSettings::default(),
            env_of(&[]),
            BTreeMap::new(),
        )
        .unwrap_err();
        assert!(err.contains("no protocol"), "{err}");

        let bad_auth = r#"
[models.x]
protocol = "openai-responses"
base_url = "https://h"
auth = "keychain:oops"
context_window = 1000
"#;
        let err = resolve_toml(
            bad_auth,
            ConfigUserSettings::default(),
            env_of(&[]),
            BTreeMap::new(),
        )
        .unwrap_err();
        assert!(err.contains("invalid auth"), "{err}");
        assert!(err.contains("env:VAR"), "{err}");
    }

    #[test]
    fn incompatible_thinking_mode_is_a_resolve_error() {
        let toml_text = r#"
[models.x]
protocol = "anthropic-messages"
base_url = "https://h"
auth = "none"
thinking = "effort"
context_window = 1000
"#;
        let err = resolve_toml(
            toml_text,
            ConfigUserSettings::default(),
            env_of(&[]),
            BTreeMap::new(),
        )
        .unwrap_err();
        assert!(err.contains("thinking `effort`"), "{err}");
        assert!(err.contains("anthropic-messages"), "{err}");
    }

    #[test]
    fn default_model_precedence_override_file_sole_entry() {
        // --model override wins over the file key.
        let cfg = resolve_toml(
            CATALOG,
            ConfigUserSettings {
                model: Some("opus".into()),
                ..Default::default()
            },
            env_of(&[]),
            BTreeMap::new(),
        )
        .unwrap();
        assert_eq!(cfg.model, "opus");

        // File key applies otherwise (CATALOG sets kimi-k3).
        assert_eq!(resolve_catalog_cfg(&[]).model, "kimi-k3");

        // A sole entry needs no selection at all.
        let sole = r#"
[models.only]
protocol = "openai-responses"
base_url = "https://h"
auth = "none"
context_window = 1000
"#;
        let cfg = resolve_toml(
            sole,
            ConfigUserSettings::default(),
            env_of(&[]),
            BTreeMap::new(),
        )
        .unwrap();
        assert_eq!(cfg.model, "only");
    }

    #[test]
    fn missing_model_selection_errors_are_actionable() {
        // No models at all.
        let err = resolve_toml(
            "",
            ConfigUserSettings::default(),
            env_of(&[]),
            BTreeMap::new(),
        )
        .unwrap_err();
        assert!(err.contains("no models configured"), "{err}");
        assert!(err.contains("[models]"), "{err}");

        // Multiple models, nothing selected.
        let two = r#"
[models.a]
protocol = "openai-responses"
base_url = "https://h"
auth = "none"
context_window = 1000

[models.b]
protocol = "openai-responses"
base_url = "https://h"
auth = "none"
context_window = 1000
"#;
        let err = resolve_toml(
            two,
            ConfigUserSettings::default(),
            env_of(&[]),
            BTreeMap::new(),
        )
        .unwrap_err();
        assert!(err.contains("no model selected"), "{err}");
        assert!(err.contains("[a, b]"), "{err}");

        // Unknown selection lists the catalog.
        let err = resolve_toml(
            two,
            ConfigUserSettings {
                model: Some("c".into()),
                ..Default::default()
            },
            env_of(&[]),
            BTreeMap::new(),
        )
        .unwrap_err();
        assert!(err.contains("unknown model \"c\""), "{err}");
        assert!(err.contains("[a, b]"), "{err}");
    }

    #[test]
    fn auth_ref_parse_forms() {
        assert_eq!(
            AuthRef::parse("env:XAI_API_KEY").unwrap(),
            AuthRef::Env("XAI_API_KEY".into())
        );
        assert_eq!(
            AuthRef::parse("token:corp").unwrap(),
            AuthRef::Token("corp".into())
        );
        assert_eq!(AuthRef::parse("none").unwrap(), AuthRef::None);
        assert!(AuthRef::parse("env:").is_err());
        assert!(AuthRef::parse("token:").is_err());
        assert!(AuthRef::parse("sk-raw-secret").is_err());
    }

    #[test]
    fn example_config_resolves_end_to_end() {
        let cfg = Config::resolve_with(
            ConfigUserSettings::default(),
            env_of(&[("XAI_API_KEY", "xai")]),
            false,
            |_| parse_file_config_str(&crate::harness::example_config_toml()),
            || Ok(Vec::new()),
            |_| Ok(BTreeMap::new()),
        )
        .unwrap();
        assert_eq!(cfg.model, "grok-4.5-build");
        assert!(cfg.models.get("grok-4.5-build").is_ok());
        // Anthropic entries resolve but defer their missing credential.
        let err = cfg.models.get("claude-opus-4-8").unwrap_err();
        assert!(err.contains("ANTHROPIC_API_KEY"), "{err}");
    }

    #[test]
    fn tokens_path_sits_next_to_config() {
        let cfg = resolve_toml(
            CATALOG,
            ConfigUserSettings {
                harness_config_path: Some(PathBuf::from("/etc/myco/config.toml")),
                ..Default::default()
            },
            env_of(&[]),
            BTreeMap::new(),
        )
        .unwrap();
        assert_eq!(cfg.tokens_path, PathBuf::from("/etc/myco/tokens.toml"));
    }

    #[test]
    fn color_mode_always_and_never_override_everything() {
        let cfg = resolve_toml(
            CATALOG,
            ConfigUserSettings {
                color: ColorMode::Always,
                ..Default::default()
            },
            env_of(&[("NO_COLOR", "1")]),
            BTreeMap::new(),
        )
        .unwrap();
        assert!(cfg.colors_enabled);
        let cfg = resolve_toml(
            CATALOG,
            ConfigUserSettings {
                color: ColorMode::Never,
                stdout_is_tty: Some(true),
                ..Default::default()
            },
            env_of(&[("CLICOLOR_FORCE", "1")]),
            BTreeMap::new(),
        )
        .unwrap();
        assert!(!cfg.colors_enabled);
    }

    #[test]
    fn auto_colors_follow_tty_and_env_overrides() {
        let auto = |env_pairs: &[(&str, &str)], tty: bool| {
            let env = env_of(env_pairs);
            let env = |k: &str| env(k).filter(|v| !v.is_empty());
            resolve_colors(ColorMode::Auto, &env, tty)
        };
        assert!(auto(&[], true));
        assert!(!auto(&[], false));
        // NO_COLOR (non-empty) disables, even on a TTY, and beats CLICOLOR_FORCE.
        assert!(!auto(&[("NO_COLOR", "1"), ("CLICOLOR_FORCE", "1")], true));
        // Empty NO_COLOR is unset.
        assert!(auto(&[("NO_COLOR", "")], true));
        // CLICOLOR_FORCE forces colors without a TTY; "0" does not.
        assert!(auto(&[("CLICOLOR_FORCE", "1")], false));
        assert!(!auto(&[("CLICOLOR_FORCE", "0")], false));
        // Dumb terminals stay plain.
        assert!(!auto(&[("TERM", "dumb")], true));
    }

    #[test]
    fn color_mode_parses() {
        assert_eq!("auto".parse::<ColorMode>().unwrap(), ColorMode::Auto);
        assert_eq!("ALWAYS".parse::<ColorMode>().unwrap(), ColorMode::Always);
        assert_eq!("off".parse::<ColorMode>().unwrap(), ColorMode::Never);
        assert!("rainbow".parse::<ColorMode>().is_err());
    }

    #[test]
    fn harness_config_path_override_beats_env_beats_home_default() {
        let path_for = |override_path: Option<PathBuf>, env_pairs: &[(&str, &str)]| {
            let env = env_of(env_pairs);
            let env = move |k: &str| env(k).filter(|v: &String| !v.is_empty());
            resolve_harness_config_path(override_path, &env).unwrap()
        };
        assert_eq!(
            path_for(
                Some(PathBuf::from("/tmp/x.toml")),
                &[("MYCO_CONFIG", "/env/y.toml")]
            ),
            PathBuf::from("/tmp/x.toml")
        );
        assert_eq!(
            path_for(None, &[("MYCO_CONFIG", "/env/y.toml")]),
            PathBuf::from("/env/y.toml")
        );
        assert!(path_for(None, &[]).ends_with(".myco/config.toml"));
    }

    #[test]
    fn harness_loader_gets_resolved_path_and_result_is_stored() {
        let cfg = Config::resolve_with(
            ConfigUserSettings {
                harness_config_path: Some(PathBuf::from("/tmp/h.toml")),
                ..Default::default()
            },
            env_of(&[]),
            false,
            |p| {
                assert_eq!(p, Path::new("/tmp/h.toml"));
                let mut file = parse_file_config_str(
                    "[models.m]\nprotocol = \"openai-responses\"\n\
                     base_url = \"https://h\"\nauth = \"none\"\ncontext_window = 1000\n",
                )?;
                file.attach_timeout_secs = 42;
                Ok(file)
            },
            || Ok(vec!["devbox".into()]),
            |_| Ok(BTreeMap::new()),
        )
        .unwrap();
        assert_eq!(cfg.harness.attach_timeout_secs, 42);
        assert_eq!(cfg.harness.remote_hosts.len(), 1);
        assert_eq!(cfg.harness.remote_hosts[0].name, "devbox");
    }

    #[test]
    fn load_errors_propagate() {
        let err = Config::resolve_with(
            ConfigUserSettings::default(),
            env_of(&[]),
            false,
            |_| Err("invalid config TOML".into()),
            || Ok(Vec::new()),
            |_| Ok(BTreeMap::new()),
        )
        .unwrap_err();
        assert!(err.contains("invalid config TOML"));

        let err = Config::resolve_with(
            ConfigUserSettings::default(),
            env_of(&[]),
            false,
            |_| parse_file_config_str(""),
            || Ok(Vec::new()),
            |_| Err("bad tokens.toml".into()),
        )
        .unwrap_err();
        assert!(err.contains("bad tokens.toml"));
    }
}
