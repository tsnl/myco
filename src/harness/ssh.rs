//! SSH support for remote hosts (identity discovery, agent preflight).
//!
//! All SSH-related harness logic lives here. Remotes are `Host` aliases from
//! `~/.ssh/config`; myco builds `ssh -o BatchMode=yes <alias> myco --mode host …`.
//! BatchMode is required because the NDJSON pipe is not a TTY — OpenSSH will
//! never prompt for a key passphrase on that pipe. Identities must already be
//! loaded in `ssh-agent` (or unlocked via macOS Keychain) before attach.
//!
//! Contents:
//! - `ssh -G` IdentityFile discovery (host names double as the ssh aliases)
//! - existing-agent queries (`ssh-add -l`) and interactive unlock (`ssh-add`,
//!   `--apple-load-keychain` / `--apple-use-keychain` on macOS)
//! - CLI-facing preflight report + WARNING-section body (silent when clean;
//!   printed via the combined [`crate::harness::StartupPreflight`] block)

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use super::HostConfig;
use crate::external_command::{SSH, SSH_ADD, SSH_KEYGEN};
use crate::session::{Palette, write_warning_open};

/// Outcome of [`ensure_remote_ssh_identities`].
#[derive(Debug, Default, Clone)]
pub struct SshAgentPreflightReport {
    /// Whether any remote hosts are configured (every remote is SSH-backed).
    pub had_ssh_hosts: bool,
    /// `ssh-add -l` succeeded (agent reachable). Empty agent still counts as ok.
    pub agent_ok: bool,
    /// Human-readable agent status (fingerprint list or error).
    pub agent_status: String,
    /// Identity files that were already present in the agent.
    pub already_loaded: Vec<PathBuf>,
    /// Identity files successfully added during this preflight.
    pub added: Vec<PathBuf>,
    /// Identity files still missing after attempts (with reason).
    pub still_missing: Vec<(PathBuf, String)>,
    /// Non-fatal notes (e.g. no IdentityFile listed for a host, no TTY).
    pub notes: Vec<String>,
}

impl SshAgentPreflightReport {
    pub fn is_clean(&self) -> bool {
        self.still_missing.is_empty()
    }

    /// The report warrants a WARNING body: SSH hosts exist and the agent is
    /// unreachable or keys are still missing.
    pub fn has_problems(&self) -> bool {
        self.had_ssh_hosts && !(self.agent_ok && self.is_clean())
    }

    /// Write preflight problems as a WARNING section (thin rule + header +
    /// body) to `out` — stdout live, or any buffer in tests. Writes nothing on
    /// the happy path (no SSH hosts, or agent reachable with no keys missing).
    /// The palette styles only the rule + header; body lines stay plain.
    pub fn write_warning_section(
        &self,
        out: &mut impl Write,
        palette: Palette,
    ) -> std::io::Result<()> {
        if !self.has_problems() {
            return Ok(());
        }
        write_warning_open(out, palette)?;
        self.write_body(out)
    }

    /// Body lines only (no rule/header) — shared with the combined startup
    /// preflight block ([`crate::harness::StartupPreflight`]).
    pub(crate) fn write_body(&self, out: &mut impl Write) -> std::io::Result<()> {
        if !self.agent_ok {
            writeln!(out, "ssh-agent: {}", self.agent_status)?;
        }
        for (p, why) in &self.still_missing {
            writeln!(out, "missing key {}: {why}", p.display())?;
        }
        for note in &self.notes {
            writeln!(out, "note: {note}")?;
        }
        if !self.still_missing.is_empty() {
            writeln!(
                out,
                "hint: run `ssh-add <key>` (macOS: `ssh-add --apple-use-keychain <key>`) \
                 then `/hosts` after reconnect, or restart myco"
            )?;
        }
        Ok(())
    }
}

/// Ensure identities required by SSH-backed hosts are loaded in the agent.
///
/// Steps:
/// 1. Collect unique `IdentityFile` paths via `ssh -G <alias>` for each SSH host.
/// 2. List fingerprints currently in the agent (`ssh-add -l`).
/// 3. On macOS, try `ssh-add --apple-load-keychain` once if anything is missing.
/// 4. For each still-missing key, run interactive `ssh-add` on `/dev/tty` when available.
///
/// Never fatally errors for soft-fail hosts: returns a report the CLI can print.
/// Returns `Err` only when the preflight machinery itself is unusable in a
/// surprising way (rare); callers may still continue attach.
pub fn ensure_remote_ssh_identities(hosts: &[HostConfig]) -> SshAgentPreflightReport {
    let mut report = SshAgentPreflightReport::default();

    let ssh_targets = ssh_host_targets(hosts);
    if ssh_targets.is_empty() {
        report
            .notes
            .push("no SSH-backed hosts in config; skipping agent preflight".into());
        return report;
    }
    report.had_ssh_hosts = true;

    // Map identity path → hosts that need it (for messages).
    let mut identity_hosts: BTreeMap<PathBuf, BTreeSet<String>> = BTreeMap::new();
    for host in &ssh_targets {
        match identity_files_for_alias(host) {
            Ok(paths) => {
                if paths.is_empty() {
                    report
                        .notes
                        .push(format!("host {host:?}: ssh -G listed no IdentityFile"));
                }
                for p in paths {
                    identity_hosts.entry(p).or_default().insert(host.clone());
                }
            }
            Err(e) => {
                report.notes.push(format!(
                    "host {host:?}: could not resolve identities via ssh -G: {e}"
                ));
            }
        }
    }

    if identity_hosts.is_empty() {
        report
            .notes
            .push("no identity files discovered for SSH hosts".into());
        // Still check agent reachability for diagnostics.
        match agent_fingerprints() {
            Ok((status, _)) => {
                report.agent_ok = true;
                report.agent_status = status;
            }
            Err(e) => {
                report.agent_ok = false;
                report.agent_status = e;
            }
        }
        return report;
    }

    let mut loaded = match agent_fingerprints() {
        Ok((status, fps)) => {
            report.agent_ok = true;
            report.agent_status = status;
            fps
        }
        Err(e) => {
            report.agent_ok = false;
            report.agent_status = e.clone();
            report.notes.push(format!(
                "ssh-agent not reachable ({e}); remote hosts using BatchMode will fail until the agent is up"
            ));
            return report;
        }
    };

    // Classify identities.
    let mut missing: Vec<PathBuf> = Vec::new();
    for path in identity_hosts.keys() {
        match identity_fingerprint(path) {
            Ok(fp) if loaded.contains(&fp) => {
                report.already_loaded.push(path.clone());
            }
            Ok(_) => missing.push(path.clone()),
            Err(e) => {
                // Cannot fingerprint (missing file, etc.) — still try ssh-add later.
                report.notes.push(format!(
                    "could not fingerprint {}: {e} (will still try ssh-add)",
                    path.display()
                ));
                missing.push(path.clone());
            }
        }
    }

    if missing.is_empty() {
        return report;
    }

    // macOS: load passphrases from Keychain into the agent without prompting.
    if cfg!(target_os = "macos") {
        match run_ssh_add_apple_load_keychain() {
            Ok(msg) => {
                if !msg.is_empty() {
                    report
                        .notes
                        .push(format!("ssh-add --apple-load-keychain: {msg}"));
                }
            }
            Err(e) => report
                .notes
                .push(format!("ssh-add --apple-load-keychain failed: {e}")),
        }
        if let Ok((status, fps)) = agent_fingerprints() {
            report.agent_status = status;
            loaded = fps;
        }
        missing.retain(|path| match identity_fingerprint(path) {
            Ok(fp) => {
                if loaded.contains(&fp) {
                    report.already_loaded.push(path.clone());
                    false
                } else {
                    true
                }
            }
            Err(_) => true,
        });
    }

    if missing.is_empty() {
        return report;
    }

    let tty = open_tty();
    if tty.is_none() {
        report.notes.push(
            "no controlling TTY; cannot prompt for key passphrases \
             (run `ssh-add` manually or start myco from a terminal)"
                .into(),
        );
        for path in missing {
            let hosts = identity_hosts
                .get(&path)
                .map(join_hosts)
                .unwrap_or_default();
            report.still_missing.push((
                path.clone(),
                format!("not in agent (needed by {hosts}); no TTY for ssh-add"),
            ));
        }
        return report;
    }

    for path in missing {
        let hosts = identity_hosts
            .get(&path)
            .map(join_hosts)
            .unwrap_or_else(|| "?".into());
        eprintln!("ssh-agent: unlocking {} (hosts: {hosts})", path.display());
        match interactive_ssh_add(&path) {
            Ok(()) => {
                report.added.push(path.clone());
            }
            Err(e) => {
                report.still_missing.push((path, e));
            }
        }
    }

    // Refresh status after adds.
    if let Ok((status, _)) = agent_fingerprints() {
        report.agent_status = status;
        report.agent_ok = true;
    }

    report
}

// ---------------------------------------------------------------------------
// Discovery
// ---------------------------------------------------------------------------

/// SSH destination per configured host: every remote *is* an `~/.ssh/config`
/// alias of the same name, spawned as `ssh <alias> …` by construction
/// ([`super::HarnessConfig::from_ssh_aliases`]), so this is just the name list.
pub(crate) fn ssh_host_targets(hosts: &[HostConfig]) -> Vec<String> {
    hosts.iter().map(|h| h.name.clone()).collect()
}

fn identity_files_for_alias(alias: &str) -> Result<Vec<PathBuf>, String> {
    let output = SSH
        .command()
        .args(["-G", alias])
        .output()
        .map_err(|e| format!("spawn ssh -G: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "ssh -G {alias:?} exited {}: {stderr}",
            output.status
        ));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut paths = Vec::new();
    let mut seen = BTreeSet::new();
    for line in stdout.lines() {
        // ssh -G prints lowercase keywords: `identityfile /path`
        let Some(rest) = line
            .strip_prefix("identityfile ")
            .or_else(|| line.strip_prefix("IdentityFile "))
        else {
            continue;
        };
        let raw = rest.trim();
        if raw.is_empty() {
            continue;
        }
        let path = expand_user_path(raw);
        if seen.insert(path.clone()) {
            paths.push(path);
        }
    }
    // `ssh -G` often lists every default key name; keep only files that exist so we
    // do not try to `ssh-add` missing id_ecdsa/id_ed25519 placeholders. If none
    // exist (misconfigured IdentityFile), keep the full list for error reporting.
    let existing: Vec<PathBuf> = paths.iter().filter(|p| p.exists()).cloned().collect();
    if existing.is_empty() {
        Ok(paths)
    } else {
        Ok(existing)
    }
}

fn expand_user_path(raw: &str) -> PathBuf {
    if let Some(rest) = raw.strip_prefix("~/")
        && let Some(home) = home_dir()
    {
        return home.join(rest);
    }
    if raw == "~"
        && let Some(home) = home_dir()
    {
        return home;
    }
    PathBuf::from(raw)
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from).or_else(|| {
        // dirs crate is a dependency of the package.
        dirs::home_dir()
    })
}

// ---------------------------------------------------------------------------
// Agent queries
// ---------------------------------------------------------------------------

/// Returns (human status line, set of SHA256 fingerprints without the `SHA256:` prefix normalization).
fn agent_fingerprints() -> Result<(String, BTreeSet<String>), String> {
    let output = SSH_ADD
        .command()
        .arg("-l")
        .output()
        .map_err(|e| format!("spawn ssh-add -l: {e}"))?;
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();

    // Exit 1 + "The agent has no identities." is fine.
    if !output.status.success() {
        let msg = if !stderr.is_empty() {
            stderr
        } else if !stdout.is_empty() {
            stdout
        } else {
            format!("ssh-add -l exited {}", output.status)
        };
        if msg.to_ascii_lowercase().contains("no identities") {
            return Ok((msg, BTreeSet::new()));
        }
        // Could not open a connection to your authentication agent.
        return Err(msg);
    }

    let mut fps = BTreeSet::new();
    for line in stdout.lines() {
        // e.g. "4096 SHA256:abcd… /path (RSA)"
        if let Some(fp) = line.split_whitespace().find(|t| t.starts_with("SHA256:")) {
            fps.insert(fp.to_string());
        }
    }
    let status = if stdout.is_empty() {
        "agent has no identities".into()
    } else {
        format!("{} key(s) in agent", fps.len())
    };
    Ok((status, fps))
}

fn identity_fingerprint(path: &Path) -> Result<String, String> {
    // Prefer .pub (works for encrypted private keys and SK keys without provider).
    let pub_path = public_key_path(path);
    let candidates: Vec<PathBuf> = if pub_path.exists() {
        vec![pub_path, path.to_path_buf()]
    } else {
        vec![path.to_path_buf()]
    };

    let mut last_err = String::new();
    for cand in candidates {
        let output = SSH_KEYGEN
            .command()
            .args(["-lf", cand.to_str().unwrap_or_default()])
            .output()
            .map_err(|e| format!("spawn ssh-keygen: {e}"))?;
        if output.status.success() {
            let stdout = String::from_utf8_lossy(&output.stdout);
            if let Some(fp) = stdout.split_whitespace().find(|t| t.starts_with("SHA256:")) {
                return Ok(fp.to_string());
            }
            last_err = format!(
                "no SHA256 fingerprint in ssh-keygen output for {}",
                cand.display()
            );
        } else {
            last_err = String::from_utf8_lossy(&output.stderr).trim().to_string();
            if last_err.is_empty() {
                last_err = format!("ssh-keygen -lf {} failed", cand.display());
            }
        }
    }
    Err(last_err)
}

fn public_key_path(private: &Path) -> PathBuf {
    let mut s = private.as_os_str().to_owned();
    s.push(".pub");
    PathBuf::from(s)
}

// ---------------------------------------------------------------------------
// Unlock
// ---------------------------------------------------------------------------

fn run_ssh_add_apple_load_keychain() -> Result<String, String> {
    let output = SSH_ADD
        .command()
        .arg("--apple-load-keychain")
        .output()
        .map_err(|e| format!("spawn: {e}"))?;
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    let msg = [stdout, stderr]
        .into_iter()
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("; ");
    // apple-load-keychain returns non-zero when nothing was loaded; that is ok.
    Ok(msg)
}

fn interactive_ssh_add(path: &Path) -> Result<(), String> {
    if !path.exists() {
        return Err(format!("file does not exist: {}", path.display()));
    }

    let mut cmd = SSH_ADD.command();
    // Store passphrase in Keychain on macOS so later --apple-load-keychain works.
    if cfg!(target_os = "macos") {
        cmd.arg("--apple-use-keychain");
    }
    cmd.arg(path);

    // Prefer the real terminal so passphrase prompts work even if stdin is piped.
    match open_tty() {
        Some(tty_in) => {
            // Need separate handles for stdin; reopen for stdout/stderr inherit from process.
            cmd.stdin(Stdio::from(tty_in));
            cmd.stdout(Stdio::inherit());
            cmd.stderr(Stdio::inherit());
        }
        None => {
            cmd.stdin(Stdio::inherit());
            cmd.stdout(Stdio::inherit());
            cmd.stderr(Stdio::inherit());
        }
    }

    let status = cmd.status().map_err(|e| format!("spawn ssh-add: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("ssh-add exited {status}"))
    }
}

fn open_tty() -> Option<std::fs::File> {
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/tty")
        .ok()
}

fn join_hosts(hosts: &BTreeSet<String>) -> String {
    hosts.iter().cloned().collect::<Vec<_>>().join(", ")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::preflight::warning_output;

    #[test]
    fn warning_silent_on_happy_path() {
        let report = SshAgentPreflightReport {
            had_ssh_hosts: true,
            agent_ok: true,
            agent_status: "2 key(s) in agent".into(),
            already_loaded: vec![PathBuf::from("/home/u/.ssh/id_ed25519")],
            notes: vec!["host \"x\": ssh -G listed no IdentityFile".into()],
            ..Default::default()
        };
        assert_eq!(
            warning_output(|out, p| report.write_warning_section(out, p)),
            ""
        );
    }

    #[test]
    fn warning_silent_without_ssh_hosts() {
        // Default report: no SSH hosts (agent_ok=false is irrelevant then).
        let report = SshAgentPreflightReport::default();
        assert_eq!(
            warning_output(|out, p| report.write_warning_section(out, p)),
            ""
        );
    }

    #[test]
    fn warning_reports_unreachable_agent() {
        let report = SshAgentPreflightReport {
            had_ssh_hosts: true,
            agent_ok: false,
            agent_status: "Could not open a connection to your authentication agent.".into(),
            notes: vec!["ssh-agent not reachable".into()],
            ..Default::default()
        };
        let out = warning_output(|out, p| report.write_warning_section(out, p));
        // Body directly follows the opened header (the rule/header layout
        // itself is transcript's pinned claim).
        assert!(out.contains("WARNING\n\nssh-agent: Could not open a connection"));
        assert!(out.contains("note: ssh-agent not reachable"));
        // Hint only accompanies missing keys.
        assert!(!out.contains("hint:"));
    }

    #[test]
    fn warning_reports_missing_keys_with_hint() {
        let report = SshAgentPreflightReport {
            had_ssh_hosts: true,
            agent_ok: true,
            agent_status: "1 key(s) in agent".into(),
            still_missing: vec![(
                PathBuf::from("/home/u/.ssh/id_rsa"),
                "not in agent (needed by workstation); no TTY for ssh-add".into(),
            )],
            ..Default::default()
        };
        let out = warning_output(|out, p| report.write_warning_section(out, p));
        assert!(out.contains("WARNING\n\nmissing key /home/u/.ssh/id_rsa: not in agent"));
        assert!(out.contains("hint: run `ssh-add <key>`"));
        // Agent is fine; no agent status line.
        assert!(!out.contains("ssh-agent:"));
    }

    #[test]
    fn expand_tilde() {
        let p = expand_user_path("~/foo/bar");
        assert!(p.is_absolute(), "{p:?}");
        assert!(p.ends_with("foo/bar"), "{p:?}");
    }
}
