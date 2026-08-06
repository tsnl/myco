//! Credentials and access tokens: the store behind `POST /api/auth/token`.
//!
//! The shape is the OAuth 2.0 *Resource Owner Password Credentials* grant
//! (RFC 6749 §4.3): a client exchanges a username and password for a bearer
//! token with a fixed lifetime, and presents that token on every later
//! request. Choosing a specified grant over a bespoke scheme is the point —
//! the failure modes are known and the client side is boring.
//!
//! ## Where the data lives
//!
//! Two keyspaces, both in memory, with deliberately different durability:
//!
//! - **users** — id → [`StoredUser`] (password hash, timestamps). Snapshotted
//!   to `$MYCO_HOME/v2/auth.json` on every write. A user list that vanished on
//!   restart would make the admin interface pointless.
//! - **tokens** — SHA-256 of the token → [`StoredToken`]. Never persisted. A
//!   restart logs everyone out, which is the behavior you want from a process
//!   that just changed underneath its clients.
//!
//! Tokens are stored *hashed*, exactly as a password would be, because the
//! store is a credential table: reading a snapshot or a memory dump must not
//! hand over a live session. Lookup hashes the presented token and compares
//! the digest, so the plaintext exists only in transit.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::RwLock;

use base64::Engine as _;
use chrono::{DateTime, Duration, Utc};
use myco_api::Author;
use rand::TryRngCore as _;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use webauthn_rs::prelude::{Passkey, PasskeyAuthentication, PasskeyRegistration};

/// How long an issued access token stays valid.
pub const ACCESS_TOKEN_TTL_HOURS: i64 = 12;

/// How long an operator-minted one-time code stays redeemable. Codes are
/// handed to a person out-of-band (a message, a shoulder); a quarter hour
/// covers that without leaving live codes lying around.
pub const CODE_TTL_MINUTES: i64 = 15;

/// How long an in-flight passkey ceremony (a challenge the browser has not
/// answered yet) stays valid.
pub const CEREMONY_TTL_MINUTES: i64 = 5;

/// PBKDF2 work factor. High enough to cost real time on a GPU, low enough
/// that a login stays snappy on a laptop.
///
/// Lowered under `cfg(test)` so the suite is not dominated by key
/// stretching. Safe because the iteration count is stored *in* each hash and
/// read back at verification time — a test-built hash and a real one are
/// both verifiable by the same code.
pub const PBKDF2_ITERATIONS: u32 = 600_000;

/// Shortest password the admin interface will set. A floor, not a policy —
/// the real defense is the KDF above.
pub const MIN_PASSWORD_LEN: usize = 8;

/// Encoded hash: `pbkdf2_sha256$<iterations>$<salt_b64>$<hash_b64>`.
///
/// Self-describing so the work factor can be raised later without stranding
/// existing hashes: verification reads the iteration count out of the stored
/// value rather than assuming today's constant.
const HASH_ALGORITHM: &str = "pbkdf2_sha256";

const B64: base64::engine::general_purpose::GeneralPurpose =
    base64::engine::general_purpose::STANDARD_NO_PAD;

// ---------------------------------------------------------------------------
// Records
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct StoredUser {
    pub id: String,
    pub name: String,
    /// `None` until an administrator sets one; such a user cannot log in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password_hash: Option<String>,
    pub created_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password_set_at: Option<DateTime<Utc>>,
    /// Kept in the store but refused at login, so history stays attributable.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub disabled: bool,
}

impl StoredUser {
    pub fn author(&self) -> Author {
        Author::User {
            id: self.id.clone(),
            name: self.name.clone(),
        }
    }

    pub fn identity(&self) -> myco_api::Identity {
        myco_api::Identity {
            id: self.id.clone(),
            name: self.name.clone(),
        }
    }

    /// A user who could log in right now, given the right password.
    pub fn can_authenticate(&self) -> bool {
        !self.disabled && self.password_hash.is_some()
    }
}

/// A live access token. The plaintext is not here — only what it grants.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredToken {
    pub user_id: String,
    pub issued_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
}

/// A parked passkey-login ceremony: whose it is, its server-side state, and
/// when it stops being answerable.
type PendingAuthentication = (String, PasskeyAuthentication, DateTime<Utc>);

/// An operator-minted one-time code, stored hashed like a token. Memory-only
/// and single-use: it exists to be redeemed against *this* server process —
/// a restart voids it, and the operator mints another.
#[derive(Debug, Clone)]
struct StoredCode {
    user_id: String,
    expires_at: DateTime<Utc>,
}

/// What minting a code hands the operator.
#[derive(Debug, Clone)]
pub struct MintedCode {
    /// The only time the plaintext exists outside the person's hands.
    pub code: String,
    pub user_id: String,
    pub expires_at: DateTime<Utc>,
}

/// What a successful `POST /api/auth/token` hands back.
#[derive(Debug, Clone)]
pub struct IssuedToken {
    /// The only time the plaintext exists outside the client.
    pub access_token: String,
    pub expires_in_seconds: i64,
    pub user: StoredUser,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthError {
    /// Deliberately one variant for "no such user", "wrong password", and
    /// "no password set": a login response must not tell an attacker which
    /// usernames exist.
    InvalidCredentials,
    Disabled,
    UnknownUser(String),
    UserExists(String),
    WeakPassword(String),
    Io(String),
}

impl std::fmt::Display for AuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AuthError::InvalidCredentials => write!(f, "incorrect username or password"),
            AuthError::Disabled => write!(f, "this account is disabled"),
            AuthError::UnknownUser(id) => write!(f, "no such user: {id}"),
            AuthError::UserExists(id) => write!(f, "user already exists: {id}"),
            AuthError::WeakPassword(why) => write!(f, "{why}"),
            AuthError::Io(e) => write!(f, "{e}"),
        }
    }
}

// ---------------------------------------------------------------------------
// Password hashing
// ---------------------------------------------------------------------------

fn random_bytes<const N: usize>() -> [u8; N] {
    let mut buf = [0u8; N];
    rand::rngs::OsRng
        .try_fill_bytes(&mut buf)
        .expect("the OS random source is available");
    buf
}

/// Hash `password` with a fresh 16-byte salt at the default work factor.
pub fn hash_password(password: &str) -> String {
    hash_password_with(password, PBKDF2_ITERATIONS)
}

/// Hash at an explicit work factor. Public so tests can stand up a store
/// without paying production key-stretching on every fixture; nothing else
/// should call it with anything but [`PBKDF2_ITERATIONS`].
pub fn hash_password_with(password: &str, iterations: u32) -> String {
    let salt: [u8; 16] = random_bytes();
    let mut out = [0u8; 32];
    pbkdf2::pbkdf2_hmac::<Sha256>(password.as_bytes(), &salt, iterations, &mut out);
    // The count written here must be the count used above: verification reads
    // it back, so a mismatch makes every hash unverifiable.
    format!(
        "{HASH_ALGORITHM}${iterations}${}${}",
        B64.encode(salt),
        B64.encode(out)
    )
}

/// Verify against an encoded hash. A malformed or unknown-algorithm hash
/// verifies as `false` rather than erroring — a corrupt record must not
/// become an authentication bypass.
pub fn verify_password(password: &str, encoded: &str) -> bool {
    let mut parts = encoded.splitn(4, '$');
    let (Some(algorithm), Some(iterations), Some(salt), Some(expected)) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return false;
    };
    if algorithm != HASH_ALGORITHM {
        return false;
    }
    let (Ok(iterations), Ok(salt), Ok(expected)) = (
        iterations.parse::<u32>(),
        B64.decode(salt),
        B64.decode(expected),
    ) else {
        return false;
    };
    if iterations == 0 || expected.len() != 32 {
        return false;
    }
    let mut candidate = [0u8; 32];
    pbkdf2::pbkdf2_hmac::<Sha256>(password.as_bytes(), &salt, iterations, &mut candidate);
    candidate.ct_eq(expected.as_slice()).into()
}

/// The key a token is filed under. Tokens are opaque and high-entropy, so a
/// plain digest is right here — unlike a password, there is nothing to
/// brute-force.
fn token_digest(token: &str) -> String {
    let mut h = Sha256::new();
    h.update(token.as_bytes());
    format!("{:x}", h.finalize())
}

// ---------------------------------------------------------------------------
// The store
// ---------------------------------------------------------------------------

#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
struct Snapshot {
    #[serde(default)]
    users: Vec<StoredUser>,
}

/// `passkeys.json`: user id → enrolled credentials. Its own file, written by
/// the *server* process only — `auth.json` is the admin CLI's to rewrite, and
/// two writers on one file would clobber each other's changes.
#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
struct PasskeySnapshot {
    #[serde(default)]
    passkeys: HashMap<String, Vec<Passkey>>,
}

/// The in-memory credential store. Cheap to read (an `RwLock` over two maps),
/// and the only thing that can turn a request into an [`Author`].
pub struct AuthStore {
    users: RwLock<HashMap<String, StoredUser>>,
    tokens: RwLock<HashMap<String, StoredToken>>,
    /// One-time codes, hashed → record. Memory-only (see [`StoredCode`]).
    codes: RwLock<HashMap<String, StoredCode>>,
    /// Enrolled passkeys per user, persisted to `passkeys.json`.
    passkeys: RwLock<HashMap<String, Vec<Passkey>>>,
    passkeys_path: Option<PathBuf>,
    /// In-flight registration ceremonies, one per user (the newest wins).
    reg_states: RwLock<HashMap<String, (PasskeyRegistration, DateTime<Utc>)>>,
    /// In-flight login ceremonies, keyed by an opaque ticket.
    auth_states: RwLock<HashMap<String, PendingAuthentication>>,
    /// Where users are snapshotted. `None` for tests that want a pure
    /// in-memory store with no disk footprint at all.
    path: Option<PathBuf>,
    /// PBKDF2 work factor for hashes this store *writes*. Verification always
    /// uses the count recorded in the stored hash, so lowering this in a test
    /// cannot weaken a real stored credential.
    iterations: u32,
}

impl AuthStore {
    /// A store with no backing file: everything is lost on drop.
    pub fn in_memory() -> Self {
        Self {
            users: RwLock::new(HashMap::new()),
            tokens: RwLock::new(HashMap::new()),
            codes: RwLock::new(HashMap::new()),
            passkeys: RwLock::new(HashMap::new()),
            passkeys_path: None,
            reg_states: RwLock::new(HashMap::new()),
            auth_states: RwLock::new(HashMap::new()),
            path: None,
            iterations: PBKDF2_ITERATIONS,
        }
    }

    /// Lower the work factor for hashes this store writes.
    ///
    /// **Tests only.** Key stretching is the whole defense for a stolen
    /// snapshot; a server that calls this has no meaningful password
    /// security.
    pub fn with_work_factor(mut self, iterations: u32) -> Self {
        self.iterations = iterations.max(1);
        self
    }

    /// Default location: `$MYCO_HOME/v2/auth.json`.
    pub fn default_path() -> Result<PathBuf, String> {
        Ok(crate::core::data_root()?.join("auth.json"))
    }

    /// Load the user snapshot from `path`, or start empty if it is absent.
    /// Tokens always start empty — a restart is a logout.
    pub fn open(path: impl Into<PathBuf>) -> Result<Self, AuthError> {
        let path = path.into();
        let users = match std::fs::read(&path) {
            Ok(bytes) => {
                let snap: Snapshot = serde_json::from_slice(&bytes)
                    .map_err(|e| AuthError::Io(format!("parse {}: {e}", path.display())))?;
                snap.users.into_iter().map(|u| (u.id.clone(), u)).collect()
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => HashMap::new(),
            Err(e) => return Err(AuthError::Io(format!("read {}: {e}", path.display()))),
        };
        let passkeys_path = path.with_file_name("passkeys.json");
        let passkeys = match std::fs::read(&passkeys_path) {
            Ok(bytes) => {
                serde_json::from_slice::<PasskeySnapshot>(&bytes)
                    .map_err(|e| AuthError::Io(format!("parse {}: {e}", passkeys_path.display())))?
                    .passkeys
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => HashMap::new(),
            Err(e) => {
                return Err(AuthError::Io(format!(
                    "read {}: {e}",
                    passkeys_path.display()
                )));
            }
        };
        Ok(Self {
            users: RwLock::new(users),
            tokens: RwLock::new(HashMap::new()),
            codes: RwLock::new(HashMap::new()),
            passkeys: RwLock::new(passkeys),
            passkeys_path: Some(passkeys_path),
            reg_states: RwLock::new(HashMap::new()),
            auth_states: RwLock::new(HashMap::new()),
            path: Some(path),
            iterations: PBKDF2_ITERATIONS,
        })
    }

    fn persist_passkeys(&self) -> Result<(), AuthError> {
        let Some(path) = &self.passkeys_path else {
            return Ok(());
        };
        let passkeys = self.passkeys.read().unwrap_or_else(|e| e.into_inner());
        let bytes = serde_json::to_vec_pretty(&PasskeySnapshot {
            passkeys: passkeys.clone(),
        })
        .map_err(|e| AuthError::Io(e.to_string()))?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| AuthError::Io(format!("create {}: {e}", parent.display())))?;
        }
        crate::core::atomically_write(path, &bytes).map_err(AuthError::Io)
    }

    fn persist(&self) -> Result<(), AuthError> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        let users = self.users.read().unwrap_or_else(|e| e.into_inner());
        let mut list: Vec<StoredUser> = users.values().cloned().collect();
        list.sort_by(|a, b| a.id.cmp(&b.id));
        let bytes = serde_json::to_vec_pretty(&Snapshot { users: list })
            .map_err(|e| AuthError::Io(e.to_string()))?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| AuthError::Io(format!("create {}: {e}", parent.display())))?;
        }
        crate::core::atomically_write(path, &bytes).map_err(AuthError::Io)
    }

    // -- administration ----------------------------------------------------

    pub fn add_user(&self, id: &str, name: &str) -> Result<StoredUser, AuthError> {
        let id = normalize_id(id);
        {
            let mut users = self.users.write().unwrap_or_else(|e| e.into_inner());
            if users.contains_key(&id) {
                return Err(AuthError::UserExists(id));
            }
            users.insert(
                id.clone(),
                StoredUser {
                    id: id.clone(),
                    name: name.to_string(),
                    password_hash: None,
                    created_at: Utc::now(),
                    password_set_at: None,
                    disabled: false,
                },
            );
        }
        self.persist()?;
        self.get(&id).ok_or(AuthError::UnknownUser(id))
    }

    /// Set (or replace) a password. Every existing token for the user is
    /// revoked: changing a password must end the sessions it protected,
    /// otherwise a compromised session survives the fix for it.
    pub fn set_password(&self, id: &str, password: &str) -> Result<(), AuthError> {
        if password.chars().count() < MIN_PASSWORD_LEN {
            return Err(AuthError::WeakPassword(format!(
                "password must be at least {MIN_PASSWORD_LEN} characters"
            )));
        }
        let id = normalize_id(id);
        let hash = hash_password_with(password, self.iterations);
        {
            let mut users = self.users.write().unwrap_or_else(|e| e.into_inner());
            let user = users
                .get_mut(&id)
                .ok_or(AuthError::UnknownUser(id.clone()))?;
            user.password_hash = Some(hash);
            user.password_set_at = Some(Utc::now());
        }
        self.revoke_all_for(&id);
        self.persist()
    }

    pub fn set_disabled(&self, id: &str, disabled: bool) -> Result<(), AuthError> {
        let id = normalize_id(id);
        {
            let mut users = self.users.write().unwrap_or_else(|e| e.into_inner());
            let user = users
                .get_mut(&id)
                .ok_or(AuthError::UnknownUser(id.clone()))?;
            user.disabled = disabled;
        }
        if disabled {
            self.revoke_all_for(&id);
        }
        self.persist()
    }

    /// Remove a user entirely. Their sessions die with them; entries they
    /// already authored keep their name, since a transcript is a record of
    /// what happened, not of who currently has an account.
    pub fn remove_user(&self, id: &str) -> Result<(), AuthError> {
        let id = normalize_id(id);
        {
            let mut users = self.users.write().unwrap_or_else(|e| e.into_inner());
            if users.remove(&id).is_none() {
                return Err(AuthError::UnknownUser(id));
            }
        }
        self.revoke_all_for(&id);
        self.persist()
    }

    pub fn get(&self, id: &str) -> Option<StoredUser> {
        let id = normalize_id(id);
        self.users
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(&id)
            .cloned()
    }

    /// Every user, by id.
    pub fn users(&self) -> Vec<StoredUser> {
        let mut list: Vec<StoredUser> = self
            .users
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .cloned()
            .collect();
        list.sort_by(|a, b| a.id.cmp(&b.id));
        list
    }

    pub fn is_empty(&self) -> bool {
        self.users
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .is_empty()
    }

    /// Live (unexpired) tokens per user id — what the admin interface shows
    /// as "sessions".
    pub fn session_counts(&self) -> Vec<(String, usize)> {
        let now = Utc::now();
        let tokens = self.tokens.read().unwrap_or_else(|e| e.into_inner());
        let mut counts: HashMap<String, usize> = HashMap::new();
        for t in tokens.values().filter(|t| t.expires_at > now) {
            *counts.entry(t.user_id.clone()).or_default() += 1;
        }
        let mut list: Vec<(String, usize)> = counts.into_iter().collect();
        list.sort();
        list
    }

    /// Drop every token for `id`. Returns how many were live.
    pub fn revoke_all_for(&self, id: &str) -> usize {
        let id = normalize_id(id);
        let mut tokens = self.tokens.write().unwrap_or_else(|e| e.into_inner());
        let before = tokens.len();
        tokens.retain(|_, t| t.user_id != id);
        before - tokens.len()
    }

    // -- one-time codes ----------------------------------------------------

    /// Mint a single-use login code for `id`. Operator-only by construction:
    /// the route that calls this requires the server's own identity, and the
    /// admin CLI reaches that route with the operator token.
    pub fn mint_code(&self, id: &str) -> Result<MintedCode, AuthError> {
        let user = self
            .get(id)
            .ok_or_else(|| AuthError::UnknownUser(normalize_id(id)))?;
        if user.disabled {
            return Err(AuthError::Disabled);
        }
        let code = random_code();
        let expires_at = Utc::now() + Duration::minutes(CODE_TTL_MINUTES);
        let mut codes = self.codes.write().unwrap_or_else(|e| e.into_inner());
        // One live code per user: minting again replaces, so a mis-sent code
        // dies the moment its replacement exists.
        codes.retain(|_, c| c.user_id != user.id);
        codes.insert(
            token_digest(&code),
            StoredCode {
                user_id: user.id.clone(),
                expires_at,
            },
        );
        Ok(MintedCode {
            code,
            user_id: user.id,
            expires_at,
        })
    }

    /// Redeem a one-time code for a token. Burns the code on success; a
    /// wrong guess burns nothing but learns nothing either — one error for
    /// unknown user, wrong code, and expired code alike.
    pub fn redeem_code(&self, id: &str, code: &str) -> Result<IssuedToken, AuthError> {
        let id = normalize_id(id);
        let digest = token_digest(code.trim());
        let record = {
            let mut codes = self.codes.write().unwrap_or_else(|e| e.into_inner());
            let now = Utc::now();
            codes.retain(|_, c| c.expires_at > now);
            match codes.get(&digest) {
                Some(c) if c.user_id == id => {
                    let c = c.clone();
                    codes.remove(&digest);
                    Some(c)
                }
                _ => None,
            }
        };
        let Some(_record) = record else {
            return Err(AuthError::InvalidCredentials);
        };
        let user = self.get(&id).ok_or(AuthError::InvalidCredentials)?;
        if user.disabled {
            return Err(AuthError::Disabled);
        }
        Ok(self.issue(&user))
    }

    // -- passkeys ----------------------------------------------------------

    pub fn passkeys_for(&self, id: &str) -> Vec<Passkey> {
        self.passkeys
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(&normalize_id(id))
            .cloned()
            .unwrap_or_default()
    }

    /// Every user's passkey count, for the admin listing.
    pub fn passkey_counts(&self) -> HashMap<String, usize> {
        self.passkeys
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|(id, list)| (id.clone(), list.len()))
            .collect()
    }

    /// Record a freshly enrolled credential.
    pub fn add_passkey(&self, id: &str, passkey: Passkey) -> Result<usize, AuthError> {
        let id = normalize_id(id);
        let count = {
            let mut passkeys = self.passkeys.write().unwrap_or_else(|e| e.into_inner());
            let list = passkeys.entry(id).or_default();
            list.push(passkey);
            list.len()
        };
        self.persist_passkeys()?;
        Ok(count)
    }

    /// Apply a successful authentication's credential update (sign counter,
    /// backup state) to the stored copy.
    pub fn update_passkey(
        &self,
        id: &str,
        result: &webauthn_rs::prelude::AuthenticationResult,
    ) -> Result<(), AuthError> {
        let changed = {
            let mut passkeys = self.passkeys.write().unwrap_or_else(|e| e.into_inner());
            passkeys
                .get_mut(&normalize_id(id))
                .map(|list| {
                    list.iter_mut()
                        .filter_map(|p| p.update_credential(result))
                        .any(|updated| updated)
                })
                .unwrap_or(false)
        };
        if changed {
            self.persist_passkeys()
        } else {
            Ok(())
        }
    }

    /// Forget every credential for `id` (lost or compromised authenticator).
    pub fn clear_passkeys(&self, id: &str) -> Result<usize, AuthError> {
        let removed = self
            .passkeys
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&normalize_id(id))
            .map(|l| l.len())
            .unwrap_or(0);
        self.persist_passkeys()?;
        Ok(removed)
    }

    // -- passkey ceremony state -------------------------------------------

    /// Park a registration challenge for `id` (the newest wins — a user who
    /// restarts the ceremony should not race their own abandoned attempt).
    pub fn store_registration(&self, id: &str, state: PasskeyRegistration) {
        let mut states = self.reg_states.write().unwrap_or_else(|e| e.into_inner());
        let now = Utc::now();
        states.retain(|_, (_, exp)| *exp > now);
        states.insert(
            normalize_id(id),
            (state, now + Duration::minutes(CEREMONY_TTL_MINUTES)),
        );
    }

    pub fn take_registration(&self, id: &str) -> Option<PasskeyRegistration> {
        let mut states = self.reg_states.write().unwrap_or_else(|e| e.into_inner());
        let (state, expires) = states.remove(&normalize_id(id))?;
        (expires > Utc::now()).then_some(state)
    }

    /// Park a login challenge under a fresh opaque ticket.
    pub fn store_authentication(&self, id: &str, state: PasskeyAuthentication) -> String {
        let raw: [u8; 16] = random_bytes();
        let ticket = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(raw);
        let mut states = self.auth_states.write().unwrap_or_else(|e| e.into_inner());
        let now = Utc::now();
        states.retain(|_, (_, _, exp)| *exp > now);
        states.insert(
            ticket.clone(),
            (
                normalize_id(id),
                state,
                now + Duration::minutes(CEREMONY_TTL_MINUTES),
            ),
        );
        ticket
    }

    pub fn take_authentication(&self, ticket: &str) -> Option<(String, PasskeyAuthentication)> {
        let mut states = self.auth_states.write().unwrap_or_else(|e| e.into_inner());
        let (id, state, expires) = states.remove(ticket)?;
        (expires > Utc::now()).then_some((id, state))
    }

    // -- the grant ---------------------------------------------------------

    /// The password grant: credentials in, bearer token out.
    ///
    /// Every failure returns [`AuthError::InvalidCredentials`] except an
    /// explicitly disabled account, and an unknown username still pays for a
    /// PBKDF2 verification against a dummy hash — otherwise response timing
    /// enumerates the user list.
    pub fn login(&self, id: &str, password: &str) -> Result<IssuedToken, AuthError> {
        let user = self.get(id);
        let Some(user) = user else {
            // Constant-ish work for a nonexistent user.
            let _ = verify_password(password, &dummy_hash());
            return Err(AuthError::InvalidCredentials);
        };
        if user.disabled {
            return Err(AuthError::Disabled);
        }
        let Some(hash) = &user.password_hash else {
            let _ = verify_password(password, &dummy_hash());
            return Err(AuthError::InvalidCredentials);
        };
        if !verify_password(password, hash) {
            return Err(AuthError::InvalidCredentials);
        }
        Ok(self.issue(&user))
    }

    /// Mint a token for a user without presenting a password.
    ///
    /// The caller must already have authority — this is how the server hands
    /// its own tools a credential at boot, not a login path.
    pub fn issue_for(&self, id: &str) -> Option<IssuedToken> {
        let user = self.get(id)?;
        (!user.disabled).then(|| self.issue(&user))
    }

    /// Mint a token for an already-authenticated user.
    fn issue(&self, user: &StoredUser) -> IssuedToken {
        let raw: [u8; 32] = random_bytes();
        let access_token = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(raw);
        let now = Utc::now();
        let expires_at = now + Duration::hours(ACCESS_TOKEN_TTL_HOURS);
        self.tokens
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(
                token_digest(&access_token),
                StoredToken {
                    user_id: user.id.clone(),
                    issued_at: now,
                    expires_at,
                },
            );
        IssuedToken {
            access_token,
            expires_in_seconds: ACCESS_TOKEN_TTL_HOURS * 3600,
            user: user.clone(),
        }
    }

    /// Resolve a presented bearer token to its user, or `None` if it is
    /// unknown, expired, or belongs to an account that has since been
    /// disabled or removed.
    pub fn authenticate_token(&self, presented: &str) -> Option<StoredUser> {
        let digest = token_digest(presented);
        let record = self
            .tokens
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(&digest)
            .cloned()?;
        if record.expires_at <= Utc::now() {
            // Expired: drop it on the way past rather than leaving it to rot.
            self.tokens
                .write()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&digest);
            return None;
        }
        let user = self.get(&record.user_id)?;
        (!user.disabled).then_some(user)
    }

    /// Drop expired tokens. Nothing depends on this for correctness —
    /// [`Self::authenticate_token`] already refuses them — it just keeps the
    /// map from growing without bound.
    pub fn sweep_expired(&self) -> usize {
        let now = Utc::now();
        let mut tokens = self.tokens.write().unwrap_or_else(|e| e.into_inner());
        let before = tokens.len();
        tokens.retain(|_, t| t.expires_at > now);
        before - tokens.len()
    }
}

/// Ids are handles: compared case-insensitively, stored lower-cased, so
/// `Ada` and `ada` cannot become two accounts.
pub fn normalize_id(id: &str) -> String {
    id.trim().to_lowercase()
}

/// A one-time code a person can read over a shoulder and type: ten base32
/// characters (50 bits — unguessable within its TTL), grouped for dictation.
fn random_code() -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
    let raw: [u8; 10] = random_bytes();
    let chars: String = raw
        .iter()
        .map(|b| ALPHABET[(*b as usize) % ALPHABET.len()] as char)
        .collect();
    format!("{}-{}", &chars[..5], &chars[5..])
}

/// A well-formed hash of an unguessable value, used to spend the same time on
/// a missing user as on a real one.
fn dummy_hash() -> String {
    format!(
        "{HASH_ALGORITHM}${PBKDF2_ITERATIONS}${}${}",
        B64.encode([0u8; 16]),
        B64.encode([0u8; 32])
    )
}

#[cfg(test)]
mod tests;
