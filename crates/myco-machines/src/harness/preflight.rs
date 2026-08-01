//! Startup work before the first prompt + the combined preflight WARNING.
//!
//! Startup exports the manual for this build ([`myco_prompts::manual::export`]) and
//! verifies that the external programs myco spawns (declared in
//! [`myco_core::external_command`]) actually resolve on the agent machine. Results
//! fold into one WARNING block with the ssh-agent preflight
//! ([`SshAgentPreflightReport`]) and the soul size check
//! ([`myco_prompts::soul_truncation`]) — one section after the banner,
//! silent when everything resolves. Remote hosts are not probed here; they
//! report missing programs as tool errors at call time.
//!
//! Reporting stops at plain text ([`StartupPreflight::warning_body`]); the
//! caller owns the WARNING block around it, because the interactive REPL and
//! `--print` put it in different places (the Ui's event stream vs. stderr).

use std::io::Write;

use super::HostConfig;
use super::ssh::{SshAgentPreflightReport, ensure_remote_ssh_identities, ssh_host_targets};
use myco_core::external_command::{ExternalCommand, StartupCheck, expected_at_startup};
use myco_prompts::{SoulTruncation, soul_truncation};

/// Outcome of [`check_expected_executables`].
#[derive(Debug, Default, Clone)]
pub struct ExecutableCheckReport {
    /// Registry entries that did not resolve.
    pub missing: Vec<&'static ExternalCommand>,
}

impl ExecutableCheckReport {
    pub fn is_clean(&self) -> bool {
        self.missing.is_empty()
    }

    /// Any of the OpenSSH tools are missing — the ssh-agent preflight cannot
    /// run without them.
    pub fn ssh_tools_missing(&self) -> bool {
        self.missing
            .iter()
            .any(|m| m.startup_check == StartupCheck::WithSshRemotes)
    }

    /// Body lines only (no rule/header); writes nothing when clean.
    fn write_body(&self, out: &mut impl Write) -> std::io::Result<()> {
        for m in &self.missing {
            writeln!(
                out,
                "missing executable {}: {} ({})",
                m.name, m.purpose, m.install_hint
            )?;
        }
        if !self.missing.is_empty() {
            writeln!(
                out,
                "hint: install the missing executables, then restart myco"
            )?;
        }
        Ok(())
    }
}

/// Probe the agent machine for every expected executable.
pub fn check_expected_executables(hosts: &[HostConfig]) -> ExecutableCheckReport {
    let need_ssh = !ssh_host_targets(hosts).is_empty();
    ExecutableCheckReport {
        missing: missing_executables(need_ssh, |c| c.is_installed()),
    }
}

/// Pure core: which expected registry entries fail to resolve.
fn missing_executables(
    need_ssh: bool,
    resolves: impl Fn(&ExternalCommand) -> bool,
) -> Vec<&'static ExternalCommand> {
    expected_at_startup(need_ssh)
        .filter(|c| !resolves(c))
        .collect()
}

/// Everything startup does before the first prompt: export the manual, check
/// the soul size cap and the expected executables, then ssh-agent identities.
#[derive(Debug, Default, Clone)]
pub struct StartupPreflight {
    /// Why the manual export failed, when it did. Agents are told in their
    /// system prompt to read those files, so a failure has to be visible.
    pub manual: Option<String>,
    /// Set when the newest soul version does not fit under `max_soul_bytes`.
    pub soul: Option<SoulTruncation>,
    pub executables: ExecutableCheckReport,
    pub ssh: SshAgentPreflightReport,
}

impl StartupPreflight {
    /// The manual export runs first (the prompt built right after this points
    /// at its directory). Then the executable check; the ssh-agent preflight
    /// runs only when the OpenSSH tools it spawns actually resolve —
    /// otherwise every step would fail with spawn errors the
    /// missing-executable lines already explain. `max_soul_bytes` is the
    /// resolved config cap the prompts will use.
    pub fn run(hosts: &[HostConfig], max_soul_bytes: usize) -> Self {
        let executables = check_expected_executables(hosts);
        let ssh = if executables.ssh_tools_missing() {
            SshAgentPreflightReport::default()
        } else {
            ensure_remote_ssh_identities(hosts)
        };
        Self {
            manual: export_manual().err(),
            soul: soul_truncation(max_soul_bytes),
            executables,
            ssh,
        }
    }

    pub fn has_problems(&self) -> bool {
        self.manual.is_some()
            || self.soul.is_some()
            || !self.executables.is_clean()
            || self.ssh.has_problems()
    }

    /// Every preflight problem as plain body lines — manual, soul, executables,
    /// ssh-agent, in that order, with no rule or header. Empty when
    /// [`Self::has_problems`] is false.
    ///
    /// Order is by how invisible the problem is otherwise: a missing manual
    /// export or a cut soul never announces itself again (the agent simply
    /// runs without those bytes), while missing executables and ssh keys
    /// resurface at the tool call that needs them.
    pub fn warning_body(&self) -> String {
        let mut out = Vec::new();
        let _ = write_manual_body(self.manual.as_deref(), &mut out);
        let _ = write_soul_body(self.soul.as_ref(), &mut out);
        let _ = self.executables.write_body(&mut out);
        let _ = self.ssh.write_body(&mut out);
        String::from_utf8(out).unwrap_or_default()
    }
}

/// Copy the manual articles to `<myco home>/manual/<version>/<commit>/` for
/// this build, so agents can read and search them as ordinary files.
fn export_manual() -> Result<(), String> {
    myco_prompts::manual::export(&myco_core::myco_home()?).map(|_| ())
}

/// Manual-export lines only (no rule/header); writes nothing on success.
fn write_manual_body(failure: Option<&str>, out: &mut impl Write) -> std::io::Result<()> {
    let Some(failure) = failure else {
        return Ok(());
    };
    writeln!(out, "manual export failed: {failure}")?;
    writeln!(
        out,
        "agents cannot read the runtime docs this session; `myco --help <id>` still works"
    )
}

/// Soul lines only (no rule/header); writes nothing when the soul fits.
fn write_soul_body(cut: Option<&SoulTruncation>, out: &mut impl Write) -> std::io::Result<()> {
    let Some(cut) = cut else { return Ok(()) };
    writeln!(out, "soul/{}: {}", cut.version, cut.describe())?;
    writeln!(
        out,
        "every agent prompt from now on carries only the first {} of that version",
        cut.human_limit()
    )?;
    writeln!(
        out,
        "hint: write a shorter soul revision, or raise `max_soul_bytes` in config.toml"
    )
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn preflight(
        soul: Option<SoulTruncation>,
        missing: Vec<&'static ExternalCommand>,
        ssh: SshAgentPreflightReport,
    ) -> StartupPreflight {
        StartupPreflight {
            manual: None,
            soul,
            executables: ExecutableCheckReport { missing },
            ssh,
        }
    }

    fn rendered(pf: &StartupPreflight) -> String {
        let body = pf.warning_body();
        // Plain problem lines. The block's rule and header come from the caller
        // (`tui::write_warning_section`), which is what keeps several unrelated
        // problems inside one WARNING block.
        assert!(!body.contains("WARNING"), "{body}");
        body
    }

    #[test]
    fn ssh_tools_expected_only_with_ssh_hosts() {
        let names: Vec<_> = missing_executables(false, |_| false)
            .iter()
            .map(|e| e.name)
            .collect();
        assert_eq!(names, ["bash", "tmux", "fzf"]);

        let names: Vec<_> = missing_executables(true, |_| false)
            .iter()
            .map(|e| e.name)
            .collect();
        assert_eq!(
            names,
            ["bash", "tmux", "fzf", "ssh", "ssh-add", "ssh-keygen"]
        );
    }

    #[test]
    fn silent_when_everything_resolves() {
        let pf = preflight(
            None,
            missing_executables(true, |_| true),
            SshAgentPreflightReport::default(),
        );
        assert!(!pf.has_problems());
        assert_eq!(rendered(&pf), "");
    }

    #[test]
    fn missing_tmux_reports_the_purpose_and_an_install_hint() {
        let pf = preflight(
            None,
            missing_executables(false, |e| e.name != "tmux"),
            SshAgentPreflightReport::default(),
        );
        let out = rendered(&pf);
        assert!(
            out.contains("missing executable tmux: bare /resume cannot open the session browser"),
            "{out}"
        );
        assert!(
            out.contains("hint: install the missing executables"),
            "{out}"
        );
    }

    /// Executables come before ssh-agent: a missing `ssh` binary explains the
    /// agent failure that follows it.
    #[test]
    fn executables_are_reported_before_ssh_agent() {
        let pf = preflight(
            None,
            missing_executables(true, |e| e.name != "ssh"),
            SshAgentPreflightReport {
                had_ssh_hosts: true,
                agent_ok: false,
                agent_status: "agent down".into(),
                ..Default::default()
            },
        );
        let out = rendered(&pf);
        let exec_at = out.find("missing executable ssh:").unwrap();
        let agent_at = out.find("ssh-agent: agent down").unwrap();
        assert!(exec_at < agent_at, "{out}");
    }

    #[test]
    fn clean_ssh_report_notes_stay_out_of_executable_warnings() {
        // A clean-but-noted ssh report (e.g. "no SSH-backed hosts") must not
        // leak into a WARNING block opened for missing executables.
        let pf = preflight(
            None,
            missing_executables(false, |e| e.name != "tmux"),
            SshAgentPreflightReport {
                notes: vec!["no SSH-backed hosts in config; skipping agent preflight".into()],
                ..Default::default()
            },
        );
        let out = rendered(&pf);
        assert!(out.contains("missing executable tmux"), "{out}");
        assert!(!out.contains("note:"), "{out}");
    }

    // The exact truncation strings ("soul truncated at 64 KiB of 128.9 KiB")
    // are `soul_truncation`'s claim, pinned in `myco_prompts` tests; here
    // only the block's shape and ordering are at stake.
    #[test]
    fn truncated_soul_warns_first_and_names_the_version() {
        let pf = preflight(
            Some(SoulTruncation {
                version: "20260722T0215-3f2a.md".into(),
                bytes: 132_000,
                limit: 64 * 1024,
            }),
            missing_executables(false, |e| e.name != "tmux"),
            SshAgentPreflightReport::default(),
        );
        assert!(pf.has_problems());
        let out = rendered(&pf);
        assert!(out.contains("soul/20260722T0215-3f2a.md:"), "{out}");
        assert!(
            out.contains("every agent prompt from now on carries only the first"),
            "{out}"
        );
        assert!(
            out.contains("raise `max_soul_bytes` in config.toml"),
            "{out}"
        );
        // The soul leads the block; the other checks follow it.
        let soul_at = out.find("soul/20260722T0215").unwrap();
        let exec_at = out.find("missing executable tmux").unwrap();
        assert!(soul_at < exec_at, "{out}");
    }

    #[test]
    fn a_truncated_soul_alone_is_enough_to_report() {
        // Nothing else wrong: the soul alone still opens a WARNING block.
        let pf = preflight(
            Some(SoulTruncation {
                version: "20260722T0215-3f2a.md".into(),
                bytes: 4096,
                limit: 2048,
            }),
            missing_executables(true, |_| true),
            SshAgentPreflightReport::default(),
        );
        assert!(pf.has_problems());
        let out = rendered(&pf);
        assert!(out.contains("soul/20260722T0215-3f2a.md:"), "{out}");
        assert!(!out.contains("missing executable"), "{out}");
    }

    /// The prompt tells agents to read the exported files, so a failed export
    /// has to reach the user — nothing else this session will mention it.
    #[test]
    fn failed_manual_export_warns_first_and_names_the_path() {
        let pf = StartupPreflight {
            manual: Some("/home/u/.myco/manual/9.9.9/abc: permission denied".into()),
            soul: None,
            executables: ExecutableCheckReport {
                missing: missing_executables(false, |e| e.name != "tmux"),
            },
            ssh: SshAgentPreflightReport::default(),
        };
        assert!(pf.has_problems());
        let out = rendered(&pf);
        assert!(
            out.contains("manual export failed: /home/u/.myco/manual/9.9.9/abc: permission denied"),
            "{out}"
        );
        assert!(out.contains("`myco --help <id>` still works"), "{out}");
        let manual_at = out.find("manual export failed").unwrap();
        let exec_at = out.find("missing executable tmux").unwrap();
        assert!(manual_at < exec_at, "{out}");
    }

    #[test]
    fn ssh_tools_missing_matches_only_openssh_tools() {
        let only_tmux = ExecutableCheckReport {
            missing: missing_executables(true, |e| e.name != "tmux"),
        };
        assert!(!only_tmux.ssh_tools_missing());
        let no_ssh_add = ExecutableCheckReport {
            missing: missing_executables(true, |e| e.name != "ssh-add"),
        };
        assert!(no_ssh_add.ssh_tools_missing());
    }
}
