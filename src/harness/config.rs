//! Harness / host pool configuration.
//!
//! Remote hosts come from **`~/.ssh/config`**: every concrete `Host` alias
//! (no `*`/`?` wildcards, no `!` negations) is a remote host of the same name,
//! attached lazily as `ssh <alias> myco --mode host`. SSH details (user, port,
//! identities, ProxyJump, …) stay in ssh config where OpenSSH reads them
//! natively — myco only adds `BatchMode=yes`.
//!
//! `~/.myco/config.toml` holds the myco-specific knobs (`enable_subagent`,
//! `attach_timeout_secs`, `remote_myco`). The **local** host is always
//! available in-process and is never configured.

use std::path::{Path, PathBuf};

use super::{HarnessConfig, HostConfig};

/// On-disk config file shape (`~/.myco/config.toml`). Knobs only — hosts come
/// from `~/.ssh/config`.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct FileConfig {
    /// When false, do not register the in-process `subagent` tool.
    #[serde(default = "default_true")]
    pub enable_subagent: bool,
    /// Per-remote-host connect timeout in seconds on first tool use (lazy spawn + hello).
    /// `0` disables the timeout. (Config key kept as `attach_timeout_secs`.)
    #[serde(default = "default_attach_timeout_secs")]
    pub attach_timeout_secs: u64,
    /// Binary to run on every remote after connect (default `"myco"`). Must be
    /// on the PATH non-interactive SSH uses, or an absolute path valid on all
    /// remotes.
    #[serde(default = "default_remote_myco")]
    pub remote_myco: String,
}

impl Default for FileConfig {
    fn default() -> Self {
        Self {
            enable_subagent: default_true(),
            attach_timeout_secs: default_attach_timeout_secs(),
            remote_myco: default_remote_myco(),
        }
    }
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
    /// Combine file knobs with `Host` aliases from `~/.ssh/config`.
    ///
    /// The reserved name `local` is skipped (always in-process, never SSH).
    pub fn into_harness_config(self, ssh_aliases: Vec<String>) -> HarnessConfig {
        let remote_myco = {
            let t = self.remote_myco.trim();
            if t.is_empty() {
                default_remote_myco()
            } else {
                t.to_string()
            }
        };
        let remote_hosts = ssh_aliases
            .into_iter()
            .filter(|a| a != "local")
            .map(|alias| HostConfig {
                command: ssh_spawn_command(&alias, &remote_myco),
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

/// Argv for one remote: `ssh -o BatchMode=yes <alias> <myco> --mode host --name <alias>`.
///
/// BatchMode is required because the NDJSON pipe is not a TTY — OpenSSH must
/// never prompt there. Everything else about the connection comes from
/// `~/.ssh/config` for the alias.
pub fn ssh_spawn_command(alias: &str, remote_myco: &str) -> Vec<String> {
    vec![
        "ssh".into(),
        "-o".into(),
        "BatchMode=yes".into(),
        alias.into(),
        remote_myco.into(),
        "--mode".into(),
        "host".into(),
        "--name".into(),
        alias.into(),
    ]
}

/// Concrete `Host` aliases from ssh_config text, in file order, deduped.
///
/// Wildcard (`*`/`?`) and negated (`!`) patterns are matching rules, not
/// machines, and are skipped. `Include` directives are not followed — aliases
/// myco should see must be in the main file.
pub fn ssh_config_host_aliases(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        // ssh_config keywords are separated from arguments by whitespace or `=`.
        let Some((keyword, rest)) = line.split_once(|c: char| c.is_whitespace() || c == '=') else {
            continue;
        };
        if !keyword.eq_ignore_ascii_case("host") {
            continue;
        }
        // `Host = devbox` — whitespace around the optional `=` is valid syntax.
        let rest = rest.trim_start().strip_prefix('=').unwrap_or(rest);
        for pattern in rest.split_whitespace() {
            let alias = pattern.trim_matches('"');
            if alias.is_empty()
                || alias.starts_with('!')
                || alias.contains('*')
                || alias.contains('?')
            {
                continue;
            }
            if seen.insert(alias.to_string()) {
                out.push(alias.to_string());
            }
        }
    }
    out
}

/// Default config path: `$MYCO_CONFIG` or `~/.myco/config.toml`.
pub fn default_config_path() -> Result<PathBuf, String> {
    if let Ok(p) = std::env::var("MYCO_CONFIG") {
        return Ok(PathBuf::from(p));
    }
    let home = dirs::home_dir().ok_or_else(|| "could not resolve home directory".to_string())?;
    Ok(home.join(".myco").join("config.toml"))
}

/// Where remote hosts come from: `~/.ssh/config`.
pub fn default_ssh_config_path() -> Result<PathBuf, String> {
    let home = dirs::home_dir().ok_or_else(|| "could not resolve home directory".to_string())?;
    Ok(home.join(".ssh").join("config"))
}

/// Load harness config: knobs from `path` (missing file → defaults), remote
/// hosts from `~/.ssh/config` `Host` aliases (missing file → local only).
pub fn load_harness_config(path: &Path) -> Result<HarnessConfig, String> {
    let file = if path.exists() {
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("read config {}: {e}", path.display()))?;
        parse_file_config_str(&text).map_err(|e| format!("parse config {}: {e}", path.display()))?
    } else {
        FileConfig::default()
    };
    let aliases = default_ssh_config_path()
        .ok()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .map(|text| ssh_config_host_aliases(&text))
        .unwrap_or_default();
    Ok(file.into_harness_config(aliases))
}

/// Parse `config.toml` text. Rejects the removed `[[remote_hosts]]` section
/// rather than silently ignoring it.
pub fn parse_file_config_str(text: &str) -> Result<FileConfig, String> {
    let value: toml::Value =
        toml::from_str(text).map_err(|e| format!("invalid config TOML: {e}"))?;
    if value.get("remote_hosts").is_some() {
        return Err(
            "`[[remote_hosts]]` is no longer supported: remote hosts now come from \
             `Host` aliases in ~/.ssh/config — remove the section (set `remote_myco` \
             here if remotes need a non-default myco binary)"
                .into(),
        );
    }
    value
        .try_into()
        .map_err(|e| format!("invalid config TOML: {e}"))
}

/// Example config written by docs / first-run hints.
pub fn example_config_toml() -> String {
    r#"# Myco harness config (~/.myco/config.toml)
# Override path with MYCO_CONFIG or myco --config.
#
# The local host is always enabled in-process. Remote hosts are NOT listed
# here: every concrete `Host` alias in ~/.ssh/config (no wildcards) is a
# remote host of the same name, attached lazily as `ssh <alias> myco --mode
# host`. Put user / port / identity / ProxyJump in ~/.ssh/config.

enable_subagent = true
# Per-remote connect timeout in seconds on first tool use (0 disables).
# Remotes connect lazily; startup does not wait for them.
attach_timeout_secs = 10
# Binary to run on remotes (default "myco"). Must be on the PATH used by
# non-interactive SSH, or an absolute path valid on every remote.
# remote_myco = "myco"
"#
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn harness_from(toml_text: &str, ssh_config: &str) -> HarnessConfig {
        parse_file_config_str(toml_text)
            .unwrap()
            .into_harness_config(ssh_config_host_aliases(ssh_config))
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
        let aliases = ssh_config_host_aliases(ssh_config);
        assert_eq!(aliases, ["devbox"]);
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
        let aliases = ssh_config_host_aliases(ssh_config);
        // `Match` lines and comment lines never add hosts.
        assert_eq!(aliases, ["lower", "eq-form", "spaced-eq", "quoted"]);
    }

    #[test]
    fn duplicate_aliases_deduped_in_order() {
        let ssh_config = "Host a b\nHost b c\nHost a\n";
        assert_eq!(ssh_config_host_aliases(ssh_config), ["a", "b", "c"]);
    }

    #[test]
    fn local_alias_reserved_and_skipped() {
        let cfg = harness_from("", "Host local devbox\n");
        let names: Vec<_> = cfg.remote_hosts.iter().map(|h| h.name.as_str()).collect();
        assert_eq!(names, ["devbox"]);
    }

    #[test]
    fn remote_myco_overrides_remote_binary() {
        let cfg = harness_from(
            r#"remote_myco = "/home/alice/.local/bin/myco""#,
            "Host gpu\n",
        );
        let cmd = &cfg.remote_hosts[0].command;
        assert!(cmd.iter().any(|s| s == "/home/alice/.local/bin/myco"));
        assert!(!cmd.iter().any(|s| s == "myco"));
        // Blank value falls back to the default binary name.
        let cfg = harness_from(r#"remote_myco = "  ""#, "Host gpu\n");
        assert!(cfg.remote_hosts[0].command.iter().any(|s| s == "myco"));
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
    fn unknown_keys_are_ignored() {
        let cfg = parse_file_config_str("default_host = \"devbox\"").unwrap();
        assert!(cfg.enable_subagent);
    }

    #[test]
    fn spawn_command_shape() {
        let cmd = ssh_spawn_command("devbox", "myco");
        assert_eq!(cmd[0], "ssh");
        assert!(cmd.windows(2).any(|w| w == ["-o", "BatchMode=yes"]));
        assert!(cmd.iter().any(|s| s == "devbox"));
        assert!(cmd.windows(2).any(|w| w == ["--mode", "host"]));
        assert!(cmd.windows(2).any(|w| w == ["--name", "devbox"]));
    }
}
