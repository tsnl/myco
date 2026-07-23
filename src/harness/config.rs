//! Host pool configuration: remote hosts from `~/.ssh/config`.
//!
//! Every concrete `Host` alias (no `*`/`?` wildcards, no `!` negations) is a
//! remote host of the same name, attached lazily as
//! `ssh <alias> myco --mode host`. A small parser below extracts the aliases
//! (following `Include` directives); SSH details (user, port, identities,
//! ProxyJump, …) stay in ssh config where OpenSSH reads them natively — myco
//! only adds `BatchMode=yes`. The **local** host is always available
//! in-process and is never configured. Knobs (`attach_timeout_secs`) come
//! from `~/.myco/config.toml` ([`crate::config::file`]), resolved in
//! [`crate::config::Config`].

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use super::{HarnessConfig, HostConfig};

impl HarnessConfig {
    /// Host pool from concrete `Host` aliases in `~/.ssh/config` plus the
    /// resolved connect timeout ([`crate::config::Config`] applies the
    /// default). The reserved name `local` is skipped (always in-process,
    /// never SSH).
    pub fn from_ssh_aliases(ssh_aliases: Vec<String>, attach_timeout_secs: u64) -> Self {
        let remote_hosts = ssh_aliases
            .into_iter()
            .filter(|a| a != "local")
            .map(|alias| HostConfig {
                command: ssh_spawn_command(&alias),
                ssh_destination: Some(alias.clone()),
                name: alias,
            })
            .collect();
        Self {
            remote_hosts,
            attach_timeout_secs,
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
/// Understands the `Keyword args` and `Keyword=args` forms, double-quoted
/// tokens, and `Include` directives (relative paths and `*`/`?` filename
/// globs resolve against `~/.ssh`). Wildcard (`*`/`?`) and negated (`!`)
/// patterns are matching rules, not machines, and are skipped. Everything
/// else — options, `Match` blocks — is OpenSSH's business and is ignored.
pub fn ssh_config_host_aliases(reader: &mut impl std::io::BufRead) -> Result<Vec<String>, String> {
    let mut text = String::new();
    reader
        .read_to_string(&mut text)
        .map_err(|e| e.to_string())?;
    let mut scan = AliasScan::default();
    scan.file(&text, 0);
    Ok(scan.aliases)
}

/// Include chains deeper than this are dropped (runaway-nesting guard;
/// cycles are separately broken by the visited set).
const MAX_INCLUDE_DEPTH: usize = 16;

#[derive(Default)]
struct AliasScan {
    aliases: Vec<String>,
    seen: HashSet<String>,
    /// Canonical paths of files already included — breaks `Include` cycles.
    visited: HashSet<PathBuf>,
}

impl AliasScan {
    fn file(&mut self, text: &str, depth: usize) {
        for line in text.lines() {
            let Some((keyword, args)) = keyword_and_args(line) else {
                continue;
            };
            if keyword.eq_ignore_ascii_case("host") {
                for alias in argument_tokens(args) {
                    if alias.starts_with('!') || alias.contains(['*', '?']) {
                        continue;
                    }
                    if self.seen.insert(alias.clone()) {
                        self.aliases.push(alias);
                    }
                }
            } else if keyword.eq_ignore_ascii_case("include") && depth < MAX_INCLUDE_DEPTH {
                for pattern in argument_tokens(args) {
                    for path in include_files(&pattern) {
                        // Missing/unreadable targets are skipped like OpenSSH
                        // does; canonicalization doubles as the cycle key.
                        let Ok(canonical) = path.canonicalize() else {
                            continue;
                        };
                        if !self.visited.insert(canonical) {
                            continue;
                        }
                        if let Ok(sub) = std::fs::read_to_string(&path) {
                            self.file(&sub, depth + 1);
                        }
                    }
                }
            }
        }
    }
}

/// Split one config line into (keyword, argument text), or `None` for blank
/// and `#`-comment lines. The keyword ends at whitespace or `=`; one optional
/// `=` separator before the arguments is consumed (`Host=name` / `Host = name`).
fn keyword_and_args(line: &str) -> Option<(&str, &str)> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    let end = line
        .find(|c: char| c.is_whitespace() || c == '=')
        .unwrap_or(line.len());
    let (keyword, rest) = line.split_at(end);
    let rest = rest.trim_start();
    let rest = rest.strip_prefix('=').map_or(rest, str::trim_start);
    Some((keyword, rest))
}

/// Whitespace-separated argument tokens; double quotes group a token
/// (`"my host"`). No escape processing — OpenSSH's lexer has none either.
fn argument_tokens(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_quotes = false;
    for c in s.chars() {
        if c == '"' {
            in_quotes = !in_quotes;
        } else if c.is_whitespace() && !in_quotes {
            if !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            }
        } else {
            cur.push(c);
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// Expand one `Include` argument into candidate files. Absolute paths are
/// used as-is, `~/` expands to home, and bare relative paths resolve against
/// `~/.ssh` (OpenSSH's rule). A `*`/`?` glob is honored in the final filename
/// component only: the parent directory is listed and entries matched, sorted
/// for deterministic order.
fn include_files(pattern: &str) -> Vec<PathBuf> {
    let path = if let Some(rest) = pattern.strip_prefix("~/") {
        let Some(home) = dirs::home_dir() else {
            return Vec::new();
        };
        home.join(rest)
    } else if Path::new(pattern).is_absolute() {
        PathBuf::from(pattern)
    } else {
        let Some(home) = dirs::home_dir() else {
            return Vec::new();
        };
        home.join(".ssh").join(pattern)
    };
    let (Some(parent), Some(name)) = (path.parent(), path.file_name().and_then(|n| n.to_str()))
    else {
        return vec![path];
    };
    if !name.contains(['*', '?']) {
        return vec![path];
    }
    let Ok(entries) = std::fs::read_dir(parent) else {
        return Vec::new();
    };
    let mut matches: Vec<PathBuf> = entries
        .flatten()
        .filter(|e| {
            e.file_name()
                .to_str()
                .is_some_and(|entry| glob_match(name, entry))
        })
        .map(|e| e.path())
        .collect();
    matches.sort();
    matches
}

/// Minimal `*`/`?` glob over a single filename (no `/`, no character classes).
fn glob_match(pattern: &str, name: &str) -> bool {
    let mut pat = pattern.chars();
    match pat.next() {
        None => name.is_empty(),
        Some('*') => {
            let rest = pat.as_str();
            let mut tail = name;
            loop {
                if glob_match(rest, tail) {
                    return true;
                }
                let mut chars = tail.chars();
                if chars.next().is_none() {
                    return false;
                }
                tail = chars.as_str();
            }
        }
        Some(c) => {
            let mut chars = name.chars();
            let head = chars.next();
            (if c == '?' {
                head.is_some()
            } else {
                head == Some(c)
            }) && glob_match(pat.as_str(), chars.as_str())
        }
    }
}

/// Where remote hosts come from: `~/.ssh/config`.
pub fn default_ssh_config_path() -> Result<PathBuf, String> {
    let home = dirs::home_dir().ok_or_else(|| "could not resolve home directory".to_string())?;
    Ok(home.join(".ssh").join("config"))
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

#[cfg(test)]
mod tests {
    use super::*;

    fn aliases_from(ssh_config: &str) -> Vec<String> {
        ssh_config_host_aliases(&mut ssh_config.as_bytes()).unwrap()
    }

    fn harness_from(ssh_config: &str, attach_timeout_secs: u64) -> HarnessConfig {
        HarnessConfig::from_ssh_aliases(aliases_from(ssh_config), attach_timeout_secs)
    }

    #[test]
    fn no_ssh_hosts_is_local_only() {
        let cfg = harness_from("", 10);
        assert!(cfg.remote_hosts.is_empty());
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
        let cfg = harness_from(ssh_config, 5);
        assert_eq!(cfg.attach_timeout_secs, 5);
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
    fn include_subfile_with_quoted_path_and_eq_form() {
        let dir = std::env::temp_dir().join(format!("myco-sshconf-sub-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("extra conf"), "Host devbox\n").unwrap();
        let main = format!("Include=\"{}/extra conf\"\nHost laptop\n", dir.display());
        let aliases = ssh_config_host_aliases(&mut main.as_bytes()).unwrap();
        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(aliases, ["devbox", "laptop"]);
    }

    #[test]
    fn include_cycle_terminates() {
        let dir = std::env::temp_dir().join(format!("myco-sshconf-cycle-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let a = dir.join("a.conf");
        let b = dir.join("b.conf");
        std::fs::write(&a, format!("Host a\nInclude {}\n", b.display())).unwrap();
        std::fs::write(&b, format!("Host b\nInclude {}\n", a.display())).unwrap();
        let main = format!("Include {}\n", a.display());
        let aliases = ssh_config_host_aliases(&mut main.as_bytes()).unwrap();
        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(aliases, ["a", "b"]);
    }

    #[test]
    fn missing_include_target_is_skipped() {
        assert_eq!(
            aliases_from("Include /nonexistent-myco-test/x.conf\nHost devbox\n"),
            ["devbox"]
        );
    }

    #[test]
    fn quoted_alias_may_contain_spaces() {
        assert_eq!(
            aliases_from("Host \"my host\" plain\n"),
            ["my host", "plain"]
        );
    }

    #[test]
    fn local_alias_reserved_and_skipped() {
        let cfg = harness_from("Host local devbox\n", 10);
        let names: Vec<_> = cfg.remote_hosts.iter().map(|h| h.name.as_str()).collect();
        assert_eq!(names, ["devbox"]);
    }
}
