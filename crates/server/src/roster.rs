//! The user roster: who is allowed to drive this server, read from
//! `$MYCO_HOME/v3/server.toml`. Ported from v2 (`main-v2:src/config/roster.rs`).
//!
//! Instances are attributed — every verb call names its principal — so the
//! server needs a real identity before it records anything. There is
//! deliberately **no fallback**: a missing, empty, or unmatched roster is a
//! startup error, not a guess. The roster is a closed list on purpose: it
//! answers "which names may appear as a principal". It holds **no
//! credentials** — those live in [`crate::auth::AuthStore`], which the
//! server seeds from this list at startup.

use std::path::{Path, PathBuf};

/// Where the roster lives: `$MYCO_SERVER_CONFIG` → `$MYCO_HOME/v3/server.toml`.
pub fn resolve_roster_path(env: &impl Fn(&str) -> Option<String>) -> Result<PathBuf, String> {
    if let Some(p) = env("MYCO_SERVER_CONFIG") {
        return Ok(PathBuf::from(p));
    }
    Ok(crate::util::data_root()?.join("server.toml"))
}

/// One registered person. `id` is the stable handle; `name` is what humans
/// and the model see.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RosterUser {
    pub id: String,
    /// Display name. Defaults to `id` when the file omits it.
    #[serde(default)]
    pub name: Option<String>,
}

impl RosterUser {
    pub fn display_name(&self) -> &str {
        self.name.as_deref().unwrap_or(&self.id)
    }
}

/// `server.toml` as written on disk.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct FileRoster {
    #[serde(default)]
    pub users: Vec<RosterUser>,
}

/// The validated roster, plus which entry this process is acting as — the
/// operator.
#[derive(Debug, Clone)]
pub struct Roster {
    pub path: PathBuf,
    users: Vec<RosterUser>,
    local: usize,
}

/// What a usable `server.toml` looks like, quoted in every startup error so
/// the fix is in the message rather than the manual.
pub const EXAMPLE_SERVER_TOML: &str = "\
# Everyone who may drive this server. Instances record who did what,
# so there is no anonymous access.
[[users]]
id = \"ada\"            # matches $USER (or $MYCO_USER) for local runs
name = \"Ada Lovelace\" # optional; defaults to the id
";

impl Roster {
    /// Every registered user, in file order.
    pub fn users(&self) -> &[RosterUser] {
        &self.users
    }

    /// The entry this process runs as — the operator.
    pub fn local(&self) -> &RosterUser {
        &self.users[self.local]
    }

    /// Validate a parsed file and pick the local identity
    /// (`$MYCO_USER` → `$USER` → `$USERNAME`, matched against `id`).
    pub fn resolve(
        path: PathBuf,
        file: FileRoster,
        env: &impl Fn(&str) -> Option<String>,
    ) -> Result<Self, String> {
        let users = file.users;
        if users.is_empty() {
            return Err(format!(
                "no users defined in {} — add at least one [[users]] entry:\n\n{EXAMPLE_SERVER_TOML}",
                path.display()
            ));
        }
        for user in &users {
            if user.id.trim().is_empty() {
                return Err(format!(
                    "{}: a [[users]] entry has an empty id",
                    path.display()
                ));
            }
            if user.id.chars().any(char::is_whitespace) {
                return Err(format!(
                    "{}: user id {:?} contains whitespace; ids are handles, like a login name",
                    path.display(),
                    user.id
                ));
            }
        }
        for (i, user) in users.iter().enumerate() {
            if users[..i].iter().any(|u| u.id == user.id) {
                return Err(format!(
                    "{}: duplicate user id {:?} — ids identify principals and must be unique",
                    path.display(),
                    user.id
                ));
            }
        }
        let Some(who) = env("MYCO_USER")
            .or_else(|| env("USER"))
            .or_else(|| env("USERNAME"))
        else {
            return Err(format!(
                "cannot tell which user this process runs as: none of $MYCO_USER, $USER, \
                 $USERNAME is set. Set $MYCO_USER to one of the ids in {} ({}).",
                path.display(),
                id_list(&users)
            ));
        };
        let local = users.iter().position(|u| u.id == who).ok_or_else(|| {
            format!(
                "{who:?} is not in the roster at {} (known ids: {}). Add a [[users]] entry \
                 for it, or set $MYCO_USER to one of those ids.",
                path.display(),
                id_list(&users)
            )
        })?;

        Ok(Self { path, users, local })
    }
}

fn id_list(users: &[RosterUser]) -> String {
    users
        .iter()
        .map(|u| u.id.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Read and parse `server.toml`. A missing file is an error carrying the
/// file we wanted — this is the message a first-run user sees.
pub fn load_file_roster(path: &Path) -> Result<FileRoster, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(format!(
                "no server roster at {} — myco records who did what, so it will not start \
                 without one. Create the file:\n\n{EXAMPLE_SERVER_TOML}",
                path.display()
            ));
        }
        Err(e) => return Err(format!("read {}: {e}", path.display())),
    };
    parse_file_roster_str(&text).map_err(|e| format!("{}: {e}", path.display()))
}

pub fn parse_file_roster_str(text: &str) -> Result<FileRoster, String> {
    toml::from_str(text).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> + use<> {
        let pairs: Vec<(String, String)> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |key: &str| pairs.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone())
    }

    fn roster(text: &str, env_pairs: &[(&str, &str)]) -> Result<Roster, String> {
        Roster::resolve(
            PathBuf::from("/cfg/server.toml"),
            parse_file_roster_str(text).unwrap(),
            &env_of(env_pairs),
        )
    }

    const TWO_USERS: &str = r#"
[[users]]
id = "ada"
name = "Ada Lovelace"

[[users]]
id = "grace"
"#;

    #[test]
    fn the_local_user_is_the_roster_entry_matching_the_environment() {
        let r = roster(TWO_USERS, &[("USER", "ada")]).unwrap();
        assert_eq!(r.local().id, "ada");
        // A name is optional; the id stands in for it.
        let grace = r.users().iter().find(|u| u.id == "grace").unwrap();
        assert_eq!(grace.display_name(), "grace");
    }

    #[test]
    fn myco_user_beats_the_os_user() {
        let r = roster(TWO_USERS, &[("MYCO_USER", "grace"), ("USER", "ada")]).unwrap();
        assert_eq!(r.local().id, "grace");
    }

    /// The whole point: an unregistered operator must not be invented into
    /// existence, because the name they'd be given lands in stored history.
    #[test]
    fn an_unregistered_user_is_an_error_not_a_default() {
        let err = roster(TWO_USERS, &[("USER", "mallory")]).unwrap_err();
        assert!(err.contains("mallory"), "{err}");
        assert!(err.contains("ada, grace"), "{err}");
    }

    #[test]
    fn an_empty_or_malformed_roster_is_rejected_with_an_example() {
        let err = roster("", &[("USER", "ada")]).unwrap_err();
        assert!(err.contains("no users defined"), "{err}");
        assert!(err.contains("[[users]]"), "{err}");

        let dup = roster(
            "[[users]]\nid = \"ada\"\n[[users]]\nid = \"ada\"\n",
            &[("USER", "ada")],
        )
        .unwrap_err();
        assert!(dup.contains("duplicate user id"), "{dup}");

        let spacey = roster("[[users]]\nid = \"ada l\"\n", &[("USER", "ada l")]).unwrap_err();
        assert!(spacey.contains("whitespace"), "{spacey}");
    }

    #[test]
    fn a_missing_file_names_itself_and_shows_the_fix() {
        let err = load_file_roster(Path::new("/nonexistent/myco/server.toml")).unwrap_err();
        assert!(err.contains("/nonexistent/myco/server.toml"), "{err}");
        assert!(err.contains("[[users]]"), "{err}");
    }
}
