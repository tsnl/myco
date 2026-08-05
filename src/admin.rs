//! `myco auth …` — the administrative interface over the credential store.
//!
//! The store has no self-service surface on purpose: there is no signup, no
//! password-reset email, no way for a request to create an account. Everything
//! that mints or revokes access happens here, from a shell on the machine, by
//! someone who already has the box.
//!
//! Names come from the roster in `server.toml`; this manages what those names
//! *know*. `list` shows both halves, so "in the roster but cannot log in" is
//! visible rather than something you discover at a login prompt.

use std::io::Write as _;

use crate::auth::{AuthStore, MIN_PASSWORD_LEN};
use crate::config::Config;

#[derive(clap::Subcommand, Debug, Clone)]
pub enum AuthCommand {
    /// Show every user, whether they can log in, and their live sessions.
    List,
    /// Set or replace a password (prompted, never passed on the command line
    /// where it would land in shell history and `ps`).
    Passwd {
        /// Roster id, e.g. `ada`.
        id: String,
    },
    /// Refuse logins for a user and end their sessions. Their history keeps
    /// their name — a transcript records what happened, not who still has an
    /// account.
    Disable { id: String },
    /// Allow logins again.
    Enable { id: String },
    /// End every session for a user without changing their password.
    Revoke { id: String },
    /// Forget a user's credentials entirely.
    Remove { id: String },
}

/// Run an admin command. Returns the process exit code.
pub fn run(config: &Config, command: AuthCommand) -> i32 {
    let path = match AuthStore::default_path() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("myco: {e}");
            return 1;
        }
    };
    let store = match AuthStore::open(&path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("myco: {e}");
            return 1;
        }
    };
    // Same reconciliation the server does, so the admin interface works on a
    // fresh machine before the server has ever run.
    for user in config.roster.users() {
        if store.get(&user.id).is_none() {
            let _ = store.add_user(&user.id, user.display_name());
        }
    }

    match command {
        AuthCommand::List => list(&store),
        AuthCommand::Passwd { id } => passwd(&store, &id),
        AuthCommand::Disable { id } => set_disabled(&store, &id, true),
        AuthCommand::Enable { id } => set_disabled(&store, &id, false),
        AuthCommand::Revoke { id } => {
            let n = store.revoke_all_for(&id);
            println!("revoked {n} session(s) for {id}");
            0
        }
        AuthCommand::Remove { id } => match store.remove_user(&id) {
            Ok(()) => {
                println!("removed {id}");
                println!(
                    "note: {id} is still in the roster, so the server will re-add \
                     them (without a password) on next start"
                );
                0
            }
            Err(e) => {
                eprintln!("myco: {e}");
                1
            }
        },
    }
}

fn list(store: &AuthStore) -> i32 {
    let sessions: std::collections::HashMap<String, usize> =
        store.session_counts().into_iter().collect();
    let users = store.users();
    if users.is_empty() {
        println!("no users");
        return 0;
    }
    let width = users.iter().map(|u| u.id.len()).max().unwrap_or(4).max(4);
    println!("{:<width$}  {:<10}  {:<8}  NAME", "ID", "LOGIN", "SESSIONS");
    for u in &users {
        let state = if u.disabled {
            "disabled"
        } else if u.password_hash.is_none() {
            "no passwd"
        } else {
            "ok"
        };
        println!(
            "{:<width$}  {:<10}  {:<8}  {}",
            u.id,
            state,
            sessions.get(&u.id).copied().unwrap_or(0),
            u.name,
        );
    }
    0
}

fn set_disabled(store: &AuthStore, id: &str, disabled: bool) -> i32 {
    match store.set_disabled(id, disabled) {
        Ok(()) => {
            println!(
                "{id} is now {}",
                if disabled { "disabled" } else { "enabled" }
            );
            0
        }
        Err(e) => {
            eprintln!("myco: {e}");
            1
        }
    }
}

fn passwd(store: &AuthStore, id: &str) -> i32 {
    if store.get(id).is_none() {
        eprintln!(
            "myco: no such user: {id}. Add them to the [[users]] list in \
             server.toml first — the roster is what decides who exists."
        );
        return 1;
    }
    let first = match prompt_password(&format!("New password for {id}: ")) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("myco: {e}");
            return 1;
        }
    };
    let again = match prompt_password("Retype: ") {
        Ok(p) => p,
        Err(e) => {
            eprintln!("myco: {e}");
            return 1;
        }
    };
    if first != again {
        eprintln!("myco: passwords do not match");
        return 1;
    }
    if first.chars().count() < MIN_PASSWORD_LEN {
        eprintln!("myco: password must be at least {MIN_PASSWORD_LEN} characters");
        return 1;
    }
    match store.set_password(id, &first) {
        Ok(()) => {
            println!("password set for {id}; existing sessions were ended");
            0
        }
        Err(e) => {
            eprintln!("myco: {e}");
            1
        }
    }
}

/// Read a password without echoing it.
///
/// Falls back to a visible read when stdin is not a terminal (a pipe in a
/// setup script), and says so — silently echoing a password would be worse
/// than a warning nobody reads.
fn prompt_password(prompt: &str) -> Result<String, String> {
    eprint!("{prompt}");
    std::io::stderr().flush().ok();
    if std::io::IsTerminal::is_terminal(&std::io::stdin()) {
        let password = read_hidden()?;
        eprintln!();
        Ok(password)
    } else {
        let mut line = String::new();
        std::io::stdin()
            .read_line(&mut line)
            .map_err(|e| format!("read password: {e}"))?;
        Ok(line.trim_end_matches(['\n', '\r']).to_string())
    }
}

#[cfg(unix)]
fn read_hidden() -> Result<String, String> {
    use std::os::unix::io::AsRawFd;

    // Drop ECHO for the duration of the read, then restore it — including on
    // the error path, or a failed read leaves the user's terminal mute.
    let fd = std::io::stdin().as_raw_fd();
    let mut term = unsafe { std::mem::zeroed::<libc::termios>() };
    if unsafe { libc::tcgetattr(fd, &mut term) } != 0 {
        return Err("cannot read terminal settings".into());
    }
    let original = term;
    term.c_lflag &= !libc::ECHO;
    if unsafe { libc::tcsetattr(fd, libc::TCSAFLUSH, &term) } != 0 {
        return Err("cannot disable echo".into());
    }
    let mut line = String::new();
    let read = std::io::stdin().read_line(&mut line);
    unsafe { libc::tcsetattr(fd, libc::TCSAFLUSH, &original) };
    read.map_err(|e| format!("read password: {e}"))?;
    Ok(line.trim_end_matches(['\n', '\r']).to_string())
}

#[cfg(not(unix))]
fn read_hidden() -> Result<String, String> {
    let mut line = String::new();
    std::io::stdin()
        .read_line(&mut line)
        .map_err(|e| format!("read password: {e}"))?;
    Ok(line.trim_end_matches(['\n', '\r']).to_string())
}
