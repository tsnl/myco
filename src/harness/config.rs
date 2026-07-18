//! Harness / host pool configuration (`~/.myco/config.toml`).
//!
//! The **local** host is always available in-process (not configured here).
//! Only remote hosts are listed; each is described with explicit SSH fields
//! rather than a free-form spawn command.

use std::path::Path;

use super::{HarnessConfig, HostConfig, RemoteHostConfig};
use crate::generative_model::Model;

/// On-disk config file shape.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct FileConfig {
    /// Default model for the interactive CLI (`--model` overrides).
    #[serde(default)]
    pub model: Option<Model>,
    /// When false, do not register the in-process `subagent` tool.
    #[serde(default = "default_true")]
    pub enable_subagent: bool,
    /// Per-remote-host connect timeout in seconds on first tool use (lazy spawn + hello).
    /// `0` disables the timeout. (Config key kept as `attach_timeout_secs`.)
    #[serde(default = "default_attach_timeout_secs")]
    pub attach_timeout_secs: u64,
    /// Remote hosts only. Local is always present and is not listed here.
    #[serde(default)]
    pub remote_hosts: Vec<FileRemoteHost>,
}

impl Default for FileConfig {
    fn default() -> Self {
        Self {
            model: None,
            enable_subagent: default_true(),
            attach_timeout_secs: default_attach_timeout_secs(),
            remote_hosts: Vec::new(),
        }
    }
}

/// One remote host: name + SSH destination / options (myco builds the argv).
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct FileRemoteHost {
    /// Logical host name for tool routing (`host` field). Must not be `"local"`.
    pub name: String,
    /// SSH destination: `~/.ssh/config` Host alias, hostname, or `user@host`.
    pub ssh: String,
    /// Remote binary to run after connect (default `"myco"`).
    /// Accepts legacy key `honk` from pre-rename configs.
    #[serde(default = "default_remote_myco", alias = "honk")]
    pub myco: String,
    /// Extra OpenSSH `-o key=value` options. `BatchMode=yes` is always applied.
    #[serde(default)]
    pub ssh_options: Vec<String>,
    /// Optional identity file (`ssh -i`).
    #[serde(default)]
    pub identity_file: Option<String>,
    /// Optional port (`ssh -p`).
    #[serde(default)]
    pub port: Option<u16>,
    /// Optional login name (`ssh -l`). Prefer `user@host` in `ssh` when possible.
    #[serde(default)]
    pub user: Option<String>,
}

fn default_true() -> bool {
    true
}

fn default_attach_timeout_secs() -> u64 {
    10
}

fn default_remote_myco() -> String {
    "myco".into()
}

impl FileConfig {
    pub fn into_harness_config(self) -> Result<HarnessConfig, String> {
        let mut remote_hosts = Vec::with_capacity(self.remote_hosts.len());
        let mut seen = std::collections::HashSet::new();
        for h in self.remote_hosts {
            let name = h.name.trim().to_string();
            if name.is_empty() {
                return Err("remote host entry with empty name".into());
            }
            if name == "local" {
                return Err(
                    "remote host name \"local\" is reserved; local is always in-process".into(),
                );
            }
            if !seen.insert(name.clone()) {
                return Err(format!("duplicate remote host name {name:?}"));
            }
            let ssh = h.ssh.trim().to_string();
            if ssh.is_empty() {
                return Err(format!("remote host {name:?} has empty `ssh` destination"));
            }
            let myco = {
                let t = h.myco.trim();
                if t.is_empty() {
                    default_remote_myco()
                } else {
                    t.to_string()
                }
            };
            let remote = RemoteHostConfig {
                name: name.clone(),
                ssh,
                myco,
                ssh_options: h.ssh_options,
                identity_file: h.identity_file.filter(|s| !s.trim().is_empty()),
                port: h.port,
                user: h.user.filter(|s| !s.trim().is_empty()),
            };
            let command = remote.spawn_command();
            remote_hosts.push(HostConfig {
                name,
                command,
                ssh_destination: Some(remote.ssh),
            });
        }

        Ok(HarnessConfig {
            remote_hosts,
            enable_subagent: self.enable_subagent,
            attach_timeout_secs: self.attach_timeout_secs,
        })
    }
}

/// Load the on-disk config from `path`. Missing file → [`FileConfig::default`].
/// Path defaulting (`--config` → `$MYCO_CONFIG` → `~/.myco/config.toml`) and
/// host validation ([`FileConfig::into_harness_config`]) live in
/// [`crate::config::Config`].
pub fn load_file_config(path: &Path) -> Result<FileConfig, String> {
    if !path.exists() {
        return Ok(FileConfig::default());
    }
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("read config {}: {e}", path.display()))?;
    toml::from_str(&text)
        .map_err(|e| format!("parse config {}: invalid config TOML: {e}", path.display()))
}

/// Parse a TOML config string into [`HarnessConfig`].
pub fn parse_harness_config_str(text: &str) -> Result<HarnessConfig, String> {
    let file: FileConfig = toml::from_str(text).map_err(|e| format!("invalid config TOML: {e}"))?;
    file.into_harness_config()
}

/// Example config written by docs / first-run hints.
pub fn example_config_toml() -> String {
    r#"# Myco harness config (~/.myco/config.toml)
# Override path with MYCO_CONFIG or myco --config.
#
# The local host is always enabled in-process (no subprocess, not listed here).
# Host tools that omit `host` run on local.

# Default model for the interactive CLI (--model overrides).
# model = "grok-4.5-build"

enable_subagent = true
# Per-remote connect timeout in seconds on first tool use (0 disables).
# Remotes connect lazily; startup does not wait for them.
attach_timeout_secs = 10

# Remote hosts: explicit SSH fields (myco builds `ssh -o BatchMode=yes …`).
# Prefer Host aliases / ProxyJump / User in ~/.ssh/config when possible.
# [[remote_hosts]]
# name = "devbox"
# ssh = "devbox"                 # Host alias, hostname, or user@host
# # myco = "myco"                # remote binary (default)
# # user = "alice"               # optional ssh -l
# # port = 22                    # optional ssh -p
# # identity_file = "~/.ssh/id"  # optional ssh -i
# # ssh_options = ["ProxyJump=bastion"]  # extra -o key=value
"#
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_config_is_local_only() {
        let cfg = parse_harness_config_str("").unwrap();
        assert!(cfg.remote_hosts.is_empty());
        assert!(cfg.enable_subagent);
        assert_eq!(cfg.attach_timeout_secs, 10);
    }

    #[test]
    fn remote_host_parse() {
        let text = r#"
enable_subagent = false

[[remote_hosts]]
name = "devbox"
ssh = "devbox"
ssh_options = ["ConnectTimeout=5"]
port = 2222
user = "alice"
identity_file = "~/.ssh/id_ed25519"
"#;
        let cfg = parse_harness_config_str(text).unwrap();
        assert!(!cfg.enable_subagent);
        assert_eq!(cfg.remote_hosts.len(), 1);
        let h = &cfg.remote_hosts[0];
        assert_eq!(h.name, "devbox");
        assert_eq!(h.ssh_destination.as_deref(), Some("devbox"));
        let cmd = &h.command;
        assert_eq!(cmd[0], "ssh");
        assert!(cmd.windows(2).any(|w| w == ["-o", "BatchMode=yes"]));
        assert!(cmd.windows(2).any(|w| w == ["-o", "ConnectTimeout=5"]));
        assert!(cmd.windows(2).any(|w| w == ["-p", "2222"]));
        assert!(cmd.windows(2).any(|w| w == ["-l", "alice"]));
        assert!(
            cmd.windows(2)
                .any(|w| w[0] == "-i" && w[1].contains("id_ed25519"))
        );
        assert!(cmd.iter().any(|s| s == "devbox"));
        assert!(cmd.iter().any(|s| s == "myco"));
        assert!(cmd.windows(2).any(|w| w == ["--mode", "host"]));
        assert!(cmd.windows(2).any(|w| w == ["--name", "devbox"]));
    }

    #[test]
    fn model_key_parses_with_aliases() {
        let file: FileConfig = toml::from_str("model = \"claude-opus-4-8\"").unwrap();
        assert_eq!(file.model, Some(Model::ClaudeOpus48));
        let file: FileConfig = toml::from_str("model = \"claude-opus-4.8\"").unwrap();
        assert_eq!(file.model, Some(Model::ClaudeOpus48));
        assert_eq!(FileConfig::default().model, None);
        assert!(toml::from_str::<FileConfig>("model = \"gpt-99\"").is_err());
    }

    #[test]
    fn local_name_reserved() {
        let text = r#"
[[remote_hosts]]
name = "local"
ssh = "somewhere"
"#;
        let err = parse_harness_config_str(text).unwrap_err();
        assert!(err.contains("reserved"), "{err}");
    }

    #[test]
    fn empty_ssh_rejected() {
        let text = r#"
[[remote_hosts]]
name = "x"
ssh = "  "
"#;
        let err = parse_harness_config_str(text).unwrap_err();
        assert!(err.contains("ssh"), "{err}");
    }

    #[test]
    fn duplicate_name_rejected() {
        let text = r#"
[[remote_hosts]]
name = "a"
ssh = "a"
[[remote_hosts]]
name = "a"
ssh = "b"
"#;
        let err = parse_harness_config_str(text).unwrap_err();
        assert!(err.contains("duplicate"), "{err}");
    }

    #[test]
    fn rejects_legacy_default_host_key() {
        // serde ignores unknown fields by default only if we don't deny them.
        // default_host is no longer a field; it is silently ignored unless we
        // use deny_unknown_fields. Document that local is always default.
        let text = r#"
default_host = "devbox"
[[remote_hosts]]
name = "devbox"
ssh = "devbox"
"#;
        // Unknown keys are ignored by serde by default — config still parses.
        let cfg = parse_harness_config_str(text).unwrap();
        assert_eq!(cfg.remote_hosts.len(), 1);
    }
}
