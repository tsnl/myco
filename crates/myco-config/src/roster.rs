//! The user roster: who is allowed to drive this server, read from
//! `$MYCO_HOME/v2/server.toml`.
//!
//! Sessions are attributed — every [`myco_api::Entry`] names its author — so
//! the runtime needs a real identity before it can record anything. There is
//! deliberately **no fallback**: a missing, empty, or unmatched roster is a
//! startup error, not a guess. Inventing an identity from `$USER` would write
//! a name nobody registered into a durable, shareable transcript.
//!
//! The roster is a closed list on purpose: it answers "which names may appear
//! as an author in this store". It holds **no credentials** — passwords and
//! access tokens live in `myco_auth::AuthStore`, which the server seeds from
//! this list at startup. Editing a name here is a config change; granting
//! someone HTTP access is an administrative one.

use std::path::{Path, PathBuf};

use myco_api::Author;

/// Where the roster lives, unless the caller overrides it.
///
/// `$MYCO_SERVER_CONFIG` → `$MYCO_HOME/v2/server.toml`.
pub fn resolve_roster_path(
    override_path: Option<PathBuf>,
    env: &impl Fn(&str) -> Option<String>,
) -> Result<PathBuf, String> {
    if let Some(p) = override_path {
        return Ok(p);
    }
    if let Some(p) = env("MYCO_SERVER_CONFIG") {
        return Ok(PathBuf::from(p));
    }
    Ok(myco_core::data_root()?.join("server.toml"))
}

/// One registered person. `id` is the stable handle stored in transcripts;
/// `name` is what humans and the model see.
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

    pub fn author(&self) -> Author {
        Author::User {
            id: self.id.clone(),
            name: self.display_name().to_string(),
        }
    }
}

/// `server.toml` as written on disk.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct FileRoster {
    #[serde(default)]
    pub users: Vec<RosterUser>,
}

/// The validated roster, plus which entry this process is acting as.
#[derive(Debug, Clone)]
pub struct Roster {
    pub path: PathBuf,
    users: Vec<RosterUser>,
    local: usize,
}

/// What a usable `server.toml` looks like, quoted in every startup error so
/// the fix is in the message rather than the manual.
pub const EXAMPLE_SERVER_TOML: &str = "\
# Everyone who may drive this server. Sessions record who wrote each
# entry, so there is no anonymous access.
[[users]]
id = \"ada\"            # matches $USER (or $MYCO_USER) for local runs
name = \"Ada Lovelace\" # optional; defaults to the id
";

impl Roster {
    /// Every registered user, in file order.
    pub fn users(&self) -> &[RosterUser] {
        &self.users
    }

    /// The entry this process runs as.
    pub fn local(&self) -> &RosterUser {
        &self.users[self.local]
    }

    /// The author for turns this process drives locally.
    pub fn local_author(&self) -> Author {
        self.local().author()
    }

    pub fn get(&self, id: &str) -> Option<&RosterUser> {
        self.users.iter().find(|u| u.id == id)
    }

    /// Validate a parsed file and pick the local identity.
    ///
    /// The local user is `$MYCO_USER` → `$USER` → `$USERNAME`, matched against
    /// `id`. Every failure names the roster path and the ids it does contain.
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
            if let Some(dup) = users[..i].iter().find(|u| u.id == user.id) {
                return Err(format!(
                    "{}: duplicate user id {:?} — ids identify authors in stored sessions and must be unique",
                    path.display(),
                    dup.id
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

/// Read and parse `server.toml`. A missing file is an error, and the error
/// carries the file we wanted — this is the message a first-run user sees.
pub fn load_file_roster(path: &Path) -> Result<FileRoster, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(format!(
                "no server roster at {} — myco records who wrote every session entry, so it \
                 will not start without one. Create the file:\n\n{EXAMPLE_SERVER_TOML}",
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
        assert_eq!(
            r.local_author(),
            Author::User {
                id: "ada".into(),
                name: "Ada Lovelace".into()
            }
        );
        // A name is optional; the id stands in for it.
        assert_eq!(r.get("grace").unwrap().display_name(), "grace");
    }

    #[test]
    fn myco_user_beats_the_os_user() {
        let r = roster(TWO_USERS, &[("MYCO_USER", "grace"), ("USER", "ada")]).unwrap();
        assert_eq!(r.local().id, "grace");
    }

    /// The whole point: an unregistered operator must not be invented into
    /// existence, because the name they'd be given lands in a stored session.
    #[test]
    fn an_unregistered_user_is_an_error_not_a_default() {
        let err = roster(TWO_USERS, &[("USER", "mallory")]).unwrap_err();
        assert!(err.contains("mallory"), "{err}");
        assert!(err.contains("ada, grace"), "{err}");
    }

    #[test]
    fn an_unidentifiable_process_is_an_error() {
        let err = roster(TWO_USERS, &[]).unwrap_err();
        assert!(err.contains("MYCO_USER"), "{err}");
    }

    #[test]
    fn an_empty_roster_is_rejected_with_an_example() {
        let err = roster("", &[("USER", "ada")]).unwrap_err();
        assert!(err.contains("no users defined"), "{err}");
        assert!(err.contains("[[users]]"), "{err}");
    }

    #[test]
    fn duplicate_and_malformed_ids_are_rejected() {
        let dup = roster(
            "[[users]]\nid = \"ada\"\n[[users]]\nid = \"ada\"\n",
            &[("USER", "ada")],
        )
        .unwrap_err();
        assert!(dup.contains("duplicate user id"), "{dup}");

        let spacey = roster("[[users]]\nid = \"ada l\"\n", &[("USER", "ada l")]).unwrap_err();
        assert!(spacey.contains("whitespace"), "{spacey}");

        let empty = roster("[[users]]\nid = \"\"\n", &[("USER", "")]).unwrap_err();
        assert!(empty.contains("empty id"), "{empty}");
    }

    #[test]
    fn a_missing_file_names_itself_and_shows_the_fix() {
        let err = load_file_roster(Path::new("/nonexistent/myco/server.toml")).unwrap_err();
        assert!(err.contains("/nonexistent/myco/server.toml"), "{err}");
        assert!(err.contains("[[users]]"), "{err}");
    }

    #[test]
    fn the_roster_path_falls_back_from_override_to_env_to_home() {
        let over = resolve_roster_path(Some(PathBuf::from("/tmp/r.toml")), &env_of(&[])).unwrap();
        assert_eq!(over, PathBuf::from("/tmp/r.toml"));
        let from_env =
            resolve_roster_path(None, &env_of(&[("MYCO_SERVER_CONFIG", "/tmp/e.toml")])).unwrap();
        assert_eq!(from_env, PathBuf::from("/tmp/e.toml"));
        let _home = myco_test_support::temp_home("roster-path");
        let default = resolve_roster_path(None, &env_of(&[])).unwrap();
        assert!(default.ends_with("v2/server.toml"), "{default:?}");
    }
}
