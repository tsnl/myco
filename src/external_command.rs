//! Single source of truth for external programs myco spawns.
//!
//! Every executable myco launches by name is declared here as an
//! [`ExternalCommand`]: how it resolves (env override → PATH → well-known
//! dirs), why myco needs it, and when the startup preflight expects it
//! ([`crate::harness::StartupPreflight`]). Call sites spawn through
//! [`ExternalCommand::command`] / [`ExternalCommand::tokio_command`], never
//! `Command::new("literal")` — so a new external process cannot skip the
//! registry or the preflight (enforced by the
//! `every_literal_spawn_goes_through_the_registry` test). `build.rs` programs
//! are out of scope: build scripts cannot use the crate.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// When the startup preflight expects the program on the agent machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartupCheck {
    /// Every interactive session (standard local tools).
    Always,
    /// Only when SSH-backed remote hosts are configured.
    WithSshRemotes,
    /// Never warned about at startup (test-only or best-effort spawns).
    Never,
}

/// One external program myco spawns by name.
#[derive(Debug)]
pub struct ExternalCommand {
    pub name: &'static str,
    /// What breaks without it — printed on the startup WARNING line.
    pub purpose: &'static str,
    /// Short install pointer for the WARNING line.
    pub install_hint: &'static str,
    pub startup_check: StartupCheck,
    /// Env var consulted before PATH; must point at an existing file.
    env_override: Option<&'static str>,
    /// Install dirs probed after PATH (GUI-launched processes on macOS often
    /// miss /opt/homebrew/bin in PATH).
    fallback_dirs: &'static [&'static str],
}

pub static BASH: ExternalCommand = ExternalCommand {
    name: "bash",
    purpose: "the bash tool cannot run commands",
    install_hint: "install bash",
    startup_check: StartupCheck::Always,
    env_override: None,
    fallback_dirs: &[],
};

pub static LYNX: ExternalCommand = ExternalCommand {
    name: "lynx",
    purpose: "the lynx_tui_browser tool cannot fetch pages",
    install_hint: "brew install lynx / apt install lynx, or set MYCO_LYNX",
    startup_check: StartupCheck::Always,
    env_override: Some("MYCO_LYNX"),
    fallback_dirs: &["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin"],
};

pub static SSH: ExternalCommand = ExternalCommand {
    name: "ssh",
    purpose: "remote hosts cannot connect",
    install_hint: "install the OpenSSH client",
    startup_check: StartupCheck::WithSshRemotes,
    env_override: None,
    fallback_dirs: &[],
};

pub static SSH_ADD: ExternalCommand = ExternalCommand {
    name: "ssh-add",
    purpose: "ssh-agent preflight and key unlock cannot run",
    install_hint: "install the OpenSSH client",
    startup_check: StartupCheck::WithSshRemotes,
    env_override: None,
    fallback_dirs: &[],
};

pub static SSH_KEYGEN: ExternalCommand = ExternalCommand {
    name: "ssh-keygen",
    purpose: "identity fingerprinting for the agent preflight cannot run",
    install_hint: "install the OpenSSH client",
    startup_check: StartupCheck::WithSshRemotes,
    env_override: None,
    fallback_dirs: &[],
};

/// Test-only process listing (bash session reap assertions).
pub static PS: ExternalCommand = ExternalCommand {
    name: "ps",
    purpose: "process diagnostics in tests",
    install_hint: "install procps",
    startup_check: StartupCheck::Never,
    env_override: None,
    fallback_dirs: &[],
};

/// Optional: bare `/resume` opens the session browser as a tmux popup when
/// the CLI is running inside tmux; without it the inline picker is used.
pub static TMUX: ExternalCommand = ExternalCommand {
    name: "tmux",
    purpose: "the session browser cannot open as a tmux popup",
    install_hint: "install tmux (optional)",
    startup_check: StartupCheck::Never,
    env_override: None,
    fallback_dirs: &["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin"],
};

/// Optional: fuzzy search + transcript preview in `--mode session-browser`;
/// without it the browser falls back to a paged prompt.
pub static FZF: ExternalCommand = ExternalCommand {
    name: "fzf",
    purpose: "the session browser cannot offer fuzzy search",
    install_hint: "brew install fzf / apt install fzf (optional)",
    startup_check: StartupCheck::Never,
    env_override: None,
    fallback_dirs: &["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin"],
};

/// Every registered program; the startup preflight iterates this.
pub static ALL: &[&ExternalCommand] =
    &[&BASH, &LYNX, &SSH, &SSH_ADD, &SSH_KEYGEN, &PS, &TMUX, &FZF];

/// Registry entries the startup preflight expects, in `ALL` order.
pub fn expected_at_startup(
    with_ssh_remotes: bool,
) -> impl Iterator<Item = &'static ExternalCommand> {
    ALL.iter().copied().filter(move |c| match c.startup_check {
        StartupCheck::Always => true,
        StartupCheck::WithSshRemotes => with_ssh_remotes,
        StartupCheck::Never => false,
    })
}

impl ExternalCommand {
    /// Resolve the program: env override → PATH → fallback dirs. `None` means
    /// not installed. Existence check is `is_file` (no executable-bit probe),
    /// matching how myco has always resolved lynx.
    pub fn resolve(&self) -> Option<PathBuf> {
        if let Some(var) = self.env_override
            && let Ok(p) = std::env::var(var)
            && !p.is_empty()
            && Path::new(&p).is_file()
        {
            return Some(PathBuf::from(p));
        }
        if let Some(hit) = std::env::var_os("PATH").and_then(|path| find_in(self.name, &path)) {
            return Some(hit);
        }
        self.fallback_dirs
            .iter()
            .map(|d| Path::new(d).join(self.name))
            .find(|p| p.is_file())
    }

    pub fn is_installed(&self) -> bool {
        self.resolve().is_some()
    }

    /// Spawnable program token: the resolved path, or the bare name when
    /// nothing resolved — the spawn then fails with the natural OS error.
    fn program(&self) -> OsString {
        match self.resolve() {
            Some(p) => p.into_os_string(),
            None => self.name.into(),
        }
    }

    pub fn command(&self) -> std::process::Command {
        std::process::Command::new(self.program())
    }

    pub fn tokio_command(&self) -> tokio::process::Command {
        tokio::process::Command::new(self.program())
    }
}

/// `name` is a file in some dir of the `PATH`-style value.
fn find_in(name: &str, path: &std::ffi::OsStr) -> Option<PathBuf> {
    std::env::split_paths(path)
        .filter(|dir| !dir.as_os_str().is_empty())
        .map(|dir| dir.join(name))
        .find(|p| p.is_file())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Claim: every external program myco spawns by name is declared in this
    /// registry. A `Command::new("literal")` anywhere else in src/ would
    /// bypass resolution and the startup preflight — declare the program here
    /// and spawn via `command()` / `tokio_command()` instead.
    #[test]
    fn every_literal_spawn_goes_through_the_registry() {
        let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut offenders = Vec::new();
        scan(&src, &mut offenders);
        assert!(
            offenders.is_empty(),
            "literal Command::new(\"…\") outside src/external_command.rs; \
             declare the program in the registry and spawn through it:\n{}",
            offenders.join("\n")
        );
    }

    fn scan(dir: &Path, offenders: &mut Vec<String>) {
        for entry in std::fs::read_dir(dir).expect("read src dir") {
            let path = entry.expect("dir entry").path();
            if path.is_dir() {
                scan(&path, offenders);
                continue;
            }
            if path.extension().is_none_or(|e| e != "rs")
                || path.file_name().is_some_and(|n| n == "external_command.rs")
            {
                continue;
            }
            let text = std::fs::read_to_string(&path).expect("read source file");
            for (i, line) in text.lines().enumerate() {
                if line.contains("Command::new(\"") {
                    offenders.push(format!("{}:{}: {}", path.display(), i + 1, line.trim()));
                }
            }
        }
    }

    #[test]
    fn registry_names_are_unique() {
        let mut names: Vec<_> = ALL.iter().map(|c| c.name).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), ALL.len());
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
        assert_eq!(find_in("present", &path_var), Some(dir.join("present")));
        assert_eq!(find_in("absent", &path_var), None);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
