//! The startup rc file (`~/.myco/mycorc`): a user-authored bash script sourced
//! once per agent launch, before `.env` loading and config resolution.
//!
//! Essentially a bashrc for myco. When stdin is a TTY the script gets the
//! terminal, so it can prompt (`ssh-add` passphrases, `ssh-agent` startup);
//! its **exported environment diff** — variables added, changed, or unset —
//! is then applied to the myco process, where `auth = { source = "env", … }`
//! lookups and every spawned tool process see it. Shell-local state (aliases,
//! functions, cwd) does not carry over.
//!
//! Only agent launches source the file (interactive REPL and `-p` print
//! mode); a remote `--mode host` worker gets its environment from the
//! non-interactive SSH shell instead. A missing file is skipped silently. A
//! failing rc warns and still applies what was captured: the environment dump
//! runs from an EXIT trap, so even an `exit` (or `set -e` failure) mid-rc
//! leaves a usable snapshot.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::io::IsTerminal;
use std::os::fd::AsFd;
use std::os::unix::ffi::OsStringExt;
use std::path::Path;
use std::process::Stdio;

use crate::external_command::BASH;

/// Where the rc shell dumps its final environment (`env -0`); set only for
/// that child and excluded from the applied diff.
const ENV_DUMP_VAR: &str = "MYCO_MYCORC_ENV_DUMP";

/// Vars the rc shell churns as mechanism, not user intent: bash bumps `SHLVL`
/// and rewrites `_` in every child shell, and applying `PWD` / `OLDPWD` would
/// desync the environment from the process's real working directory.
const EXCLUDED_VARS: &[&str] = &["_", "SHLVL", "PWD", "OLDPWD", ENV_DUMP_VAR];

/// Source `<myco home>/mycorc` when present and apply its exported environment
/// changes to this process. Missing file → silence; problems → stderr
/// warnings, never a startup failure (bashrc semantics: a broken rc does not
/// keep the shell from opening).
///
/// # Safety
///
/// Mutates the process environment (`set_var` / `remove_var`). The caller must
/// guarantee no other threads exist yet — call it from `main`, before the
/// tokio runtime starts.
pub unsafe fn run_startup_rc() {
    let Ok(rc) = crate::session::myco_home().map(|home| home.join("mycorc")) else {
        return;
    };
    if !rc.is_file() {
        return;
    }
    match source_rc(&rc, std::io::stdin().is_terminal()) {
        Ok(outcome) => {
            if let Some(warning) = outcome.warning {
                eprintln!("warning: {warning}");
            }
            // SAFETY: forwarded — the caller guarantees single-threadedness.
            unsafe { outcome.changes.apply() };
        }
        Err(e) => eprintln!(
            "warning: {}: {e}; environment changes not applied",
            rc.display()
        ),
    }
}

/// Exported-environment diff between the process and the finished rc shell.
#[derive(Debug, Default)]
struct EnvChanges {
    set: Vec<(OsString, OsString)>,
    removed: Vec<OsString>,
}

impl EnvChanges {
    /// # Safety
    ///
    /// Same contract as [`run_startup_rc`]: no other threads may exist.
    unsafe fn apply(&self) {
        for (name, value) in &self.set {
            // Keys cannot contain `=` / NUL (parse construction) and are
            // non-empty, so set_var cannot panic.
            unsafe { std::env::set_var(name, value) };
        }
        for name in &self.removed {
            unsafe { std::env::remove_var(name) };
        }
    }
}

#[derive(Debug)]
struct RcOutcome {
    changes: EnvChanges,
    /// Non-fatal problem worth a stderr warning (the rc exited uncleanly).
    warning: Option<String>,
}

/// Run bash to source `rc` and diff the environment it ends with against ours.
///
/// `interactive_stdin` hands the child our stdin (TTY launches — the rc may
/// prompt); otherwise it gets no stdin, so a piped launch (nested myco,
/// `… | myco -p`) can never have its input bytes eaten by the rc. The rc's
/// stdout goes to our stderr: print-mode stdout must carry only the answer,
/// and rc chatter belongs with warnings either way.
fn source_rc(rc: &Path, interactive_stdin: bool) -> Result<RcOutcome, String> {
    let before: BTreeMap<OsString, OsString> = std::env::vars_os().collect();
    let dump_path = std::env::temp_dir().join(format!(
        "myco-mycorc-{}.env",
        uuid::Uuid::new_v4().as_simple()
    ));

    let status = BASH
        .command()
        .arg("-c")
        // The trap is installed before the rc runs, so the dump survives an
        // `exit` or `set -e` failure inside it; single quotes defer the
        // expansion to trap time.
        .arg(format!(r#"trap 'env -0 > "${ENV_DUMP_VAR}"' EXIT; . "$1""#))
        .arg("mycorc") // $0 in bash error messages
        .arg(rc)
        .env(ENV_DUMP_VAR, &dump_path)
        .stdin(if interactive_stdin {
            Stdio::inherit()
        } else {
            Stdio::null()
        })
        .stdout(stdout_to_stderr())
        .stderr(Stdio::inherit())
        .status()
        .map_err(|e| format!("could not run bash: {e}"))?;

    let dump = std::fs::read(&dump_path);
    let _ = std::fs::remove_file(&dump_path);
    let dump = dump.map_err(|_| format!("the rc shell wrote no environment dump ({status})"))?;

    let changes = diff_env(&before, &parse_env_dump(&dump));
    let warning = (!status.success()).then(|| {
        format!(
            "{} did not exit cleanly ({status}); its environment changes were still applied",
            rc.display()
        )
    });
    Ok(RcOutcome { changes, warning })
}

fn stdout_to_stderr() -> Stdio {
    match std::io::stderr().as_fd().try_clone_to_owned() {
        Ok(fd) => Stdio::from(fd),
        Err(_) => Stdio::inherit(),
    }
}

/// Parse `env -0` output. NUL separation keeps multiline values intact; an
/// entry without `=` (or with an empty name) cannot be a variable and is
/// dropped.
fn parse_env_dump(bytes: &[u8]) -> BTreeMap<OsString, OsString> {
    let mut map = BTreeMap::new();
    for entry in bytes.split(|b| *b == 0) {
        let Some(eq) = entry.iter().position(|b| *b == b'=') else {
            continue;
        };
        if eq == 0 {
            continue;
        }
        map.insert(
            OsString::from_vec(entry[..eq].to_vec()),
            OsString::from_vec(entry[eq + 1..].to_vec()),
        );
    }
    map
}

fn excluded(name: &OsStr) -> bool {
    EXCLUDED_VARS.iter().any(|v| OsStr::new(v) == name)
}

fn diff_env(
    before: &BTreeMap<OsString, OsString>,
    after: &BTreeMap<OsString, OsString>,
) -> EnvChanges {
    let mut changes = EnvChanges::default();
    for (name, value) in after {
        if excluded(name) || before.get(name) == Some(value) {
            continue;
        }
        changes.set.push((name.clone(), value.clone()));
    }
    for name in before.keys() {
        if !excluded(name) && !after.contains_key(name) {
            changes.removed.push(name.clone());
        }
    }
    changes
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn os(s: &str) -> OsString {
        OsString::from(s)
    }

    fn env_map(pairs: &[(&str, &str)]) -> BTreeMap<OsString, OsString> {
        pairs.iter().map(|(k, v)| (os(k), os(v))).collect()
    }

    fn write_temp_rc(body: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "myco-mycorc-test-{}.sh",
            uuid::Uuid::new_v4().as_simple()
        ));
        std::fs::write(&path, body).unwrap();
        path
    }

    #[test]
    fn env_dump_parses_nul_separated_entries_with_newlines_and_equals() {
        let dump = b"A=1\0MULTI=line1\nline2\0EQ=a=b\0\0NOEQ\0=empty-name\0";
        let map = parse_env_dump(dump);
        assert_eq!(map.get(OsStr::new("A")), Some(&os("1")));
        assert_eq!(map.get(OsStr::new("MULTI")), Some(&os("line1\nline2")));
        assert_eq!(map.get(OsStr::new("EQ")), Some(&os("a=b")));
        // NOEQ and the empty-name entry cannot be variables.
        assert_eq!(map.len(), 3);
    }

    #[test]
    fn diff_reports_added_changed_and_removed_but_not_shell_churn() {
        let before = env_map(&[
            ("KEEP", "same"),
            ("CHANGE", "old"),
            ("GONE", "x"),
            ("SHLVL", "1"),
            ("PWD", "/a"),
        ]);
        let after = env_map(&[
            ("KEEP", "same"),
            ("CHANGE", "new"),
            ("ADDED", "hi"),
            ("SHLVL", "2"),
            ("PWD", "/b"),
            ("_", "/usr/bin/env"),
            (ENV_DUMP_VAR, "/tmp/dump"),
        ]);
        let changes = diff_env(&before, &after);
        assert_eq!(
            changes.set,
            vec![(os("ADDED"), os("hi")), (os("CHANGE"), os("new"))]
        );
        assert_eq!(changes.removed, vec![os("GONE")]);
    }

    #[test]
    fn sourcing_an_rc_captures_exports_without_touching_this_process() {
        let rc = write_temp_rc(
            "export MYCO_RC_TEST_EXPORTED=\"two\nlines\"\nMYCO_RC_TEST_UNEXPORTED=nope\n",
        );
        let outcome = source_rc(&rc, false).unwrap();
        let _ = std::fs::remove_file(&rc);
        assert!(outcome.warning.is_none());
        assert!(
            outcome
                .changes
                .set
                .contains(&(os("MYCO_RC_TEST_EXPORTED"), os("two\nlines")))
        );
        // Unexported shell variables never reach `env` output.
        assert!(
            !outcome
                .changes
                .set
                .iter()
                .any(|(k, _)| k == "MYCO_RC_TEST_UNEXPORTED")
        );
        // Capture is a diff, not an application: this process is untouched.
        assert!(std::env::var_os("MYCO_RC_TEST_EXPORTED").is_none());
    }

    #[test]
    fn unset_in_the_rc_lands_in_removed() {
        // cargo puts CARGO_PKG_NAME in the test process env; nothing to
        // observe when the test binary runs outside cargo.
        if std::env::var_os("CARGO_PKG_NAME").is_none() {
            return;
        }
        let rc = write_temp_rc("unset CARGO_PKG_NAME\n");
        let outcome = source_rc(&rc, false).unwrap();
        let _ = std::fs::remove_file(&rc);
        assert!(outcome.changes.removed.contains(&os("CARGO_PKG_NAME")));
        assert!(std::env::var_os("CARGO_PKG_NAME").is_some());
    }

    #[test]
    fn exit_mid_rc_still_captures_earlier_exports_and_warns() {
        let rc = write_temp_rc(
            "export MYCO_RC_TEST_BEFORE_EXIT=yes\nexit 3\nexport MYCO_RC_TEST_AFTER_EXIT=never\n",
        );
        let outcome = source_rc(&rc, false).unwrap();
        let _ = std::fs::remove_file(&rc);
        let warning = outcome.warning.expect("nonzero exit warns");
        assert!(warning.contains("exit status: 3"), "{warning}");
        assert!(
            outcome
                .changes
                .set
                .contains(&(os("MYCO_RC_TEST_BEFORE_EXIT"), os("yes")))
        );
        assert!(
            !outcome
                .changes
                .set
                .iter()
                .any(|(k, _)| k == "MYCO_RC_TEST_AFTER_EXIT")
        );
    }

    #[test]
    fn piped_launch_gives_the_rc_no_stdin_to_eat() {
        // `read` must hit EOF immediately (not hang, not consume the caller's
        // piped input) and the rc continues past it.
        let rc = write_temp_rc("read -r line\nexport MYCO_RC_TEST_AFTER_READ=done\n");
        let outcome = source_rc(&rc, false).unwrap();
        let _ = std::fs::remove_file(&rc);
        assert!(
            outcome
                .changes
                .set
                .contains(&(os("MYCO_RC_TEST_AFTER_READ"), os("done")))
        );
    }

    #[test]
    fn a_signal_killed_rc_shell_reports_no_dump() {
        let rc = write_temp_rc("kill -9 $$\n");
        let err = source_rc(&rc, false).unwrap_err();
        let _ = std::fs::remove_file(&rc);
        assert!(err.contains("no environment dump"), "{err}");
    }
}
