//! Startup health check for expected executables + combined preflight WARNING.
//!
//! Interactive startup verifies that the external programs myco spawns
//! (declared in [`crate::external_command`]) actually resolve on the agent
//! machine. Results fold into the same WARNING block as the ssh-agent
//! preflight ([`SshAgentPreflightReport`]) and the soul size check
//! ([`crate::prompts::soul_truncation`]) — one section after the banner,
//! silent when everything resolves. Remote hosts are not probed here; they
//! report missing programs as tool errors at call time.

use std::io::Write;

use super::HostConfig;
use super::ssh::{SshAgentPreflightReport, ensure_remote_ssh_identities, ssh_host_targets};
use crate::external_command::{ExternalCommand, StartupCheck, expected_at_startup};
use crate::prompts::{SoulTruncation, soul_truncation};
use crate::session::{Palette, write_warning_open};

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

/// Everything interactive startup checks before the first prompt: the soul
/// size cap, expected executables, then ssh-agent identities.
#[derive(Debug, Default, Clone)]
pub struct StartupPreflight {
    /// Set when the newest soul version does not fit under `max_soul_bytes`.
    pub soul: Option<SoulTruncation>,
    pub executables: ExecutableCheckReport,
    pub ssh: SshAgentPreflightReport,
}

impl StartupPreflight {
    /// Executable check first; the ssh-agent preflight runs only when the
    /// OpenSSH tools it spawns actually resolve — otherwise every step would
    /// fail with spawn errors the missing-executable lines already explain.
    /// `max_soul_bytes` is the resolved config cap the prompts will use.
    pub fn run(hosts: &[HostConfig], max_soul_bytes: usize) -> Self {
        let executables = check_expected_executables(hosts);
        let ssh = if executables.ssh_tools_missing() {
            SshAgentPreflightReport::default()
        } else {
            ensure_remote_ssh_identities(hosts)
        };
        Self {
            soul: soul_truncation(max_soul_bytes),
            executables,
            ssh,
        }
    }

    pub fn has_problems(&self) -> bool {
        self.soul.is_some() || !self.executables.is_clean() || self.ssh.has_problems()
    }

    /// Plain body lines of the WARNING section (soul first, then executables,
    /// then ssh-agent; no rule/header). Empty on the happy path. The
    /// interactive CLI feeds this to its Ui's WARNING section.
    ///
    /// The soul goes first: a cut soul is invisible everywhere else (the agent
    /// simply runs with less of itself), while missing executables and ssh
    /// keys announce themselves again at the tool call that needs them.
    pub fn warning_body(&self) -> String {
        let mut out = Vec::new();
        let _ = write_soul_body(self.soul.as_ref(), &mut out);
        let _ = self.executables.write_body(&mut out);
        if self.ssh.has_problems() {
            let _ = self.ssh.write_body(&mut out);
        }
        String::from_utf8(out).unwrap_or_default()
    }

    /// Write all preflight problems as one WARNING section. Writes nothing on
    /// the happy path.
    pub fn write_warning_section(
        &self,
        out: &mut impl Write,
        palette: Palette,
    ) -> std::io::Result<()> {
        if !self.has_problems() {
            return Ok(());
        }
        write_warning_open(out, palette)?;
        out.write_all(self.warning_body().as_bytes())
    }
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

/// Print preflight problems as a WARNING block on stdout, after the startup
/// banner and before the first USER block. Happy path prints nothing.
/// Live-only, like ERROR: not stored in history, not replayed on Ctrl-L/resume.
pub fn print_startup_preflight(report: &StartupPreflight, palette: Palette) {
    let mut out = std::io::stdout();
    let _ = report.write_warning_section(&mut out, palette);
    let _ = out.flush();
}

/// Test-only render of one report's `write_warning_section` with a plain
/// palette (shared by the ssh-agent and combined-preflight tests).
#[cfg(test)]
pub(crate) fn warning_output(
    write: impl FnOnce(&mut Vec<u8>, Palette) -> std::io::Result<()>,
) -> String {
    let mut buf = Vec::new();
    write(&mut buf, Palette::plain()).unwrap();
    String::from_utf8(buf).unwrap()
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
            soul,
            executables: ExecutableCheckReport { missing },
            ssh,
        }
    }

    fn rendered(pf: &StartupPreflight) -> String {
        warning_output(|out, p| pf.write_warning_section(out, p))
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
    fn missing_tmux_opens_warning_with_install_hint() {
        let pf = preflight(
            None,
            missing_executables(false, |e| e.name != "tmux"),
            SshAgentPreflightReport::default(),
        );
        let out = rendered(&pf);
        assert!(out.contains("WARNING"), "{out}");
        assert!(
            out.contains("missing executable tmux: bare /resume cannot open the session browser"),
            "{out}"
        );
        assert!(
            out.contains("hint: install the missing executables"),
            "{out}"
        );
    }

    #[test]
    fn combined_block_has_one_header_executables_before_ssh() {
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
        assert_eq!(out.matches("WARNING\n").count(), 1, "{out}");
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
    // are `soul_truncation`'s claim, pinned in `crate::prompts` tests; here
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
        assert_eq!(out.matches("WARNING\n").count(), 1, "{out}");
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
    fn a_truncated_soul_alone_is_enough_to_open_the_block() {
        // Nothing else wrong: the soul still gets its own headed WARNING.
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
        assert!(out.contains("WARNING\n"), "{out}");
        assert!(out.contains("soul/20260722T0215-3f2a.md:"), "{out}");
        assert!(!out.contains("missing executable"), "{out}");
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
