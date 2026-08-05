//! Startup configuration resolved from config.toml and the process environment.
//!
//! Runs once at application startup: [`Config::resolve`] takes optional
//! [`ConfigUserSettings`] overrides (CLI flags, embedder choices), loads the
//! config file (`--config` → `$MYCO_CONFIG` → `~/.myco/config.toml`), and
//! produces fully resolved settings — the **model catalog**, the host pool
//! (remote hosts from `~/.ssh/config` `Host` aliases), the default model key
//! (`--model` → config file `model` → sole catalog entry), the prelude size cap
//! (`max_prelude_bytes`), and the color decision for stdout rendering. Downstream
//! code reads the resolved fields; nothing else reads these environment
//! variables or files.
//!
//! ## Model catalog
//!
//! Myco ships **no built-in models or gateways**: `[gateways.*]` and
//! `[models.*]` in config.toml are the entire catalog (example in the
//! `overview` manual article). A model entry names a gateway (or
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
use std::path::{Path, PathBuf};

use myco_models::{
    AnthropicBackendConfig, BackendConfig, CatalogModel, Effort, ModelCatalog, ModelSpec,
    OpenAIBackendConfig, Protocol, RetryPolicy, ThinkingMode,
};

pub mod file;
pub mod harness;
pub mod roster;
pub use file::{
    AuthEntry, FileConfig, GatewayEntry, ModelEntry, RetryEntry, load_file_config,
    parse_file_config_str,
};
pub use harness::{HarnessConfig, HostConfig, load_ssh_host_aliases};
pub use roster::{
    EXAMPLE_SERVER_TOML, FileRoster, Roster, RosterUser, load_file_roster, parse_file_roster_str,
};

/// Default per-generate output token cap when a model entry sets none.
pub const DEFAULT_MAX_OUTPUT_TOKENS: usize = 8192;

/// Default per-remote connect timeout (seconds) when the config file sets none.
pub const DEFAULT_ATTACH_TIMEOUT_SECS: u64 = 10;

/// Default per-image cap when a model entry sets no `max_image_base64_bytes`
/// (matches the Anthropic API's 5 MB per-image cap; a clear local error beats
/// a confusing provider 400).
///
/// Measured on the **base64 payload**, not the file on disk: images travel as
/// `data:` URLs, and base64 inflates by 4/3, so a 5 MiB file is already a
/// 6.7 MiB upload. Checking the file size instead understates every image by a
/// third and lets rejects through to the provider.
///
/// This is the only place the fallback exists: resolution turns it into a
/// concrete `ModelSpec::max_image_base64_bytes`, and everything downstream
/// takes that resolved value from its caller.
pub const DEFAULT_MAX_IMAGE_BASE64_BYTES: u64 = 5 * 1024 * 1024;

/// Default consecutive `max_tokens` resumes per turn when a model entry sets
/// no `max_truncated_resumes`: enough to triple the effective output budget
/// without letting a looping model run unattended forever.
pub const DEFAULT_MAX_TRUNCATED_RESUMES: u32 = 3;

/// Default share of the context window at which a turn's end queues an
/// auto-compaction, when a model entry sets no `auto_compact_at`. Resolution
/// turns the fraction into a concrete `ModelSpec::auto_compact_at_tokens`;
/// the fraction never leaves this crate.
pub const DEFAULT_AUTO_COMPACT_FRACTION: f64 = 0.85;

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
// Config resolution
// ---------------------------------------------------------------------------

/// Startup-time overrides supplied by the embedding application (CLI flags,
/// tests). Any field set here wins over file/environment resolution.
#[derive(Debug, Clone, Default)]
pub struct ConfigUserSettings {
    /// Config file override (CLI `--config`).
    /// `None` → `$MYCO_CONFIG` → `~/.myco/config.toml`.
    pub config_path: Option<PathBuf>,
    /// Roster file override.
    /// `None` → `$MYCO_SERVER_CONFIG` → `~/.myco/v2/server.toml`.
    pub roster_path: Option<PathBuf>,
    /// Model key override (CLI `--model`).
    /// `None` → config file `model` → sole catalog entry.
    pub model: Option<String>,
}

/// Fully resolved application configuration. Build once at startup with
/// [`Config::resolve`]; everything downstream reads these fields instead of
/// the environment or config files.
#[derive(Debug, Clone)]
pub struct Config {
    /// Path the config file was loaded from
    /// (override → `$MYCO_CONFIG` → `~/.myco/config.toml`).
    pub config_path: PathBuf,
    /// Registered users, and which one this process runs as. Required: myco
    /// attributes every session entry, and will not invent an author.
    pub roster: Roster,
    /// Host pool: knobs from the config file (missing file → defaults) plus
    /// remote hosts from `~/.ssh/config` `Host` aliases.
    pub harness: HarnessConfig,
    /// Model catalog resolved from `[gateways]` / `[models]`.
    pub models: ModelCatalog,
    /// Default model key (`--model` → config file `model` → sole entry).
    /// Always present in the catalog; credential presence is still checked at
    /// use time via [`ModelCatalog::get`].
    pub model: String,
    /// Cap on the rendered prelude in every agent system prompt (config file
    /// `max_prelude_bytes`, default [`myco_prompts::DEFAULT_MAX_PRELUDE_BYTES`]).
    /// Enforced rather than trimmed to: the `prelude` tool refuses an edit
    /// that would cross it, and startup exits against a prelude already over
    /// it (`fatal_startup_check`).
    pub max_prelude_bytes: usize,
}

impl Config {
    /// Resolve from the real process environment, stdout TTY state, the
    /// config file, auth files, and `~/.ssh/config` host aliases. Errors
    /// carry the offending path / entry name.
    pub fn resolve(settings: ConfigUserSettings) -> Result<Self, String> {
        Self::resolve_with(
            settings,
            |k| std::env::var(k).ok(),
            load_file_config,
            roster::load_file_roster,
            load_ssh_host_aliases,
            read_auth_file,
        )
    }

    /// Resolution against injected environment / loaders (tests, embedders).
    /// Empty environment values are treated as unset.
    pub fn resolve_with(
        settings: ConfigUserSettings,
        env: impl Fn(&str) -> Option<String>,
        load_file: impl FnOnce(&Path, bool) -> Result<FileConfig, String>,
        load_roster: impl FnOnce(&Path) -> Result<FileRoster, String>,
        ssh_aliases: impl FnOnce() -> Result<Vec<String>, String>,
        read_auth_file: impl Fn(&Path) -> Result<String, String>,
    ) -> Result<Self, String> {
        let env = |key: &str| env(key).filter(|v| !v.is_empty());
        let ConfigUserSettings {
            config_path,
            roster_path,
            model: model_override,
        } = settings;

        let (config_path, named_by_user) = resolve_config_path(config_path, &env)?;
        // Every config error names its file: by the time one surfaces, "which
        // file was that" is the first question. Roster errors stay unwrapped —
        // they already name server.toml, a different file.
        let with_path = |e: String| {
            let path = config_path.display().to_string();
            match e.contains(&path) {
                true => e,
                false => format!("{e}\nconfig: {path}"),
            }
        };
        let file = load_file(&config_path, named_by_user).map_err(&with_path)?;

        // Identity before anything else: a session written by an unknown
        // author is worse than a server that refused to start.
        let roster_path = roster::resolve_roster_path(roster_path, &env)?;
        let roster = Roster::resolve(roster_path.clone(), load_roster(&roster_path)?, &env)?;

        let models = resolve_catalog(&file, &env, &read_auth_file).map_err(&with_path)?;
        // An empty catalog is onboarding, not selection: fail here, with a
        // pasteable example, before "no model selected" can mislead.
        if models.is_empty() {
            return Err(with_path(format!(
                "no models configured — myco ships no built-in models.\n\n\
                 Add at least one `[models]` entry to the config file below (create it if \
                 it does not exist), for example:\n\n{EXAMPLE_CONFIG_TOML}\n\
                 Full format (other protocols, auth sources, knobs): `myco --help overview`."
            )));
        }
        let model = resolve_default_model(model_override, file.model.clone(), &models)
            .map_err(&with_path)?;

        let max_prelude_bytes = file
            .max_prelude_bytes
            .unwrap_or(myco_prompts::DEFAULT_MAX_PRELUDE_BYTES);
        // Host workers enforce the image cap where the file is read, so they
        // are spawned with the cap of the model this process will run — fixed
        // for the process, since the model is chosen once at startup.
        let harness = HarnessConfig::from_ssh_aliases(
            ssh_aliases()?,
            file.attach_timeout_secs
                .unwrap_or(DEFAULT_ATTACH_TIMEOUT_SECS),
            models
                .spec(&model)
                .map(|s| s.max_image_base64_bytes)
                .unwrap_or(DEFAULT_MAX_IMAGE_BASE64_BYTES),
        );
        Ok(Self {
            config_path,
            roster,
            harness,
            models,
            model,
            max_prelude_bytes,
        })
    }
}

/// A minimal working catalog, quoted into the empty-catalog startup error so
/// a fresh install has something to paste and edit (the roster's
/// [`EXAMPLE_SERVER_TOML`] is the same idea for identity).
pub const EXAMPLE_CONFIG_TOML: &str = r#"    model = "sonnet"

    [gateways.anthropic]
    protocol = "anthropic-messages"
    base_url = "https://api.anthropic.com"
    auth = { source = "env", var_name = "ANTHROPIC_API_KEY" }

    [models.sonnet]
    gateway = "anthropic"
    api_id = "claude-sonnet-4-5"
    context_window = 200_000
"#;

/// `--config` override → `$MYCO_CONFIG` → `~/.myco/v2/config.toml`. The bool
/// is whether the user *named* the path (the first two arms): a named file
/// that does not exist is an error, the missing home default is not.
fn resolve_config_path(
    override_path: Option<PathBuf>,
    env: &impl Fn(&str) -> Option<String>,
) -> Result<(PathBuf, bool), String> {
    if let Some(p) = override_path {
        return Ok((p, true));
    }
    if let Some(p) = env("MYCO_CONFIG") {
        return Ok((PathBuf::from(p), true));
    }
    Ok((myco_core::data_root()?.join("config.toml"), false))
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
            Some(AuthEntry::File { path }) => match read_auth_file(Path::new(&path)) {
                Ok(v) => (v, None),
                Err(e) => (String::new(), Some(format!("model `{key}`: auth file {e}"))),
            },
        };

        // The fraction becomes a token count here, so downstream compares
        // plain numbers and a bad value is a startup error rather than a
        // surprise hours into an unattended run. 1.0 would only fire with the
        // window already full, which is too late to be worth allowing.
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

    Ok(ModelCatalog::new(entries))
}

/// Overlay a config `[retry]` table onto the built-in policy, field by field —
/// setting one knob does not silently reset the others.
///
/// `max_attempts` is clamped to at least 1: `0` reads as "do not retry", not
/// "never send the request".
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

/// `--model` → config file `model` → sole catalog entry. The chosen key must
/// exist in the catalog (credentials are checked later, at use). The catalog
/// is never empty here — `resolve_with` turned that into the onboarding error
/// before selection could run.
fn resolve_default_model(
    override_key: Option<String>,
    file_key: Option<String>,
    catalog: &ModelCatalog,
) -> Result<String, String> {
    if let Some(key) = override_key.or(file_key) {
        if !catalog.contains(&key) {
            return Err(format!(
                "unknown model {key:?}; configured models: [{}]",
                catalog.keys().join(", ")
            ));
        }
        return Ok(key);
    }
    match catalog.keys().as_slice() {
        [only] => Ok(only.to_string()),
        keys => Err(format!(
            "no model selected — set `model = \"<key>\"` in config.toml or pass --model \
             (configured: [{}])",
            keys.join(", ")
        )),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use file::model_toml;

    /// An empty catalog fails at resolve with a pasteable example — the
    /// onboarding moment, not a wall.
    #[test]
    fn empty_catalog_is_rejected_with_a_usable_example() {
        let err = resolve_toml("", ConfigUserSettings::default(), env_of(&[])).unwrap_err();
        for needle in [
            "no models configured",
            "[gateways.",
            "[models.",
            "context_window",
            "myco --help overview",
            "config:",
        ] {
            assert!(err.contains(needle), "missing {needle:?} in: {err}");
        }
    }

    /// Every config error names the file it came from: by the time one
    /// surfaces, "which file was that" is the first question.
    #[test]
    fn config_errors_name_the_file_they_came_from() {
        let path = PathBuf::from("/etc/myco/other.toml");
        // A shape error and a selection error, both wrapped.
        for toml_text in ["modle = \"x\"", &model_toml("a", &[])] {
            let toml_text = format!("{toml_text}\n{}", model_toml("b", &[]));
            let err = Config::resolve_with(
                ConfigUserSettings {
                    config_path: Some(path.clone()),
                    ..Default::default()
                },
                env_of(&[("USER", "tester")]),
                move |_, _| parse_file_config_str(&toml_text),
                test_roster,
                || Ok(Vec::new()),
                |_| Err("no files".into()),
            )
            .unwrap_err();
            assert!(err.contains("/etc/myco/other.toml"), "{err}");
        }
    }

    /// `""` satisfies "base_url is set" but fails at the provider on every
    /// turn; catch it at resolve instead.
    #[test]
    fn empty_base_url_is_a_resolve_error() {
        let toml_text = r#"
[models.x]
protocol = "openai-responses"
base_url = "   "
context_window = 1000
"#;
        let err = resolve_toml(toml_text, ConfigUserSettings::default(), env_of(&[])).unwrap_err();
        assert!(err.contains("model `x`"), "{err}");
        assert!(err.contains("base_url is empty"), "{err}");
    }

    /// The config fraction becomes a concrete token threshold at resolve, and
    /// an unset entry keeps the built-in default share — the assertion that
    /// stops the default behavior regressing silently.
    #[test]
    fn auto_compact_fraction_becomes_a_token_threshold() {
        let toml_text = r#"
model = "tuned"

[models.tuned]
protocol = "openai-responses"
base_url = "https://h"
context_window = 200000
auto_compact_at = 0.8

[models.stock]
protocol = "openai-responses"
base_url = "https://h"
context_window = 100000
"#;
        let cfg = resolve_toml(toml_text, ConfigUserSettings::default(), env_of(&[])).unwrap();
        assert_eq!(
            cfg.models.get("tuned").unwrap().spec.auto_compact_at_tokens,
            160_000
        );
        assert_eq!(
            cfg.models.get("stock").unwrap().spec.auto_compact_at_tokens,
            (100_000f64 * DEFAULT_AUTO_COMPACT_FRACTION) as u64
        );
    }

    /// A model entry's `effort` lands on its backend, and an unset entry gets
    /// the built-in default — resolution decides, so runtimes never invent an
    /// effort of their own.
    #[test]
    fn effort_resolves_onto_the_backend_with_a_default() {
        let toml_text = r#"
model = "gentle"

[models.gentle]
protocol = "openai-responses"
base_url = "https://h"
context_window = 1000
effort = "low"

[models.stock]
protocol = "openai-responses"
base_url = "https://h"
context_window = 1000
"#;
        let cfg = resolve_toml(toml_text, ConfigUserSettings::default(), env_of(&[])).unwrap();
        let effort_of = |key: &str| match &cfg.models.get(key).unwrap().backend {
            BackendConfig::OpenAIResponses(b) => b.effort,
            other => panic!("unexpected backend {other:?}"),
        };
        assert_eq!(effort_of("gentle"), Some(Effort::Low));
        assert_eq!(effort_of("stock"), Some(Effort::DEFAULT));
    }

    /// Out-of-range fractions are a startup error, not a surprise hours into
    /// an unattended run. 1.0 would only fire with the window already full.
    #[test]
    fn auto_compact_fraction_out_of_range_is_a_resolve_error() {
        for bad in ["0.0", "1.0", "1.5", "-0.2"] {
            let toml_text = format!(
                "[models.x]\nprotocol = \"openai-responses\"\nbase_url = \"https://h\"\n\
                 context_window = 1000\nauto_compact_at = {bad}\n"
            );
            let err =
                resolve_toml(toml_text, ConfigUserSettings::default(), env_of(&[])).expect_err(bad);
            assert!(err.contains("auto_compact_at"), "{err}");
            assert!(err.contains("model `x`"), "{err}");
        }
    }

    /// A gateway's `[retry]` overlays the defaults per field, and a model's
    /// own `[retry]` replaces the gateway's wholesale — an unset field on the
    /// model's table falls back to the default rather than to the gateway.
    #[test]
    fn retry_overlays_defaults_and_a_model_table_replaces_the_gateways() {
        let toml_text = r#"
model = "inherits"

[gateways.g]
protocol = "openai-responses"
base_url = "https://h"

[gateways.g.retry]
max_attempts = 7
initial_backoff_ms = 250

[models.inherits]
gateway = "g"
context_window = 1000

[models.overrides]
gateway = "g"
context_window = 1000

[models.overrides.retry]
max_attempts = 2

[models.bare]
protocol = "openai-responses"
base_url = "https://h"
context_window = 1000
"#;
        let cfg = resolve_toml(toml_text, ConfigUserSettings::default(), env_of(&[])).unwrap();
        let retry_of = |key: &str| match &cfg.models.get(key).unwrap().backend {
            BackendConfig::OpenAIResponses(b) => b.retry,
            other => panic!("unexpected backend {other:?}"),
        };
        let default = RetryPolicy::default();

        // Gateway values apply; fields it left unset keep their defaults.
        let inherits = retry_of("inherits");
        assert_eq!(inherits.max_attempts, 7);
        assert_eq!(
            inherits.initial_backoff,
            std::time::Duration::from_millis(250)
        );
        assert_eq!(inherits.max_backoff, default.max_backoff);
        assert_eq!(inherits.backoff_multiplier, default.backoff_multiplier);

        // The model's own table wins wholesale: the gateway's 250ms is gone,
        // and the unset field falls back to the default, not to the gateway.
        let overrides = retry_of("overrides");
        assert_eq!(overrides.max_attempts, 2);
        assert_eq!(overrides.initial_backoff, default.initial_backoff);

        // Configured nowhere -> the built-in policy.
        assert_eq!(retry_of("bare"), default);
    }

    /// `max_attempts = 0` reads as "do not retry", not "never send".
    #[test]
    fn zero_retry_attempts_still_sends_once() {
        let toml_text = r#"
[models.x]
protocol = "openai-responses"
base_url = "https://h"
context_window = 1000

[models.x.retry]
max_attempts = 0
"#;
        let cfg = resolve_toml(toml_text, ConfigUserSettings::default(), env_of(&[])).unwrap();
        match &cfg.models.get("x").unwrap().backend {
            BackendConfig::OpenAIResponses(b) => assert_eq!(b.retry.max_attempts, 1),
            other => panic!("unexpected backend {other:?}"),
        }
    }

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
        toml_text: impl Into<String>,
        settings: ConfigUserSettings,
        env: impl Fn(&str) -> Option<String>,
    ) -> Result<Config, String> {
        resolve_toml_with_files(toml_text, settings, env, |p| {
            Err(format!("{}: no such file", p.display()))
        })
    }

    /// A one-user roster, so config tests exercise catalog resolution rather
    /// than the identity gate (which `roster::tests` covers on its own).
    fn test_roster(_: &Path) -> Result<FileRoster, String> {
        parse_file_roster_str("[[users]]\nid = \"tester\"\n")
    }

    fn resolve_toml_with_files(
        toml_text: impl Into<String>,
        settings: ConfigUserSettings,
        env: impl Fn(&str) -> Option<String>,
        read_auth_file: impl Fn(&Path) -> Result<String, String>,
    ) -> Result<Config, String> {
        let toml_text = toml_text.into();
        Config::resolve_with(
            settings,
            move |k| env(k).or_else(|| (k == "USER").then(|| "tester".to_string())),
            move |_, _| parse_file_config_str(&toml_text),
            test_roster,
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

    /// `max_image_base64_bytes` is per model and defaults to the shared cap. The
    /// resolved value must also reach the harness, which is what remote hosts
    /// are spawned with — a default there would let a remote `view_image`
    /// return images the model rejects.
    #[test]
    fn per_model_image_cap_defaults_and_reaches_the_harness() {
        let default_cfg = resolve_catalog_cfg(&[("OPENROUTER_API_KEY", "or-key")]);
        assert_eq!(
            default_cfg
                .models
                .spec("kimi-k3")
                .unwrap()
                .max_image_base64_bytes,
            DEFAULT_MAX_IMAGE_BASE64_BYTES
        );
        assert_eq!(
            default_cfg.harness.max_image_base64_bytes,
            DEFAULT_MAX_IMAGE_BASE64_BYTES
        );

        let toml_text = r#"
model = "big-images"

[models.big-images]
protocol = "openai-responses"
base_url = "https://h"
context_window = 32_768
max_image_base64_bytes = 12_582_912

[models.stock]
protocol = "openai-responses"
base_url = "https://h"
context_window = 32_768
"#;
        let cfg = resolve_toml(toml_text, ConfigUserSettings::default(), env_of(&[])).unwrap();
        assert_eq!(
            cfg.models
                .spec("big-images")
                .unwrap()
                .max_image_base64_bytes,
            12 * 1024 * 1024
        );
        // Untouched entries keep the default: the knob is per model, not global.
        assert_eq!(
            cfg.models.spec("stock").unwrap().max_image_base64_bytes,
            DEFAULT_MAX_IMAGE_BASE64_BYTES
        );
        // Hosts follow the *selected* model.
        assert_eq!(cfg.harness.max_image_base64_bytes, 12 * 1024 * 1024);
    }

    #[test]
    fn openai_completions_protocol_gets_a_chat_completions_backend() {
        let toml_text = r#"
[models.local-qwen]
protocol = "openai-completions"
base_url = "http://localhost:11434/v1"
thinking = "none"
context_window = 32_768
max_output_tokens = 4096
"#;
        let cfg = resolve_toml(toml_text, ConfigUserSettings::default(), env_of(&[])).unwrap();
        let entry = cfg.models.get("local-qwen").unwrap();
        assert_eq!(entry.spec.protocol, Protocol::OpenAICompletions);
        assert_eq!(entry.spec.thinking, ThinkingMode::None);
        match &entry.backend {
            BackendConfig::OpenAICompletions(b) => {
                assert_eq!(b.base_url, "http://localhost:11434/v1");
                assert_eq!(b.auth_token, "");
                assert_eq!(b.max_output_tokens, Some(4096));
            }
            other => panic!("expected OpenAI Chat Completions backend, got {other:?}"),
        }
    }

    #[test]
    fn literal_auth_string_is_the_token() {
        let toml_text = model_toml("proxy", &[r#"auth = "sk-inline-secret""#]);
        let cfg = resolve_toml(toml_text, ConfigUserSettings::default(), env_of(&[])).unwrap();
        match &cfg.models.get("proxy").unwrap().backend {
            BackendConfig::OpenAIResponses(b) => assert_eq!(b.auth_token, "sk-inline-secret"),
            other => panic!("unexpected backend {other:?}"),
        }
    }

    #[test]
    fn file_auth_reads_trimmed_contents_and_read_failure_defers() {
        let toml_text = model_toml(
            "proxy",
            &[r#"auth = { source = "file", path = "~/.secrets/corp.token" }"#],
        );
        let cfg = resolve_toml_with_files(
            toml_text.clone(),
            ConfigUserSettings::default(),
            env_of(&[]),
            |p| {
                assert_eq!(p, Path::new("~/.secrets/corp.token"));
                Ok("sekrit".into())
            },
        )
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
        let cfg = resolve_toml(
            model_toml("only", &[]),
            ConfigUserSettings::default(),
            env_of(&[]),
        )
        .unwrap();
        assert_eq!(cfg.model, "only");
    }

    #[test]
    fn missing_model_selection_errors_are_actionable() {
        // No models at all.
        let err = resolve_toml("", ConfigUserSettings::default(), env_of(&[])).unwrap_err();
        assert!(err.contains("no models configured"), "{err}");
        assert!(err.contains("[models]"), "{err}");

        // Multiple models, nothing selected.
        let two = [model_toml("a", &[]), model_toml("b", &[])].join("\n");
        let err =
            resolve_toml(two.clone(), ConfigUserSettings::default(), env_of(&[])).unwrap_err();
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
        // Compact cut of the documented format (`src/manual/articles/
        // overview.md`): env-auth gateways, gateway models, a gateway-less
        // local model.
        let example = r#"
model = "grok-4.5-build"

[gateways.xai]
protocol = "openai-responses"
base_url = "https://api.x.ai/v1"
auth = { source = "env", var_name = "XAI_API_KEY" }

[gateways.anthropic]
protocol = "anthropic-messages"
base_url = "https://api.anthropic.com"
auth = { source = "env", var_name = "ANTHROPIC_API_KEY" }

[models."grok-4.5-build"]
gateway = "xai"
context_window = 500_000

[models.claude-opus-4-8]
gateway = "anthropic"
context_window = 1_000_000

[models.qwen-local]
protocol = "openai-completions"
base_url = "http://localhost:11434/v1"
thinking = "none"
context_window = 32_768
"#;
        let cfg = resolve_toml(
            example,
            ConfigUserSettings::default(),
            env_of(&[("XAI_API_KEY", "xai")]),
        )
        .unwrap();
        assert_eq!(cfg.model, "grok-4.5-build");
        assert!(cfg.models.get("grok-4.5-build").is_ok());
        assert!(cfg.models.get("qwen-local").is_ok());
        // Anthropic entries resolve but defer their missing credential.
        let err = cfg.models.get("claude-opus-4-8").unwrap_err();
        assert!(err.contains("ANTHROPIC_API_KEY"), "{err}");
    }

    #[test]
    fn config_path_override_beats_env_beats_home_default() {
        let path_for = |override_path: Option<PathBuf>, env_pairs: &[(&str, &str)]| {
            let env = env_of(env_pairs);
            let env = move |k: &str| env(k).filter(|v: &String| !v.is_empty());
            resolve_config_path(override_path, &env).unwrap()
        };
        assert_eq!(
            path_for(
                Some(PathBuf::from("/tmp/x.toml")),
                &[("MYCO_CONFIG", "/env/y.toml")]
            ),
            (PathBuf::from("/tmp/x.toml"), true)
        );
        assert_eq!(
            path_for(None, &[("MYCO_CONFIG", "/env/y.toml")]),
            (PathBuf::from("/env/y.toml"), true)
        );
        // v2 keeps its config with the rest of its world, under `<home>/v2`.
        assert!(
            path_for(None, &[]).0.ends_with("v2/config.toml"),
            "{:?}",
            path_for(None, &[])
        );
    }

    #[test]
    fn file_loader_gets_resolved_path_and_result_is_stored() {
        let cfg = Config::resolve_with(
            ConfigUserSettings {
                config_path: Some(PathBuf::from("/tmp/h.toml")),
                ..Default::default()
            },
            env_of(&[("USER", "tester")]),
            |p, named| {
                assert_eq!(p, Path::new("/tmp/h.toml"));
                assert!(named, "--config paths are user-named");
                let mut file = parse_file_config_str(&model_toml("m", &[]))?;
                file.attach_timeout_secs = Some(42);
                Ok(file)
            },
            test_roster,
            || Ok(vec!["devbox".into()]),
            |_| Err("no files".into()),
        )
        .unwrap();
        assert_eq!(cfg.harness.attach_timeout_secs, 42);
        assert_eq!(cfg.harness.remote_hosts.len(), 1);
        assert_eq!(cfg.harness.remote_hosts[0].name, "devbox");
    }

    #[test]
    fn max_prelude_bytes_comes_from_the_config_file() {
        let resolve = |extra_toml: &str| {
            resolve_toml(
                format!("{extra_toml}\n{}", model_toml("m", &[])),
                ConfigUserSettings::default(),
                env_of(&[]),
            )
            .unwrap()
            .max_prelude_bytes
        };
        // Unset → the prompts default, so an untouched config.toml keeps the
        // 256 KiB backstop.
        assert_eq!(resolve(""), myco_prompts::DEFAULT_MAX_PRELUDE_BYTES);
        assert_eq!(resolve("max_prelude_bytes = 4096"), 4096);
        // TOML underscore separators are the shape the example config uses.
        assert_eq!(resolve("max_prelude_bytes = 131_072"), 131_072);
    }

    #[test]
    fn unset_attach_timeout_defaults_at_resolve() {
        // CATALOG leaves attach_timeout_secs unset; the default lands here,
        // not at parse.
        let cfg = resolve_catalog_cfg(&[]);
        assert_eq!(cfg.harness.attach_timeout_secs, DEFAULT_ATTACH_TIMEOUT_SECS);
    }

    #[test]
    fn load_errors_propagate() {
        let err =
            resolve_toml("not = = toml", ConfigUserSettings::default(), env_of(&[])).unwrap_err();
        assert!(err.contains("invalid config TOML"), "{err}");
    }
}
