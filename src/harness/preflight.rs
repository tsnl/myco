//! Startup work before the first prompt + the combined preflight WARNING.
//!
//! Startup exports the manual for this build ([`crate::manual::export`]) and
//! verifies that the external programs myco spawns (declared in
//! [`crate::external_command`]) actually resolve on the agent machine. Results
//! fold into one WARNING block with the ssh-agent preflight
//! ([`SshAgentPreflightReport`]) and the prelude checks
//! ([`crate::prompts::prelude_oversize`], [`crate::prelude::read_failure`]) —
//! one section after the banner, silent when everything resolves. Remote hosts
//! are not probed here; they report missing programs as tool errors at call
//! time.
//!
//! One finding is fatal rather than a warning: an oversized prelude
//! ([`StartupPreflight::prelude_oversize`]). The caller is expected to report
//! it and exit — see [`StartupPreflight::fatal`].
//!
//! Reporting stops at plain text ([`StartupPreflight::warning_body`]); the
//! caller owns the WARNING block around it, because the interactive REPL and
//! `--print` put it in different places (the Ui's event stream vs. stderr).

use std::io::Write;

use super::HostConfig;
use super::ssh::{SshAgentPreflightReport, ensure_remote_ssh_identities, ssh_host_targets};
use crate::external_command::{ExternalCommand, StartupCheck, expected_at_startup};
use crate::prompts::{PreludeOversize, prelude_oversize};

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
/// the prelude size cap and the expected executables, then ssh-agent identities.
#[derive(Debug, Default, Clone)]
pub struct StartupPreflight {
    /// Why the manual export failed, when it did. Agents are told in their
    /// system prompt to read those files, so a failure has to be visible.
    pub manual: Option<String>,
    /// Set when the rendered prelude is over `max_prelude_bytes`. **Fatal**:
    /// nothing trims the prelude any more, so myco refuses to start rather
    /// than run agents on a silently partial one ([`Self::fatal`]).
    pub prelude_oversize: Option<PreludeOversize>,
    /// Set when the prelude directory exists but could not be read, so this
    /// session's agents run with an empty prelude (see
    /// [`crate::prelude::read_failure`]).
    pub prelude_unreadable: Option<String>,
    pub executables: ExecutableCheckReport,
    pub ssh: SshAgentPreflightReport,
}

impl StartupPreflight {
    /// The manual export runs first (the prompt built right after this points
    /// at its directory). Then the executable check; the ssh-agent preflight
    /// runs only when the OpenSSH tools it spawns actually resolve —
    /// otherwise every step would fail with spawn errors the
    /// missing-executable lines already explain. `max_prelude_bytes` is the
    /// resolved config cap the prompts will use.
    pub fn run(hosts: &[HostConfig], max_prelude_bytes: usize) -> Self {
        let executables = check_expected_executables(hosts);
        let ssh = if executables.ssh_tools_missing() {
            SshAgentPreflightReport::default()
        } else {
            ensure_remote_ssh_identities(hosts)
        };
        Self {
            manual: export_manual().err(),
            prelude_oversize: prelude_oversize(max_prelude_bytes),
            prelude_unreadable: crate::prelude::dir()
                .ok()
                .as_deref()
                .and_then(crate::prelude::read_failure),
            executables,
            ssh,
        }
    }

    pub fn has_problems(&self) -> bool {
        self.manual.is_some()
            || self.prelude_unreadable.is_some()
            || !self.executables.is_clean()
            || self.ssh.has_problems()
    }

    /// The message for a fatal finding, or `None` when startup may proceed.
    ///
    /// Written to stand on its own: the caller exits straight after printing
    /// it, so there is no agent to delegate the repair to and no session in
    /// which to ask. It therefore names the directory, both sizes, and the two
    /// fixes available from a shell.
    pub fn fatal(&self) -> Option<String> {
        let over = self.prelude_oversize.as_ref()?;
        let dir = crate::prelude::dir()
            .map(|d| d.display().to_string())
            .unwrap_or_else(|_| "~/.myco/workspace/prelude".into());
        Some(format!(
            "{}\n\
             refusing to start: agents would run on a prelude too big to carry, and trimming \
             it would hide the missing part from them.\n\
             fix by hand in {dir} — merge overlapping entries or delete dead ones (each is a \
             plain `*.md` file; `ls -S` lists the biggest first) — or raise max_prelude_bytes \
             in config.toml above {} bytes.",
            over.describe(),
            over.bytes,
        ))
    }

    /// Every survivable preflight problem as plain body lines — manual,
    /// prelude, executables, ssh-agent, in that order, with no rule or header.
    /// Empty when [`Self::has_problems`] is false. Fatal findings are not
    /// here; they go through [`Self::fatal`] and end the process.
    ///
    /// Order is by how invisible the problem is otherwise: a missing manual
    /// export or an unreadable prelude never announces itself again (the agent
    /// simply runs without those bytes), while missing executables and ssh
    /// keys resurface at the tool call that needs them.
    pub fn warning_body(&self) -> String {
        let mut out = Vec::new();
        let _ = write_manual_body(self.manual.as_deref(), &mut out);
        let _ = write_prelude_unreadable_body(self.prelude_unreadable.as_deref(), &mut out);
        let _ = self.executables.write_body(&mut out);
        let _ = self.ssh.write_body(&mut out);
        String::from_utf8(out).unwrap_or_default()
    }
}

/// Copy the manual articles to `<myco home>/manual/<version>/<commit>/` for
/// this build, so agents can read and search them as ordinary files.
fn export_manual() -> Result<(), String> {
    crate::manual::export(&crate::core::myco_home()?).map(|_| ())
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

/// Unreadable-prelude lines only (no rule/header); writes nothing when the
/// directory reads, or is simply absent.
fn write_prelude_unreadable_body(
    failure: Option<&str>,
    out: &mut impl Write,
) -> std::io::Result<()> {
    let Some(failure) = failure else {
        return Ok(());
    };
    writeln!(out, "prelude directory unreadable: {failure}")?;
    writeln!(
        out,
        "every agent this session runs with an empty prelude; `prelude` action=list \
         will look empty too, so do not curate against it until this is fixed"
    )
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn preflight(
        missing: Vec<&'static ExternalCommand>,
        ssh: SshAgentPreflightReport,
    ) -> StartupPreflight {
        StartupPreflight {
            executables: ExecutableCheckReport { missing },
            ssh,
            ..Default::default()
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
            missing_executables(true, |_| true),
            SshAgentPreflightReport::default(),
        );
        assert!(!pf.has_problems());
        assert_eq!(rendered(&pf), "");
    }

    #[test]
    fn missing_tmux_reports_the_purpose_and_an_install_hint() {
        let pf = preflight(
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

    /// An oversized prelude is fatal, not a warning: it never reaches the
    /// WARNING block, and the message has to carry the whole repair, since
    /// the caller exits before any session or agent exists.
    #[test]
    fn an_oversized_prelude_is_fatal_and_self_contained() {
        let pf = StartupPreflight {
            prelude_oversize: Some(PreludeOversize {
                entries: 17,
                bytes: 300_000,
                limit: 256 * 1024,
            }),
            ..Default::default()
        };
        let fatal = pf.fatal().expect("an oversized prelude must be fatal");
        assert!(
            fatal.contains(
                "prelude is 293 KiB across 17 entries, over the 256 KiB \
                            max_prelude_bytes cap"
            ),
            "{fatal}"
        );
        assert!(fatal.contains("refusing to start"), "{fatal}");
        // Both repairs, and enough detail to act without an agent's help.
        assert!(fatal.contains("prelude"), "{fatal}");
        assert!(fatal.contains("raise max_prelude_bytes"), "{fatal}");
        assert!(fatal.contains("300000 bytes"), "{fatal}");

        // Fatal is a separate channel from the warning block; a prelude that
        // stops startup must not also be reported as a survivable warning.
        assert!(!pf.has_problems());
        assert_eq!(rendered(&pf), "");
    }

    /// A prelude within the cap leaves startup alone.
    #[test]
    fn a_prelude_within_the_cap_is_not_fatal() {
        assert_eq!(
            preflight(
                missing_executables(true, |_| true),
                SshAgentPreflightReport::default()
            )
            .fatal(),
            None
        );
    }

    /// An unreadable prelude directory is invisible from inside the prompt —
    /// the agent just runs without it — so startup has to say so, and has to
    /// warn the agent off curating against a `list` that looks empty.
    #[test]
    fn unreadable_prelude_directory_is_reported() {
        let pf = StartupPreflight {
            prelude_unreadable: Some("/home/u/.myco/workspace/prelude: permission denied".into()),
            ..Default::default()
        };
        assert!(pf.has_problems());
        let out = rendered(&pf);
        assert!(
            out.contains(
                "prelude directory unreadable: /home/u/.myco/workspace/prelude: permission denied"
            ),
            "{out}"
        );
        assert!(out.contains("runs with an empty prelude"), "{out}");
        assert!(out.contains("do not curate against it"), "{out}");
    }

    /// The prompt tells agents to read the exported files, so a failed export
    /// has to reach the user — nothing else this session will mention it.
    #[test]
    fn failed_manual_export_warns_first_and_names_the_path() {
        let pf = StartupPreflight {
            manual: Some("/home/u/.myco/manual/9.9.9/abc: permission denied".into()),
            executables: ExecutableCheckReport {
                missing: missing_executables(false, |e| e.name != "tmux"),
            },
            ..Default::default()
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
