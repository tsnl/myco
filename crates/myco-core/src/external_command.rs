//! Single source of truth for external programs myco spawns.
//!
//! Every executable myco launches by name is declared here as an
//! [`ExternalCommand`]: how it resolves (env override → PATH → well-known
//! dirs). Call sites spawn through [`ExternalCommand::command`] /
//! [`ExternalCommand::tokio_command`], never `Command::new("literal")` — so a
//! new external process cannot skip the registry (enforced by the
//! `every_literal_spawn_goes_through_the_registry` test). A missing program
//! fails at the call that needs it, with the OS's own error.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// One external program myco spawns by name.
#[derive(Debug)]
pub struct ExternalCommand {
    pub name: &'static str,
    /// Env var consulted before PATH; must point at an existing file.
    env_override: Option<&'static str>,
    /// Install dirs probed after PATH (GUI-launched processes on macOS often
    /// miss /opt/homebrew/bin in PATH).
    fallback_dirs: &'static [&'static str],
}

pub static BASH: ExternalCommand = ExternalCommand {
    name: "bash",
    env_override: None,
    fallback_dirs: &[],
};

pub static SSH: ExternalCommand = ExternalCommand {
    name: "ssh",
    env_override: None,
    fallback_dirs: &[],
};

pub static SSH_ADD: ExternalCommand = ExternalCommand {
    name: "ssh-add",
    env_override: None,
    fallback_dirs: &[],
};

pub static SSH_KEYGEN: ExternalCommand = ExternalCommand {
    name: "ssh-keygen",
    env_override: None,
    fallback_dirs: &[],
};

/// Test-only process listing (bash session reap assertions).
pub static PS: ExternalCommand = ExternalCommand {
    name: "ps",
    env_override: None,
    fallback_dirs: &[],
};

/// Every registered program (uniqueness pinned by test).
pub static ALL: &[&ExternalCommand] = &[&BASH, &SSH, &SSH_ADD, &SSH_KEYGEN, &PS];

impl ExternalCommand {
    /// Resolve the program: env override → PATH → fallback dirs. `None` means
    /// not installed. Existence check is `is_file` (no executable-bit probe).
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
            crate::uuid_simple_hex(uuid::Uuid::new_v4())
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
