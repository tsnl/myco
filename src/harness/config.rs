//! Harness / host pool configuration.
//!
//! Remote hosts come from **`~/.ssh/config`**: every concrete `Host` alias
//! (no `*`/`?` wildcards, no `!` negations) is a remote host of the same name,
//! attached lazily as `ssh <alias> myco --mode host`. Parsing (including
//! `Include` directives) is delegated to the `ssh2-config` crate; SSH details
//! (user, port, identities, ProxyJump, …) stay in ssh config where OpenSSH
//! reads them natively — myco only adds `BatchMode=yes`.
//!
//! `~/.myco/config.toml` holds the myco-specific knobs (`model`,
//! `enable_subagent`, `attach_timeout_secs`). The **local** host is always
//! available in-process and is never configured. Path defaulting and loading
//! happen in [`crate::config::Config`].

use std::path::{Path, PathBuf};

use ssh2_config::{ParseRule, SshConfig};

use super::{HarnessConfig, HostConfig};
use crate::generative_model::Model;

/// On-disk config file shape (`~/.myco/config.toml`). Knobs only — hosts come
/// from `~/.ssh/config`.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct FileConfig {
    /// Model for the interactive CLI (`--model` overrides). Startup requires
    /// one of the two — there is no built-in default.
    #[serde(default)]
    pub model: Option<Model>,
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
            enable_subagent: default_true(),
            attach_timeout_secs: default_attach_timeout_secs(),
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
/// [`FileConfig::default`]. Path defaulting (`$MYCO_CONFIG` →
/// `$MYCO_HOME/config.toml` → `~/.myco/config.toml`) lives in
/// [`crate::config::Config`].
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
    r#"# Myco harness config ($MYCO_HOME/config.toml, default ~/.myco/config.toml;
# override the file path with $MYCO_CONFIG).
#
# The local host is always enabled in-process. Remote hosts are NOT listed
# here: every concrete `Host` alias in ~/.ssh/config (no wildcards; Includes
# are followed) is a remote host of the same name, attached lazily as
# `ssh <alias> myco --mode host`. Put user / port / identity / ProxyJump in
# ~/.ssh/config; `myco` must be on the remote PATH non-interactive SSH uses.

# Model for the interactive CLI. Required (or pass --model each run).
model = "grok-4.5-build"

enable_subagent = true
# Per-remote connect timeout in seconds on first tool use (0 disables).
# Remotes connect lazily; startup does not wait for them.
attach_timeout_secs = 10
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
    fn model_key_parses_with_aliases() {
        let file = parse_file_config_str("model = \"claude-opus-4-8\"").unwrap();
        assert_eq!(file.model, Some(Model::ClaudeOpus48));
        let file = parse_file_config_str("model = \"claude-opus-4.8\"").unwrap();
        assert_eq!(file.model, Some(Model::ClaudeOpus48));
        assert_eq!(FileConfig::default().model, None);
        assert!(parse_file_config_str("model = \"gpt-99\"").is_err());
    }
}
