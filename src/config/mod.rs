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
//! An entry's `auth` value is either the credential itself (a bare string) or
//! a source table: `{ source = "env", var_name = "…" }` reads the process
//! environment, `{ source = "file", path = "…" }` reads a file's trimmed
//! contents (keeps secrets out of a shareable config), `{ source = "none" }`
//! (or omitting `auth`) sends no auth header. A credential that fails to
//! *look up* (unset variable, unreadable file) is **not** a resolve error: it
//! is reported when the model is actually used ([`ModelCatalog::get`]).
//!
//! Out of scope, deliberately: `.env` loading (dotenvy runs in `main` before
//! resolution so env auth sources see its effect), `MYCO_HOME` (session storage
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
use crate::harness::{
    AuthEntry, FileConfig, HarnessConfig, load_file_config, load_ssh_host_aliases,
};

/// Default per-generate output token cap when a model entry sets none.
pub const DEFAULT_MAX_OUTPUT_TOKENS: usize = 8192;

// ---------------------------------------------------------------------------
// Auth resolution
// ---------------------------------------------------------------------------

/// Read an `auth = { source = "file", … }` credential: trimmed contents,
/// leading `~/` expanded to the home directory.
pub fn read_auth_file(path: &Path) -> Result<String, String> {
    let expanded: PathBuf = match path.strip_prefix("~") {
        Ok(rest) => dirs::home_dir()
            .ok_or_else(|| "could not resolve home directory".to_string())?
            .join(rest),
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
// Word wrap
// ---------------------------------------------------------------------------

/// Wrap cap for `--wrap auto` (narrower terminals win).
pub const DEFAULT_WRAP_WIDTH: usize = 80;

/// Word-wrap choice for stdout rendering (CLI `--wrap`). Every mode is a
/// *cap*: the effective width is `min(cap, measured terminal width)`,
/// re-measured at render time so resizes reflow.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum WrapMode {
    /// Cap at [`DEFAULT_WRAP_WIDTH`].
    #[default]
    Auto,
    Off,
    /// Cap at a custom column count.
    Columns(usize),
}

impl std::fmt::Display for WrapMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WrapMode::Auto => f.write_str("auto"),
            WrapMode::Off => f.write_str("off"),
            WrapMode::Columns(n) => write!(f, "{n}"),
        }
    }
}

impl std::str::FromStr for WrapMode {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.trim().to_ascii_lowercase();
        match s.as_str() {
            "auto" => Ok(WrapMode::Auto),
            "off" | "never" | "none" => Ok(WrapMode::Off),
            _ => match s.parse::<usize>() {
                Ok(n) if n >= 20 => Ok(WrapMode::Columns(n)),
                Ok(n) => Err(format!("wrap width {n} is too narrow (minimum 20)")),
                Err(_) => Err(format!(
                    "unknown wrap mode {s:?}; expected auto|off|<columns>"
                )),
            },
        }
    }
}

/// The configured wrap cap. Wrap is TTY-only, like colors — piped output is
/// never wrapped. Terminal-width measurement happens at render time, not here.
fn resolve_wrap(mode: WrapMode, stdout_is_tty: bool) -> Option<usize> {
    match mode {
        WrapMode::Off => None,
        WrapMode::Columns(n) => stdout_is_tty.then_some(n),
        WrapMode::Auto => stdout_is_tty.then_some(DEFAULT_WRAP_WIDTH),
    }
}

/// Terminal (columns, rows) of stdout, when stdout is a terminal.
#[cfg(unix)]
pub fn detect_terminal_size() -> Option<(usize, usize)> {
    let mut ws = libc::winsize {
        ws_row: 0,
        ws_col: 0,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: TIOCGWINSZ only writes the winsize out-param on success.
    let ok = unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut ws) } == 0;
    (ok && ws.ws_col > 0).then_some((ws.ws_col as usize, ws.ws_row as usize))
}

#[cfg(not(unix))]
pub fn detect_terminal_size() -> Option<(usize, usize)> {
    None
}

// ---------------------------------------------------------------------------
// Config resolution
// ---------------------------------------------------------------------------

/// Startup-time overrides supplied by the embedding application (CLI flags,
/// tests). Any field set here wins over file/environment resolution.
#[derive(Debug, Clone, Default)]
pub struct ConfigUserSettings {
    pub color: ColorMode,
    pub wrap: WrapMode,
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
    /// Wrap cap ([`WrapMode`] + TTY), `None` = off. The effective width is
    /// `min(cap, terminal width)`, measured by the renderer per prompt so
    /// terminal resizes reflow.
    pub wrap_max: Option<usize>,
    /// Path the config file was loaded from
    /// (override → `$MYCO_CONFIG` → `~/.myco/config.toml`).
    pub harness_config_path: PathBuf,
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
    /// config file, auth files, and `~/.ssh/config` host aliases. Errors
    /// carry the offending path / entry name.
    pub fn resolve(settings: ConfigUserSettings) -> Result<Self, String> {
        let stdout_is_tty = std::io::stdout().is_terminal();
        Self::resolve_with(
            settings,
            |k| std::env::var(k).ok(),
            stdout_is_tty,
            load_file_config,
            load_ssh_host_aliases,
            read_auth_file,
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
        read_auth_file: impl Fn(&Path) -> Result<String, String>,
    ) -> Result<Self, String> {
        let env = |key: &str| env(key).filter(|v| !v.is_empty());
        let ConfigUserSettings {
            color,
            wrap,
            stdout_is_tty: tty_override,
            harness_config_path,
            model: model_override,
        } = settings;

        let stdout_is_tty = tty_override.unwrap_or(stdout_is_tty);
        let harness_config_path = resolve_harness_config_path(harness_config_path, &env)?;
        let file = load_file(&harness_config_path)?;

        let models = resolve_catalog(&file, &env, &read_auth_file)?;
        let model = resolve_default_model(model_override, file.model.clone(), &models)?;

        let mut harness = file.into_harness_config(ssh_aliases()?);
        harness.models = models.clone();
        let colors_enabled = resolve_colors(color, &env, stdout_is_tty);
        let wrap_max = resolve_wrap(wrap, stdout_is_tty);

        Ok(Self {
            stdout_is_tty,
            colors_enabled,
            wrap_max,
            harness_config_path,
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
/// protocol/base_url, incompatible thinking mode). Credential *lookups* that
/// fail (unset env var, unreadable file) are soft: recorded per-entry and
/// reported when the model is actually used.
fn resolve_catalog(
    file: &FileConfig,
    env: &impl Fn(&str) -> Option<String>,
    read_auth_file: &impl Fn(&Path) -> Result<String, String>,
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
                 (anthropic-messages: adaptive|budget|none; openai-responses: effort|none)"
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
            Some(AuthEntry::File { path }) => match read_auth_file(Path::new(&path)) {
                Ok(v) => (v, None),
                Err(e) => (String::new(), Some(format!("model `{key}`: auth file {e}"))),
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
auth = { source = "env", var_name = "OPENROUTER_API_KEY" }

[gateways.anthropic]
protocol = "anthropic-messages"
base_url = "https://api.anthropic.com"
auth = { source = "env", var_name = "ANTHROPIC_API_KEY" }

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
thinking = "budget"
api_id = "claude-haiku-4-5"
context_window = 200_000
"#;

    fn resolve_toml(
        toml_text: &'static str,
        settings: ConfigUserSettings,
        env: impl Fn(&str) -> Option<String>,
    ) -> Result<Config, String> {
        resolve_toml_with_files(toml_text, settings, env, |p| {
            Err(format!("{}: no such file", p.display()))
        })
    }

    fn resolve_toml_with_files(
        toml_text: &'static str,
        settings: ConfigUserSettings,
        env: impl Fn(&str) -> Option<String>,
        read_auth_file: impl Fn(&Path) -> Result<String, String>,
    ) -> Result<Config, String> {
        Config::resolve_with(
            settings,
            env,
            false,
            move |_| parse_file_config_str(toml_text),
            || Ok(Vec::new()),
            read_auth_file,
        )
    }

    fn resolve_catalog_cfg(env_pairs: &[(&str, &str)]) -> Config {
        resolve_toml(CATALOG, ConfigUserSettings::default(), env_of(env_pairs)).unwrap()
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
    fn literal_auth_string_is_the_token() {
        let toml_text = r#"
[models.proxy]
protocol = "openai-responses"
base_url = "https://proxy.corp/v1"
auth = "sk-inline-secret"
context_window = 100_000
"#;
        let cfg = resolve_toml(toml_text, ConfigUserSettings::default(), env_of(&[])).unwrap();
        match &cfg.models.get("proxy").unwrap().backend {
            BackendConfig::OpenAIResponses(b) => assert_eq!(b.auth_token, "sk-inline-secret"),
            other => panic!("unexpected backend {other:?}"),
        }
    }

    #[test]
    fn file_auth_reads_trimmed_contents_and_read_failure_defers() {
        let toml_text = r#"
[models.proxy]
protocol = "openai-responses"
base_url = "https://proxy.corp/v1"
auth = { source = "file", path = "~/.secrets/corp.token" }
context_window = 100_000
"#;
        let cfg =
            resolve_toml_with_files(toml_text, ConfigUserSettings::default(), env_of(&[]), |p| {
                assert_eq!(p, Path::new("~/.secrets/corp.token"));
                Ok("sekrit".into())
            })
            .unwrap();
        match &cfg.models.get("proxy").unwrap().backend {
            BackendConfig::OpenAIResponses(b) => assert_eq!(b.auth_token, "sekrit"),
            other => panic!("unexpected backend {other:?}"),
        }

        // Unreadable file: resolve succeeds, use-time error names the path.
        let cfg = resolve_toml(toml_text, ConfigUserSettings::default(), env_of(&[])).unwrap();
        let err = cfg.models.get("proxy").unwrap_err();
        assert!(err.contains("auth file"), "{err}");
        assert!(err.contains("corp.token"), "{err}");
    }

    #[test]
    fn absent_auth_and_source_none_send_no_credential() {
        let toml_text = r#"
model = "inherits"

[gateways.g]
protocol = "openai-responses"
base_url = "https://h"
auth = "sk-gateway-token"

[models.inherits]
gateway = "g"
context_window = 1000

[models.opts-out]
gateway = "g"
auth = { source = "none" }
context_window = 1000
"#;
        let cfg = resolve_toml(toml_text, ConfigUserSettings::default(), env_of(&[])).unwrap();
        match &cfg.models.get("inherits").unwrap().backend {
            BackendConfig::OpenAIResponses(b) => assert_eq!(b.auth_token, "sk-gateway-token"),
            other => panic!("unexpected backend {other:?}"),
        }
        match &cfg.models.get("opts-out").unwrap().backend {
            BackendConfig::OpenAIResponses(b) => assert_eq!(b.auth_token, ""),
            other => panic!("unexpected backend {other:?}"),
        }
    }

    #[test]
    fn config_shape_errors_name_the_model() {
        let unknown_gateway = r#"
[models.x]
gateway = "nope"
context_window = 1000
"#;
        let err =
            resolve_toml(unknown_gateway, ConfigUserSettings::default(), env_of(&[])).unwrap_err();
        assert!(err.contains("model `x`"), "{err}");
        assert!(err.contains("unknown gateway `nope`"), "{err}");

        let no_protocol = r#"
[models.x]
base_url = "https://h"
auth = "none"
context_window = 1000
"#;
        let err =
            resolve_toml(no_protocol, ConfigUserSettings::default(), env_of(&[])).unwrap_err();
        assert!(err.contains("no protocol"), "{err}");

        let bad_auth = r#"
[models.x]
protocol = "openai-responses"
base_url = "https://h"
auth = { source = "keychain" }
context_window = 1000
"#;
        let err = resolve_toml(bad_auth, ConfigUserSettings::default(), env_of(&[])).unwrap_err();
        assert!(err.contains("unknown source \"keychain\""), "{err}");
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
        let err = resolve_toml(toml_text, ConfigUserSettings::default(), env_of(&[])).unwrap_err();
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
context_window = 1000
"#;
        let cfg = resolve_toml(sole, ConfigUserSettings::default(), env_of(&[])).unwrap();
        assert_eq!(cfg.model, "only");
    }

    #[test]
    fn missing_model_selection_errors_are_actionable() {
        // No models at all.
        let err = resolve_toml("", ConfigUserSettings::default(), env_of(&[])).unwrap_err();
        assert!(err.contains("no models configured"), "{err}");
        assert!(err.contains("[models]"), "{err}");

        // Multiple models, nothing selected.
        let two = r#"
[models.a]
protocol = "openai-responses"
base_url = "https://h"
context_window = 1000

[models.b]
protocol = "openai-responses"
base_url = "https://h"
context_window = 1000
"#;
        let err = resolve_toml(two, ConfigUserSettings::default(), env_of(&[])).unwrap_err();
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
        )
        .unwrap_err();
        assert!(err.contains("unknown model \"c\""), "{err}");
        assert!(err.contains("[a, b]"), "{err}");
    }

    #[test]
    fn example_config_resolves_end_to_end() {
        let cfg = Config::resolve_with(
            ConfigUserSettings::default(),
            env_of(&[("XAI_API_KEY", "xai")]),
            false,
            |_| parse_file_config_str(&crate::harness::example_config_toml()),
            || Ok(Vec::new()),
            |_| Err("no files".into()),
        )
        .unwrap();
        assert_eq!(cfg.model, "grok-4.5-build");
        assert!(cfg.models.get("grok-4.5-build").is_ok());
        // Anthropic entries resolve but defer their missing credential.
        let err = cfg.models.get("claude-opus-4-8").unwrap_err();
        assert!(err.contains("ANTHROPIC_API_KEY"), "{err}");
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
    fn wrap_resolves_to_a_tty_only_cap() {
        assert_eq!(resolve_wrap(WrapMode::Auto, true), Some(80));
        assert_eq!(resolve_wrap(WrapMode::Auto, false), None);
        assert_eq!(resolve_wrap(WrapMode::Columns(100), true), Some(100));
        // Wrap is TTY-only, like colors — a custom cap does not force it on pipes.
        assert_eq!(resolve_wrap(WrapMode::Columns(100), false), None);
        assert_eq!(resolve_wrap(WrapMode::Off, true), None);
    }

    #[test]
    fn wrap_resolution_flows_into_config() {
        let cfg = resolve_toml(
            CATALOG,
            ConfigUserSettings {
                stdout_is_tty: Some(true),
                ..Default::default()
            },
            env_of(&[]),
        )
        .unwrap();
        assert_eq!(cfg.wrap_max, Some(80));
        // Piped (the default `false` in resolve_toml): wrap stays off.
        assert_eq!(resolve_catalog_cfg(&[]).wrap_max, None);
    }

    #[test]
    fn wrap_mode_parses() {
        assert_eq!("auto".parse::<WrapMode>().unwrap(), WrapMode::Auto);
        assert_eq!("OFF".parse::<WrapMode>().unwrap(), WrapMode::Off);
        assert_eq!("100".parse::<WrapMode>().unwrap(), WrapMode::Columns(100));
        assert!("10".parse::<WrapMode>().is_err());
        assert!("wide".parse::<WrapMode>().is_err());
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
                     base_url = \"https://h\"\ncontext_window = 1000\n",
                )?;
                file.attach_timeout_secs = 42;
                Ok(file)
            },
            || Ok(vec!["devbox".into()]),
            |_| Err("no files".into()),
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
            |_| Err("no files".into()),
        )
        .unwrap_err();
        assert!(err.contains("invalid config TOML"));
    }
}
