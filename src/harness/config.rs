//! Harness / host pool configuration.
//!
//! Remote hosts come from **`~/.ssh/config`**: every concrete `Host` alias
//! (no `*`/`?` wildcards, no `!` negations) is a remote host of the same name,
//! attached lazily as `ssh <alias> myco --mode host`. Parsing (including
//! `Include` directives) is delegated to the `ssh2-config` crate; SSH details
//! (user, port, identities, ProxyJump, …) stay in ssh config where OpenSSH
//! reads them natively — myco only adds `BatchMode=yes`.
//!
//! `~/.myco/config.toml` holds the model catalog (`[gateways]` / `[models]`,
//! default `model`) and the myco knobs (`enable_subagent`,
//! `attach_timeout_secs`). The **local** host is always available in-process
//! and is never configured. Path defaulting, loading, and catalog resolution
//! happen in [`crate::config::Config`].

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use ssh2_config::{ParseRule, SshConfig};

use super::{HarnessConfig, HostConfig};
use crate::generative_model::{ModelPricing, Protocol, ThinkingMode};

/// On-disk config file shape (`~/.myco/config.toml`). Hosts come from
/// `~/.ssh/config`; models come from the `[gateways]` / `[models]` catalog
/// here — myco ships no built-in models. Catalog *resolution* (auth, overlay,
/// validation) lives in [`crate::config::Config`].
#[derive(Debug, Clone, serde::Deserialize)]
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
    /// When false, do not register the in-process `subagent` tool.
    #[serde(default = "default_true")]
    pub enable_subagent: bool,
    /// Per-remote-host connect timeout in seconds on first tool use (lazy spawn + hello).
    /// `0` disables the timeout. (Config key kept as `attach_timeout_secs`.)
    #[serde(default = "default_attach_timeout_secs")]
    pub attach_timeout_secs: u64,
}

impl Default for FileConfig {
    fn default() -> Self {
        Self {
            model: None,
            gateways: BTreeMap::new(),
            models: BTreeMap::new(),
            enable_subagent: default_true(),
            attach_timeout_secs: default_attach_timeout_secs(),
        }
    }
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
    /// `[models.KEY.pricing]`: USD per million tokens (`input`,
    /// optional `cached_input`, `output`). Enables session cost estimates;
    /// absent → token counts only.
    #[serde(default)]
    pub pricing: Option<ModelPricing>,
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

fn default_true() -> bool {
    true
}

fn default_attach_timeout_secs() -> u64 {
    10
}

impl FileConfig {
    /// Combine file knobs with `Host` aliases from `~/.ssh/config`.
    ///
    /// The reserved name `local` is skipped (always in-process, never SSH).
    pub fn into_harness_config(self, ssh_aliases: Vec<String>) -> HarnessConfig {
        let remote_hosts = ssh_aliases
            .into_iter()
            .filter(|a| a != "local")
            .map(|alias| HostConfig {
                command: ssh_spawn_command(&alias),
                ssh_destination: Some(alias.clone()),
                name: alias,
            })
            .collect();
        HarnessConfig {
            remote_hosts,
            enable_subagent: self.enable_subagent,
            attach_timeout_secs: self.attach_timeout_secs,
            // The resolved catalog is filled in by `crate::config::Config`
            // (catalog resolution needs env/tokens, which live there).
            models: Default::default(),
        }
    }
}

/// Argv for one remote: `ssh -o BatchMode=yes <alias> myco --mode host --name <alias>`.
///
/// BatchMode is required because the NDJSON pipe is not a TTY — OpenSSH must
/// never prompt there. Everything else about the connection comes from
/// `~/.ssh/config` for the alias. The remote `myco` must be on the PATH used
/// by non-interactive SSH.
pub fn ssh_spawn_command(alias: &str) -> Vec<String> {
    vec![
        "ssh".into(),
        "-o".into(),
        "BatchMode=yes".into(),
        alias.into(),
        "myco".into(),
        "--mode".into(),
        "host".into(),
        "--name".into(),
        alias.into(),
    ]
}

/// Concrete `Host` aliases from an ssh config, in file order, deduped.
///
/// Parsing (quoting, `=` syntax, and `Include` directives — relative paths and
/// globs resolve against `~/.ssh`) is delegated to `ssh2-config`. Wildcard
/// (`*`/`?`) and negated (`!`) patterns are matching rules, not machines, and
/// are skipped.
pub fn ssh_config_host_aliases(reader: &mut impl std::io::BufRead) -> Result<Vec<String>, String> {
    let config = SshConfig::default()
        .parse(
            reader,
            ParseRule::ALLOW_UNKNOWN_FIELDS | ParseRule::ALLOW_UNSUPPORTED_FIELDS,
        )
        .map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for host in config.get_hosts() {
        for clause in &host.pattern {
            let alias = clause.pattern.as_str();
            if clause.negated || alias.is_empty() || alias.contains('*') || alias.contains('?') {
                continue;
            }
            if seen.insert(alias.to_string()) {
                out.push(alias.to_string());
            }
        }
    }
    Ok(out)
}

/// Where remote hosts come from: `~/.ssh/config`.
pub fn default_ssh_config_path() -> Result<PathBuf, String> {
    let home = dirs::home_dir().ok_or_else(|| "could not resolve home directory".to_string())?;
    Ok(home.join(".ssh").join("config"))
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

/// Remote host aliases from `~/.ssh/config`. Missing/unreadable file → none.
pub fn load_ssh_host_aliases() -> Result<Vec<String>, String> {
    let Ok(ssh_path) = default_ssh_config_path() else {
        return Ok(Vec::new());
    };
    let Ok(f) = std::fs::File::open(&ssh_path) else {
        return Ok(Vec::new());
    };
    let mut reader = std::io::BufReader::new(f);
    ssh_config_host_aliases(&mut reader)
        .map_err(|e| format!("parse ssh config {}: {e}", ssh_path.display()))
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

enable_subagent = true
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

# Optional per-model pricing (USD per million tokens) enables session cost
# estimates (session_meta action "cost"). cached_input defaults to input.
[models.claude-haiku-4-5.pricing]
input = 1.00
cached_input = 0.10
output = 5.00

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

    fn aliases_from(ssh_config: &str) -> Vec<String> {
        ssh_config_host_aliases(&mut ssh_config.as_bytes()).unwrap()
    }

    fn harness_from(toml_text: &str, ssh_config: &str) -> HarnessConfig {
        parse_file_config_str(toml_text)
            .unwrap()
            .into_harness_config(aliases_from(ssh_config))
    }

    #[test]
    fn empty_config_and_no_ssh_hosts_is_local_only() {
        let cfg = harness_from("", "");
        assert!(cfg.remote_hosts.is_empty());
        assert!(cfg.enable_subagent);
        assert_eq!(cfg.attach_timeout_secs, 10);
    }

    #[test]
    fn concrete_ssh_aliases_become_hosts() {
        let ssh_config = r#"
Host devbox
    HostName devbox.example.com
    User alice
    Port 2222

Host gpu bastion
    IdentityFile ~/.ssh/id_ed25519
"#;
        let cfg = harness_from("enable_subagent = false", ssh_config);
        assert!(!cfg.enable_subagent);
        let names: Vec<_> = cfg.remote_hosts.iter().map(|h| h.name.as_str()).collect();
        assert_eq!(names, ["devbox", "gpu", "bastion"]);
        let h = &cfg.remote_hosts[0];
        assert_eq!(h.ssh_destination.as_deref(), Some("devbox"));
        // No per-host SSH flags: user/port/identity are ssh config's job.
        assert_eq!(
            h.command,
            [
                "ssh",
                "-o",
                "BatchMode=yes",
                "devbox",
                "myco",
                "--mode",
                "host",
                "--name",
                "devbox"
            ]
        );
    }

    #[test]
    fn wildcard_and_negated_patterns_are_skipped() {
        let ssh_config = r#"
Host *
    ServerAliveInterval 60
Host *.example.com prod-?? !prod-01 devbox
    User deploy
"#;
        assert_eq!(aliases_from(ssh_config), ["devbox"]);
    }

    #[test]
    fn keyword_variants_comments_and_quotes_parse() {
        let ssh_config = r#"
# Host commented-out
host lower
HOST=eq-form
Host = spaced-eq
  Host "quoted"
Match host something
    ProxyJump ignored
"#;
        // Comment lines and `Match` blocks never add hosts.
        assert_eq!(
            aliases_from(ssh_config),
            ["lower", "eq-form", "spaced-eq", "quoted"]
        );
    }

    #[test]
    fn duplicate_aliases_deduped_in_order() {
        assert_eq!(
            aliases_from("Host a b\nHost b c\nHost a\n"),
            ["a", "b", "c"]
        );
    }

    #[test]
    fn include_directives_are_followed() {
        let dir = std::env::temp_dir().join(format!("myco-sshconf-include-{}", std::process::id()));
        let confd = dir.join("conf.d");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&confd).unwrap();
        std::fs::write(confd.join("a.conf"), "Host devbox\n  HostName a.example\n").unwrap();
        std::fs::write(confd.join("b.conf"), "Host gpu\n").unwrap();
        let main = format!("Include {}/conf.d/*.conf\n\nHost laptop\n", dir.display());
        let aliases = ssh_config_host_aliases(&mut main.as_bytes()).unwrap();
        std::fs::remove_dir_all(&dir).ok();
        assert!(aliases.contains(&"devbox".to_string()), "{aliases:?}");
        assert!(aliases.contains(&"gpu".to_string()), "{aliases:?}");
        assert!(aliases.contains(&"laptop".to_string()), "{aliases:?}");
    }

    #[test]
    fn local_alias_reserved_and_skipped() {
        let cfg = harness_from("", "Host local devbox\n");
        let names: Vec<_> = cfg.remote_hosts.iter().map(|h| h.name.as_str()).collect();
        assert_eq!(names, ["devbox"]);
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
    fn model_pricing_parses_and_rejects_unknown_fields() {
        let text = r#"
[models.a]
protocol = "openai-responses"
base_url = "https://h"
context_window = 1000

[models.a.pricing]
input = 3.0
cached_input = 0.3
output = 15.0

[models.b]
protocol = "openai-responses"
base_url = "https://h"
context_window = 1000

[models.b.pricing]
input = 1.0
output = 5.0
"#;
        let file = parse_file_config_str(text).unwrap();
        let a = file.models["a"].pricing.unwrap();
        assert_eq!(a.input, 3.0);
        assert_eq!(a.cached_input, Some(0.3));
        assert_eq!(a.output, 15.0);
        assert_eq!(file.models["b"].pricing.unwrap().cached_input, None);

        let bad = "[models.x]\nprotocol = \"openai-responses\"\nbase_url = \"https://h\"\n\
                   context_window = 1000\n[models.x.pricing]\ninput = 1.0\noutput = 2.0\n\
                   typo_rate = 3.0\n";
        let err = parse_file_config_str(bad).unwrap_err();
        assert!(err.contains("typo_rate"), "{err}");
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
        assert_eq!(file.gateways.len(), 3);
        assert_eq!(file.models.len(), 4);
        assert_eq!(
            file.models["kimi-k3"].api_id.as_deref(),
            Some("moonshotai/kimi-k3")
        );
    }
}
