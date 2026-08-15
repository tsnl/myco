//! `server.toml`: everything an operator configures, in one file.
//!
//! It started as the user roster and grew the rest as the server did —
//! `[[users]]`, `[passkeys]`, and the model catalog's `model` /
//! `[gateways]` / `[models]`. One file, because there is one thing to
//! configure, and one parse, because a second file is a second chance for
//! the two to disagree. (It is `server.toml` rather than `config.toml`
//! because v1 owns that name in the shared `~/.myco`.)
//!
//! [`ServerConfig`] is the file as written; [`Roster`] is what the users
//! half becomes once validated — who may appear as a principal, and which
//! of them this process runs as. Instances are attributed, so the server
//! needs a real identity before it records anything, and there is
//! deliberately **no fallback**: a missing, empty, or unmatched roster is a
//! startup error, not a guess. The roster holds **no credentials** — those
//! live in [`crate::auth::AuthStore`], which the server seeds from this
//! list at startup. Ported from v2 (`main-v2:src/config/roster.rs`).

use std::path::{Path, PathBuf};

/// Where the config lives: `$MYCO_SERVER_CONFIG` → `$MYCO_HOME/server.toml`.
pub fn resolve_config_path(env: &impl Fn(&str) -> Option<String>) -> Result<PathBuf, String> {
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

/// `server.toml` as written on disk — the whole file, before any of it is
/// validated. Every section is optional to *parse*; what is required is
/// decided where it is used (a roster with no users is refused, an absent
/// One `[[hosts]]` entry: a machine to dial at startup. Each becomes a
/// `host` instance owned by the operator; the command runs under `sh -c`
/// and its stdio becomes the provider stream.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct HostEntry {
    /// The instance's title — what the tree shows. The provider announces
    /// its own name too; this one is the operator's word for it.
    pub name: String,
    /// The line that reaches a provider's stdio, e.g. `ssh box myco-hostd`.
    pub command: String,
}

/// `[passkeys]` takes the localhost defaults, an empty catalog is a
/// modelless workspace).
#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct ServerConfig {
    #[serde(default)]
    pub users: Vec<RosterUser>,
    /// Optional `[[hosts]]` entries — machines to dial at startup.
    #[serde(default)]
    pub hosts: Vec<HostEntry>,
    /// Optional `[passkeys]` section; defaults serve the localhost story.
    #[serde(default)]
    pub passkeys: PasskeySettings,
    /// The model catalog: `model` + `[gateways]` + `[models]`, resolved by
    /// `myco_models::resolve_catalog` at boot.
    #[serde(flatten)]
    pub catalog: myco_models::CatalogFile,
}

/// `[passkeys]` in `server.toml`: the WebAuthn relying party.
///
/// The defaults bind passkeys to `localhost` with any port allowed, which
/// covers the doctrine's remote pattern (an SSH tunnel — the browser still
/// sees `localhost`, a secure context) as well as any local dev proxy. Set
/// both fields if the client is ever browsed at a real HTTPS hostname; the
/// rp_id must be a domain (passkeys cannot bind to an IP address), and
/// changing it strands passkeys enrolled under the old one.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PasskeySettings {
    #[serde(default = "default_rp_id")]
    pub rp_id: String,
    /// The origin the client is browsed at, scheme included.
    #[serde(default = "default_rp_origin")]
    pub origin: String,
}

fn default_rp_id() -> String {
    "localhost".into()
}

fn default_rp_origin() -> String {
    "http://localhost".into()
}

impl Default for PasskeySettings {
    fn default() -> Self {
        Self {
            rp_id: default_rp_id(),
            origin: default_rp_origin(),
        }
    }
}

/// The validated file: the closed list of principals, which entry this
/// process is acting as (the operator), and the sections the server hands
/// on to whoever owns them. Named for the half it enforces — a roster is
/// the only part of `server.toml` this module refuses on.
#[derive(Debug, Clone)]
pub struct Roster {
    pub path: PathBuf,
    users: Vec<RosterUser>,
    local: usize,
    /// The WebAuthn relying party (`[passkeys]` in the same file).
    pub passkeys: PasskeySettings,
    /// The unresolved model catalog (`model` / `[gateways]` / `[models]`).
    pub catalog: myco_models::CatalogFile,
    /// Machines to dial at startup (`[[hosts]]` in the same file).
    pub hosts: Vec<HostEntry>,
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
        file: ServerConfig,
        env: &impl Fn(&str) -> Option<String>,
    ) -> Result<Self, String> {
        let ServerConfig {
            users,
            hosts,
            passkeys,
            catalog,
        } = file;
        for host in &hosts {
            if host.name.trim().is_empty() || host.command.trim().is_empty() {
                return Err(format!(
                    "{}: a [[hosts]] entry needs both name and command \
                     (e.g. name = \"buildbox\", command = \"ssh buildbox myco-hostd\")",
                    path.display()
                ));
            }
        }
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

        Ok(Self {
            path,
            users,
            local,
            passkeys,
            catalog,
            hosts,
        })
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
pub fn load_config(path: &Path) -> Result<ServerConfig, String> {
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
    parse_config_str(&text).map_err(|e| format!("{}: {e}", path.display()))
}

pub fn parse_config_str(text: &str) -> Result<ServerConfig, String> {
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
            parse_config_str(text).unwrap(),
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
    fn the_passkeys_section_is_optional_with_localhost_defaults() {
        let r = roster(TWO_USERS, &[("USER", "ada")]).unwrap();
        assert_eq!(r.passkeys.rp_id, "localhost");
        assert_eq!(r.passkeys.origin, "http://localhost");

        let configured = format!(
            "{TWO_USERS}\n[passkeys]\nrp_id = \"myco.example\"\norigin = \"https://myco.example\"\n"
        );
        let r = roster(&configured, &[("USER", "ada")]).unwrap();
        assert_eq!(r.passkeys.rp_id, "myco.example");
        assert_eq!(r.passkeys.origin, "https://myco.example");
    }

    #[test]
    fn the_model_catalog_rides_the_same_file() {
        // Top-level keys (`model`) must precede any table in TOML.
        let with_models = format!(
            "model = \"fast\"\n{TWO_USERS}\n[gateways.g]\nprotocol = \"openai-completions\"\n\
             base_url = \"https://example.test/v1\"\n\n[models.fast]\ngateway = \"g\"\n\
             context_window = 100000\n"
        );
        let r = roster(&with_models, &[("USER", "ada")]).unwrap();
        assert_eq!(r.catalog.model.as_deref(), Some("fast"));
        assert!(r.catalog.models.contains_key("fast"));
        assert!(r.catalog.gateways.contains_key("g"));

        // A roster without model tables is still a roster.
        let bare = roster(TWO_USERS, &[("USER", "ada")]).unwrap();
        assert!(bare.catalog.models.is_empty());
    }

    #[test]
    fn a_missing_file_names_itself_and_shows_the_fix() {
        let err = load_config(Path::new("/nonexistent/myco/server.toml")).unwrap_err();
        assert!(err.contains("/nonexistent/myco/server.toml"), "{err}");
        assert!(err.contains("[[users]]"), "{err}");
    }
}
