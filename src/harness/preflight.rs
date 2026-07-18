//! Startup health check for expected executables + combined preflight WARNING.
//!
//! Interactive startup verifies that the external programs myco spawns actually
//! resolve on the agent machine: `bash` and `lynx` for the standard local
//! tools, plus the OpenSSH client tools when SSH-backed remotes are configured.
//! Results fold into the same WARNING block as the ssh-agent preflight
//! ([`SshAgentPreflightReport`]) — one section after the banner, silent when
//! everything resolves. Remote hosts are not probed here; they report missing
//! programs as tool errors at call time.

use std::io::Write;

use super::HostConfig;
use super::ssh::{SshAgentPreflightReport, ensure_remote_ssh_identities, ssh_host_targets};
use crate::session::{Palette, write_warning_open};

/// One external program myco expects to resolve (PATH, or the tool's own
/// override — `MYCO_LYNX` for lynx).
#[derive(Debug, Clone)]
pub struct ExpectedExecutable {
    pub name: &'static str,
    /// What breaks without it — printed on the WARNING line.
    pub purpose: &'static str,
    /// Short install pointer.
    pub install_hint: &'static str,
}

/// Needed in every interactive session (standard local tools).
const ALWAYS: &[ExpectedExecutable] = &[
    ExpectedExecutable {
        name: "bash",
        purpose: "the bash tool cannot run commands",
        install_hint: "install bash",
    },
    ExpectedExecutable {
        name: "lynx",
        purpose: "the lynx_tui_browser tool cannot fetch pages",
        install_hint: "brew install lynx / apt install lynx, or set MYCO_LYNX",
    },
];

/// Spawned by the SSH transport and the ssh-agent preflight; expected only
/// when SSH-backed remote hosts are configured.
const SSH_TOOLS: &[ExpectedExecutable] = &[
    ExpectedExecutable {
        name: "ssh",
        purpose: "remote hosts cannot connect",
        install_hint: "install the OpenSSH client",
    },
    ExpectedExecutable {
        name: "ssh-add",
        purpose: "ssh-agent preflight and key unlock cannot run",
        install_hint: "install the OpenSSH client",
    },
    ExpectedExecutable {
        name: "ssh-keygen",
        purpose: "identity fingerprinting for the agent preflight cannot run",
        install_hint: "install the OpenSSH client",
    },
];

/// Outcome of [`check_expected_executables`].
#[derive(Debug, Default, Clone)]
pub struct ExecutableCheckReport {
    /// Expected executables that did not resolve.
    pub missing: Vec<ExpectedExecutable>,
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
            .any(|m| SSH_TOOLS.iter().any(|s| s.name == m.name))
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
        missing: missing_executables(need_ssh, |exe| match exe.name {
            // Same resolution the browser tool uses (MYCO_LYNX, then PATH).
            "lynx" => crate::tool_services::browser_service::resolve_lynx_bin().is_some(),
            name => on_path(name),
        }),
    }
}

/// Pure core: which expected executables fail to resolve.
fn missing_executables(
    need_ssh: bool,
    resolves: impl Fn(&ExpectedExecutable) -> bool,
) -> Vec<ExpectedExecutable> {
    let mut expected: Vec<&ExpectedExecutable> = ALWAYS.iter().collect();
    if need_ssh {
        expected.extend(SSH_TOOLS.iter());
    }
    expected
        .into_iter()
        .filter(|e| !resolves(e))
        .cloned()
        .collect()
}

fn on_path(name: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|p| on_path_env(name, &p))
}

/// `name` is a file in some dir of the `PATH`-style value.
fn on_path_env(name: &str, path: &std::ffi::OsStr) -> bool {
    std::env::split_paths(path).any(|dir| !dir.as_os_str().is_empty() && dir.join(name).is_file())
}

/// Everything interactive startup checks before the first prompt: expected
/// executables, then ssh-agent identities.
#[derive(Debug, Default, Clone)]
pub struct StartupPreflight {
    pub executables: ExecutableCheckReport,
    pub ssh: SshAgentPreflightReport,
}

impl StartupPreflight {
    /// Executable check first; the ssh-agent preflight runs only when the
    /// OpenSSH tools it spawns actually resolve — otherwise every step would
    /// fail with spawn errors the missing-executable lines already explain.
    pub fn run(hosts: &[HostConfig]) -> Self {
        let executables = check_expected_executables(hosts);
        let ssh = if executables.ssh_tools_missing() {
            SshAgentPreflightReport::default()
        } else {
            ensure_remote_ssh_identities(hosts)
        };
        Self { executables, ssh }
    }

    pub fn has_problems(&self) -> bool {
        !self.executables.is_clean() || self.ssh.has_problems()
    }

    /// Write all preflight problems as one WARNING section (executables first,
    /// then ssh-agent). Writes nothing on the happy path.
    pub fn write_warning_section(
        &self,
        out: &mut impl Write,
        palette: Palette,
    ) -> std::io::Result<()> {
        if !self.has_problems() {
            return Ok(());
        }
        write_warning_open(out, palette)?;
        self.executables.write_body(out)?;
        if self.ssh.has_problems() {
            self.ssh.write_body(out)?;
        }
        Ok(())
    }
}

/// Print preflight problems as a WARNING block on stdout, after the startup
/// banner and before the first USER block. Happy path prints nothing.
/// Live-only, like ERROR: not stored in history, not replayed on Ctrl-L/resume.
pub fn print_startup_preflight(report: &StartupPreflight, palette: Palette) {
    let mut out = std::io::stdout();
    let _ = report.write_warning_section(&mut out, palette);
    let _ = out.flush();
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn warning_output(pf: &StartupPreflight) -> String {
        let mut buf = Vec::new();
        pf.write_warning_section(&mut buf, Palette::plain())
            .unwrap();
        String::from_utf8(buf).unwrap()
    }

    #[test]
    fn ssh_tools_expected_only_with_ssh_hosts() {
        let names: Vec<_> = missing_executables(false, |_| false)
            .iter()
            .map(|e| e.name)
            .collect();
        assert_eq!(names, ["bash", "lynx"]);

        let names: Vec<_> = missing_executables(true, |_| false)
            .iter()
            .map(|e| e.name)
            .collect();
        assert_eq!(names, ["bash", "lynx", "ssh", "ssh-add", "ssh-keygen"]);
    }

    #[test]
    fn silent_when_everything_resolves() {
        let pf = StartupPreflight {
            executables: ExecutableCheckReport {
                missing: missing_executables(true, |_| true),
            },
            ssh: SshAgentPreflightReport::default(),
        };
        assert!(!pf.has_problems());
        assert_eq!(warning_output(&pf), "");
    }

    #[test]
    fn missing_lynx_opens_warning_with_install_hint() {
        let pf = StartupPreflight {
            executables: ExecutableCheckReport {
                missing: missing_executables(false, |e| e.name != "lynx"),
            },
            ssh: SshAgentPreflightReport::default(),
        };
        let out = warning_output(&pf);
        assert!(out.contains(&format!("{}\nWARNING\n\n", crate::session::SECTION_RULE)));
        assert!(
            out.contains("missing executable lynx: the lynx_tui_browser tool cannot fetch pages"),
            "{out}"
        );
        assert!(out.contains("MYCO_LYNX"), "{out}");
        assert!(
            out.contains("hint: install the missing executables"),
            "{out}"
        );
    }

    #[test]
    fn combined_block_has_one_header_executables_before_ssh() {
        let pf = StartupPreflight {
            executables: ExecutableCheckReport {
                missing: missing_executables(true, |e| e.name != "ssh"),
            },
            ssh: SshAgentPreflightReport {
                had_ssh_hosts: true,
                agent_ok: false,
                agent_status: "agent down".into(),
                ..Default::default()
            },
        };
        let out = warning_output(&pf);
        assert_eq!(out.matches("WARNING\n").count(), 1, "{out}");
        let exec_at = out.find("missing executable ssh:").unwrap();
        let agent_at = out.find("ssh-agent: agent down").unwrap();
        assert!(exec_at < agent_at, "{out}");
    }

    #[test]
    fn clean_ssh_report_notes_stay_out_of_executable_warnings() {
        // A clean-but-noted ssh report (e.g. "no SSH-backed hosts") must not
        // leak into a WARNING block opened for missing executables.
        let pf = StartupPreflight {
            executables: ExecutableCheckReport {
                missing: missing_executables(false, |e| e.name != "lynx"),
            },
            ssh: SshAgentPreflightReport {
                notes: vec!["no SSH-backed hosts in config; skipping agent preflight".into()],
                ..Default::default()
            },
        };
        let out = warning_output(&pf);
        assert!(out.contains("missing executable lynx"), "{out}");
        assert!(!out.contains("note:"), "{out}");
    }

    #[test]
    fn ssh_tools_missing_matches_only_openssh_tools() {
        let only_lynx = ExecutableCheckReport {
            missing: missing_executables(true, |e| e.name != "lynx"),
        };
        assert!(!only_lynx.ssh_tools_missing());
        let no_ssh_add = ExecutableCheckReport {
            missing: missing_executables(true, |e| e.name != "ssh-add"),
        };
        assert!(no_ssh_add.ssh_tools_missing());
    }

    #[test]
    fn path_probe_finds_only_files_in_listed_dirs() {
        let dir = std::env::temp_dir().join(format!(
            "myco-exec-check-{}",
            crate::session::uuid_simple_hex(uuid::Uuid::new_v4())
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("present"), "#!/bin/sh\n").unwrap();

        let path_var = std::env::join_paths([
            PathBuf::new(), // empty entry must be ignored, not treated as cwd
            PathBuf::from("/nonexistent-myco-dir"),
            dir.clone(),
        ])
        .unwrap();
        assert!(on_path_env("present", &path_var));
        assert!(!on_path_env("absent", &path_var));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
