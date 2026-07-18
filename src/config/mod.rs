//! Startup configuration resolved from the process environment.
//!
//! Runs once at application startup: [`Config::resolve`] takes optional
//! [`ConfigUserSettings`] overrides (CLI flags, embedder choices), reads the
//! process environment, and produces fully resolved settings — per-backend
//! credentials/base URLs, the harness/host-pool config loaded from disk
//! (`--config` → `$MYCO_CONFIG` → `~/.myco/config.toml`), the interactive
//! default model (`--model` → config file `model` → [`DEFAULT_MODEL`]), and
//! the color decision for stdout rendering. Downstream code checks the
//! resolved fields; nothing else reads these environment variables.
//!
//! Credential fallbacks: `ANTHROPIC_AUTH_TOKEN` / `ANTHROPIC_API_KEY` double
//! as the fallback credential for the OpenAI and xAI backends, and
//! `OPENAI_API_KEY` / `OPENAI_BASE_URL` back-fill the xAI backend (Grok is
//! served over the OpenAI Responses protocol).
//!
//! Out of scope, deliberately: `.env` loading (dotenvy runs in `main` before
//! resolution so this module sees its effect), `MYCO_HOME` (session storage
//! root; read by session code that also runs in `--mode host` workers where
//! no `Config` exists), and per-tool lookups like `MYCO_LYNX` (resolved at
//! tool-call time on whichever host runs the tool).

use std::io::IsTerminal;
use std::path::{Path, PathBuf};

use crate::generative_model::{self, BackendKind, Model};
use crate::harness::{FileConfig, HarnessConfig, load_file_config};

pub const ANTHROPIC_DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
pub const OPENAI_DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";
pub const XAI_DEFAULT_BASE_URL: &str = "https://api.x.ai/v1";

/// Interactive CLI model when neither `--model` nor the config file sets one.
pub const DEFAULT_MODEL: Model = Model::Grok45Build;

// ---------------------------------------------------------------------------
// Per-backend resolved credentials
// ---------------------------------------------------------------------------

/// Anthropic Messages credentials/endpoint. After [`Config::resolve`],
/// `base_url` is always populated (default applied); tokens stay `None` when
/// absent — model creation reports the error only when this backend is used.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AnthropicBackendConfig {
    /// `ANTHROPIC_BASE_URL`, default `https://api.anthropic.com`.
    pub base_url: Option<String>,
    /// `ANTHROPIC_AUTH_TOKEN` (gateway/OAuth Bearer token).
    pub auth_token: Option<String>,
    /// `ANTHROPIC_API_KEY` (`sk-ant-…`, sent as `x-api-key`).
    pub api_key: Option<String>,
}

impl AnthropicBackendConfig {
    /// Effective credential: auth token first, API key as fallback.
    pub fn credential(&self) -> Option<&str> {
        self.auth_token.as_deref().or(self.api_key.as_deref())
    }
}

/// OpenAI Responses credentials/endpoint (no served models yet; resolved for
/// a consistent surface). Same field semantics as [`AnthropicBackendConfig`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OpenAIBackendConfig {
    /// `OPENAI_BASE_URL`, default `https://api.openai.com/v1`.
    pub base_url: Option<String>,
    /// Fallback Bearer token copied from `ANTHROPIC_AUTH_TOKEN` / `ANTHROPIC_API_KEY`.
    pub auth_token: Option<String>,
    /// `OPENAI_API_KEY`.
    pub api_key: Option<String>,
}

impl OpenAIBackendConfig {
    /// Effective credential: native API key first, borrowed auth token as fallback.
    pub fn credential(&self) -> Option<&str> {
        self.api_key.as_deref().or(self.auth_token.as_deref())
    }
}

/// xAI (Grok over OpenAI Responses) credentials/endpoint. Same field
/// semantics as [`AnthropicBackendConfig`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct XAIBackendConfig {
    /// `XAI_API_BASE_URL` → `OPENAI_BASE_URL`, default `https://api.x.ai/v1`.
    pub base_url: Option<String>,
    /// Fallback Bearer token copied from `ANTHROPIC_AUTH_TOKEN` / `ANTHROPIC_API_KEY`.
    pub auth_token: Option<String>,
    /// `XAI_API_KEY` → `OPENAI_API_KEY`.
    pub api_key: Option<String>,
}

impl XAIBackendConfig {
    /// Effective credential: native API key first, borrowed auth token as fallback.
    pub fn credential(&self) -> Option<&str> {
        self.api_key.as_deref().or(self.auth_token.as_deref())
    }
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
/// tests). Any field set here wins over the environment.
#[derive(Debug, Clone, Default)]
pub struct ConfigUserSettings {
    pub anthropic: AnthropicBackendConfig,
    pub openai: OpenAIBackendConfig,
    pub xai: XAIBackendConfig,
    pub color: ColorMode,
    /// Override TTY detection (tests / embedders). `None` → detect from stdout.
    pub stdout_is_tty: Option<bool>,
    /// Harness config file override (CLI `--config`).
    /// `None` → `$MYCO_CONFIG` → `~/.myco/config.toml`.
    pub harness_config_path: Option<PathBuf>,
    /// Model id override (CLI `--model`), parsed during resolve.
    /// `None` → config file `model` → [`DEFAULT_MODEL`].
    pub model: Option<String>,
}

/// Fully resolved application configuration. Build once at startup with
/// [`Config::resolve`]; everything downstream reads these fields instead of
/// the environment.
#[derive(Debug, Clone)]
pub struct Config {
    pub anthropic: AnthropicBackendConfig,
    pub openai: OpenAIBackendConfig,
    pub xai: XAIBackendConfig,
    /// Whether stdout is a terminal (or the [`ConfigUserSettings`] override).
    pub stdout_is_tty: bool,
    /// Final color decision for stdout rendering ([`ColorMode`] + env overrides + TTY).
    pub colors_enabled: bool,
    /// Path the harness config was loaded from
    /// (override → `$MYCO_CONFIG` → `~/.myco/config.toml`).
    pub harness_config_path: PathBuf,
    /// Host-pool config from that path. Missing file → local-only default.
    pub harness: HarnessConfig,
    /// Interactive default model (override → config file `model` → [`DEFAULT_MODEL`]).
    pub model: Model,
}

impl Config {
    /// Resolve from the real process environment, stdout TTY state, and the
    /// config file. Errors carry the offending path.
    pub fn resolve(settings: ConfigUserSettings) -> Result<Self, String> {
        let stdout_is_tty = std::io::stdout().is_terminal();
        Self::resolve_with(
            settings,
            |k| std::env::var(k).ok(),
            stdout_is_tty,
            load_file_config,
        )
    }

    /// Resolution against an injected environment and config-file loader
    /// (tests, embedders). Empty environment values are treated as unset.
    pub fn resolve_with(
        settings: ConfigUserSettings,
        env: impl Fn(&str) -> Option<String>,
        stdout_is_tty: bool,
        load_file: impl FnOnce(&Path) -> Result<FileConfig, String>,
    ) -> Result<Self, String> {
        let env = |key: &str| env(key).filter(|v| !v.is_empty());
        let ConfigUserSettings {
            anthropic,
            openai,
            xai,
            color,
            stdout_is_tty: tty_override,
            harness_config_path,
            model: model_override,
        } = settings;

        let stdout_is_tty = tty_override.unwrap_or(stdout_is_tty);
        let harness_config_path = resolve_harness_config_path(harness_config_path, &env)?;
        let file = load_file(&harness_config_path)?;
        let model = match model_override {
            Some(s) => s.parse::<Model>()?,
            None => file.model.unwrap_or(DEFAULT_MODEL),
        };
        let harness = file
            .into_harness_config()
            .map_err(|e| format!("config {}: {e}", harness_config_path.display()))?;
        let (anthropic, openai, xai) = resolve_credentials(anthropic, openai, xai, &env);
        let colors_enabled = resolve_colors(color, &env, stdout_is_tty);

        Ok(Self {
            anthropic,
            openai,
            xai,
            stdout_is_tty,
            colors_enabled,
            harness_config_path,
            harness,
            model,
        })
    }

    /// Provider backend settings for `model`, credentials copied from this
    /// config. Model creation reports missing credentials.
    pub fn backend_config(&self, model: Model) -> generative_model::BackendConfig {
        protocol_backend_config(&self.anthropic, &self.xai, model)
    }
}

/// Backend defaults for `model` resolved from the environment alone — no
/// harness file I/O, infallible. The lazy path behind
/// [`generative_model::BackendConfig::default_for_model`] (subagents, compact
/// workers, tests); applications should build a full [`Config::resolve`] at
/// startup instead.
pub fn env_backend_config(model: Model) -> generative_model::BackendConfig {
    let env = |key: &str| std::env::var(key).ok().filter(|v| !v.is_empty());
    let (anthropic, _openai, xai) = resolve_credentials(
        AnthropicBackendConfig::default(),
        OpenAIBackendConfig::default(),
        XAIBackendConfig::default(),
        &env,
    );
    protocol_backend_config(&anthropic, &xai, model)
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

fn resolve_credentials(
    anthropic: AnthropicBackendConfig,
    openai: OpenAIBackendConfig,
    xai: XAIBackendConfig,
    env: &impl Fn(&str) -> Option<String>,
) -> (
    AnthropicBackendConfig,
    OpenAIBackendConfig,
    XAIBackendConfig,
) {
    // Anthropic credentials double as the fallback Bearer token for the
    // OpenAI / xAI backends (one env setup drives every gateway).
    let anthropic_fallback = env("ANTHROPIC_AUTH_TOKEN").or_else(|| env("ANTHROPIC_API_KEY"));

    let anthropic = AnthropicBackendConfig {
        base_url: anthropic
            .base_url
            .or_else(|| env("ANTHROPIC_BASE_URL"))
            .or_else(|| Some(ANTHROPIC_DEFAULT_BASE_URL.into())),
        auth_token: anthropic.auth_token.or_else(|| env("ANTHROPIC_AUTH_TOKEN")),
        api_key: anthropic.api_key.or_else(|| env("ANTHROPIC_API_KEY")),
    };

    let openai = OpenAIBackendConfig {
        base_url: openai
            .base_url
            .or_else(|| env("OPENAI_BASE_URL"))
            .or_else(|| Some(OPENAI_DEFAULT_BASE_URL.into())),
        auth_token: openai.auth_token.or_else(|| anthropic_fallback.clone()),
        api_key: openai.api_key.or_else(|| env("OPENAI_API_KEY")),
    };

    let xai = XAIBackendConfig {
        base_url: xai
            .base_url
            .or_else(|| env("XAI_API_BASE_URL"))
            .or_else(|| env("OPENAI_BASE_URL"))
            .or_else(|| Some(XAI_DEFAULT_BASE_URL.into())),
        auth_token: xai.auth_token.or_else(|| anthropic_fallback.clone()),
        api_key: xai
            .api_key
            .or_else(|| env("XAI_API_KEY"))
            .or_else(|| env("OPENAI_API_KEY")),
    };

    (anthropic, openai, xai)
}

/// Map resolved credentials onto the protocol backend settings for `model`.
fn protocol_backend_config(
    anthropic: &AnthropicBackendConfig,
    xai: &XAIBackendConfig,
    model: Model,
) -> generative_model::BackendConfig {
    match model.backend_kind() {
        BackendKind::AnthropicMessages => {
            generative_model::BackendConfig::Anthropic(generative_model::AnthropicBackendConfig {
                anthropic_base_url: anthropic
                    .base_url
                    .clone()
                    .unwrap_or_else(|| ANTHROPIC_DEFAULT_BASE_URL.into()),
                anthropic_auth_token: anthropic.credential().unwrap_or_default().to_string(),
                ..Default::default()
            })
        }
        // Every current OpenAI Responses model is xAI Grok, so this protocol
        // uses the xAI credentials (which already back-fill from the OpenAI
        // variables).
        BackendKind::OpenAIResponses => generative_model::BackendConfig::OpenAIResponses(
            generative_model::OpenAIResponsesBackendConfig {
                base_url: xai
                    .base_url
                    .clone()
                    .unwrap_or_else(|| XAI_DEFAULT_BASE_URL.into()),
                auth_token: xai.credential().unwrap_or_default().to_string(),
                ..Default::default()
            },
        ),
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

    fn env_of<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |key| {
            pairs
                .iter()
                .find(|(name, _)| *name == key)
                .map(|(_, value)| value.to_string())
        }
    }

    fn resolve_cfg(
        settings: ConfigUserSettings,
        env: impl Fn(&str) -> Option<String>,
        stdout_is_tty: bool,
    ) -> Config {
        Config::resolve_with(settings, env, stdout_is_tty, |_| Ok(FileConfig::default())).unwrap()
    }

    fn resolve(pairs: &[(&str, &str)]) -> Config {
        resolve_cfg(ConfigUserSettings::default(), env_of(pairs), false)
    }

    #[test]
    fn empty_env_yields_defaults_and_no_credentials() {
        let cfg = resolve(&[]);
        assert_eq!(
            cfg.anthropic.base_url.as_deref(),
            Some(ANTHROPIC_DEFAULT_BASE_URL)
        );
        assert_eq!(
            cfg.openai.base_url.as_deref(),
            Some(OPENAI_DEFAULT_BASE_URL)
        );
        assert_eq!(cfg.xai.base_url.as_deref(), Some(XAI_DEFAULT_BASE_URL));
        assert_eq!(cfg.anthropic.credential(), None);
        assert_eq!(cfg.openai.credential(), None);
        assert_eq!(cfg.xai.credential(), None);
    }

    #[test]
    fn empty_env_values_are_unset() {
        let cfg = resolve(&[("ANTHROPIC_AUTH_TOKEN", ""), ("ANTHROPIC_BASE_URL", "")]);
        assert_eq!(cfg.anthropic.auth_token, None);
        assert_eq!(
            cfg.anthropic.base_url.as_deref(),
            Some(ANTHROPIC_DEFAULT_BASE_URL)
        );
    }

    #[test]
    fn anthropic_auth_token_beats_api_key() {
        let cfg = resolve(&[
            ("ANTHROPIC_AUTH_TOKEN", "tok"),
            ("ANTHROPIC_API_KEY", "sk-ant-key"),
        ]);
        assert_eq!(cfg.anthropic.auth_token.as_deref(), Some("tok"));
        assert_eq!(cfg.anthropic.api_key.as_deref(), Some("sk-ant-key"));
        assert_eq!(cfg.anthropic.credential(), Some("tok"));
    }

    #[test]
    fn anthropic_api_key_alone_is_credential() {
        let cfg = resolve(&[("ANTHROPIC_API_KEY", "sk-ant-key")]);
        assert_eq!(cfg.anthropic.credential(), Some("sk-ant-key"));
    }

    #[test]
    fn xai_prefers_native_key_over_openai_key() {
        let cfg = resolve(&[("XAI_API_KEY", "xai"), ("OPENAI_API_KEY", "oai")]);
        assert_eq!(cfg.xai.credential(), Some("xai"));
        assert_eq!(cfg.openai.credential(), Some("oai"));
    }

    #[test]
    fn openai_key_back_fills_xai() {
        let cfg = resolve(&[("OPENAI_API_KEY", "oai")]);
        assert_eq!(cfg.xai.api_key.as_deref(), Some("oai"));
        assert_eq!(cfg.xai.credential(), Some("oai"));
    }

    #[test]
    fn anthropic_token_is_fallback_for_openai_and_xai() {
        let cfg = resolve(&[("ANTHROPIC_AUTH_TOKEN", "tok")]);
        assert_eq!(cfg.openai.auth_token.as_deref(), Some("tok"));
        assert_eq!(cfg.xai.auth_token.as_deref(), Some("tok"));
        assert_eq!(cfg.openai.credential(), Some("tok"));
        assert_eq!(cfg.xai.credential(), Some("tok"));
        // Native keys still win over the borrowed token.
        let cfg = resolve(&[("ANTHROPIC_AUTH_TOKEN", "tok"), ("XAI_API_KEY", "xai")]);
        assert_eq!(cfg.xai.credential(), Some("xai"));
    }

    #[test]
    fn xai_base_url_prefers_native_then_openai_then_default() {
        let cfg = resolve(&[
            ("XAI_API_BASE_URL", "https://x"),
            ("OPENAI_BASE_URL", "https://o"),
        ]);
        assert_eq!(cfg.xai.base_url.as_deref(), Some("https://x"));
        let cfg = resolve(&[("OPENAI_BASE_URL", "https://o")]);
        assert_eq!(cfg.xai.base_url.as_deref(), Some("https://o"));
        assert_eq!(
            resolve(&[]).xai.base_url.as_deref(),
            Some(XAI_DEFAULT_BASE_URL)
        );
    }

    #[test]
    fn user_settings_override_environment() {
        let settings = ConfigUserSettings {
            anthropic: AnthropicBackendConfig {
                auth_token: Some("explicit".into()),
                ..Default::default()
            },
            xai: XAIBackendConfig {
                base_url: Some("https://gw".into()),
                ..Default::default()
            },
            ..Default::default()
        };
        let cfg = resolve_cfg(
            settings,
            env_of(&[
                ("ANTHROPIC_AUTH_TOKEN", "env"),
                ("XAI_API_BASE_URL", "https://env"),
            ]),
            false,
        );
        assert_eq!(cfg.anthropic.credential(), Some("explicit"));
        assert_eq!(cfg.xai.base_url.as_deref(), Some("https://gw"));
    }

    #[test]
    fn backend_config_maps_models_to_provider_settings() {
        let cfg = resolve(&[("ANTHROPIC_AUTH_TOKEN", "tok"), ("XAI_API_KEY", "xai")]);
        match cfg.backend_config(Model::ClaudeHaiku45) {
            generative_model::BackendConfig::Anthropic(b) => {
                assert_eq!(b.anthropic_base_url, ANTHROPIC_DEFAULT_BASE_URL);
                assert_eq!(b.anthropic_auth_token, "tok");
            }
            other => panic!("expected Anthropic backend, got {other:?}"),
        }
        match cfg.backend_config(Model::Grok45Build) {
            generative_model::BackendConfig::OpenAIResponses(b) => {
                assert_eq!(b.base_url, XAI_DEFAULT_BASE_URL);
                assert_eq!(b.auth_token, "xai");
            }
            other => panic!("expected OpenAI Responses backend, got {other:?}"),
        }
    }

    #[test]
    fn color_mode_always_and_never_override_everything() {
        let env = env_of(&[("NO_COLOR", "1")]);
        let always = ConfigUserSettings {
            color: ColorMode::Always,
            ..Default::default()
        };
        assert!(resolve_cfg(always, &env, false).colors_enabled);
        let env = env_of(&[("CLICOLOR_FORCE", "1")]);
        let never = ConfigUserSettings {
            color: ColorMode::Never,
            ..Default::default()
        };
        assert!(!resolve_cfg(never, &env, true).colors_enabled);
    }

    #[test]
    fn auto_colors_follow_tty_and_env_overrides() {
        // TTY drives the default.
        assert!(resolve_cfg(Default::default(), env_of(&[]), true).colors_enabled);
        assert!(!resolve_cfg(Default::default(), env_of(&[]), false).colors_enabled);
        // NO_COLOR (non-empty) disables, even on a TTY, and beats CLICOLOR_FORCE.
        let env = env_of(&[("NO_COLOR", "1"), ("CLICOLOR_FORCE", "1")]);
        assert!(!resolve_cfg(Default::default(), &env, true).colors_enabled);
        // Empty NO_COLOR is unset.
        let env = env_of(&[("NO_COLOR", "")]);
        assert!(resolve_cfg(Default::default(), &env, true).colors_enabled);
        // CLICOLOR_FORCE forces colors without a TTY; "0" does not.
        let env = env_of(&[("CLICOLOR_FORCE", "1")]);
        assert!(resolve_cfg(Default::default(), &env, false).colors_enabled);
        let env = env_of(&[("CLICOLOR_FORCE", "0")]);
        assert!(!resolve_cfg(Default::default(), &env, false).colors_enabled);
        // Dumb terminals stay plain.
        let env = env_of(&[("TERM", "dumb")]);
        assert!(!resolve_cfg(Default::default(), &env, true).colors_enabled);
    }

    #[test]
    fn stdout_is_tty_setting_overrides_detection() {
        let settings = ConfigUserSettings {
            stdout_is_tty: Some(true),
            ..Default::default()
        };
        let cfg = resolve_cfg(settings, env_of(&[]), false);
        assert!(cfg.stdout_is_tty);
        assert!(cfg.colors_enabled);
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
        let settings = ConfigUserSettings {
            harness_config_path: Some(PathBuf::from("/tmp/x.toml")),
            ..Default::default()
        };
        let cfg = resolve_cfg(settings, env_of(&[("MYCO_CONFIG", "/env/y.toml")]), false);
        assert_eq!(cfg.harness_config_path, PathBuf::from("/tmp/x.toml"));

        let cfg = resolve_cfg(
            Default::default(),
            env_of(&[("MYCO_CONFIG", "/env/y.toml")]),
            false,
        );
        assert_eq!(cfg.harness_config_path, PathBuf::from("/env/y.toml"));

        let cfg = resolve_cfg(Default::default(), env_of(&[]), false);
        assert!(cfg.harness_config_path.ends_with(".myco/config.toml"));
    }

    #[test]
    fn harness_loader_gets_resolved_path_and_result_is_stored() {
        let settings = ConfigUserSettings {
            harness_config_path: Some(PathBuf::from("/tmp/h.toml")),
            ..Default::default()
        };
        let cfg = Config::resolve_with(settings, env_of(&[]), false, |p| {
            assert_eq!(p, Path::new("/tmp/h.toml"));
            Ok(FileConfig {
                attach_timeout_secs: 42,
                ..Default::default()
            })
        })
        .unwrap();
        assert_eq!(cfg.harness.attach_timeout_secs, 42);
    }

    #[test]
    fn harness_load_error_propagates() {
        let err = Config::resolve_with(ConfigUserSettings::default(), env_of(&[]), false, |_| {
            Err("invalid config TOML".into())
        })
        .unwrap_err();
        assert!(err.contains("invalid config TOML"));
    }

    #[test]
    fn model_override_beats_file_beats_default() {
        assert_eq!(resolve(&[]).model, DEFAULT_MODEL);

        let file_model = |_: &Path| {
            Ok(FileConfig {
                model: Some(Model::ClaudeOpus48),
                ..Default::default()
            })
        };
        let cfg = Config::resolve_with(
            ConfigUserSettings::default(),
            env_of(&[]),
            false,
            file_model,
        )
        .unwrap();
        assert_eq!(cfg.model, Model::ClaudeOpus48);

        // Override wins over the file; CLI aliases are accepted.
        let settings = ConfigUserSettings {
            model: Some("claude-haiku-4.5".into()),
            ..Default::default()
        };
        let cfg = Config::resolve_with(settings, env_of(&[]), false, file_model).unwrap();
        assert_eq!(cfg.model, Model::ClaudeHaiku45);
    }

    #[test]
    fn invalid_model_override_errors() {
        let settings = ConfigUserSettings {
            model: Some("gpt-99".into()),
            ..Default::default()
        };
        let err = Config::resolve_with(settings, env_of(&[]), false, |_| Ok(FileConfig::default()))
            .unwrap_err();
        assert!(err.contains("Unknown model"), "{err}");
    }
}
