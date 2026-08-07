//! Credentials and access tokens: the store behind `POST /api/auth/token`.
//! Ported from v2 (`main-v2:src/auth/mod.rs`), which got this right; the
//! passkey half of the store follows in its own PR.
//!
//! Two ways in, ranked by strength — and no passwords at all:
//!
//! - **One-time codes**, the bootstrap and recovery path. The operator mints
//!   one; it redeems exactly once at the token endpoint (`grant_type=code`)
//!   for a bearer token with a fixed lifetime.
//! - **Passkeys**, the steady state (arrives with the WebAuthn PRs).
//!
//! ## Where the data lives
//!
//! - **users** — id → [`StoredUser`] (name, disabled flag, timestamps).
//!   Snapshotted to `$MYCO_HOME/v3/auth.json` on every write, so a disabled
//!   account stays disabled across restarts.
//! - **tokens, codes** — never persisted. A restart logs everyone out and
//!   voids every outstanding code, which is the behavior you want from a
//!   process that just changed underneath its clients.
//!
//! Tokens and codes are stored *hashed*, because the store is a credential
//! table: reading a memory dump must not hand over a live session. Lookup
//! hashes the presented value and compares digests, so the plaintext exists
//! only in transit.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::RwLock;

use base64::Engine as _;
use chrono::{DateTime, Duration, Utc};
use rand::TryRngCore as _;
use sha2::{Digest, Sha256};

/// How long an issued access token stays valid.
pub const ACCESS_TOKEN_TTL_HOURS: i64 = 12;

/// How long an operator-minted one-time code stays redeemable. Codes are
/// handed to a person out-of-band (a message, a shoulder); a quarter hour
/// covers that without leaving live codes lying around.
pub const CODE_TTL_MINUTES: i64 = 15;

// ---------------------------------------------------------------------------
// Records
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct StoredUser {
    pub id: String,
    pub name: String,
    pub created_at: DateTime<Utc>,
    /// Kept in the store but refused at login, so history stays attributable.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub disabled: bool,
}

/// A live access token. The plaintext is not here — only what it grants.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredToken {
    pub user_id: String,
    pub issued_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
}

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
    /// Deliberately one variant for "no such user", "wrong code", and
    /// "expired code": a login response must not tell an attacker which
    /// usernames exist.
    InvalidCredentials,
    Disabled,
    UnknownUser(String),
    UserExists(String),
    Io(String),
}

impl std::fmt::Display for AuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AuthError::InvalidCredentials => write!(f, "incorrect username or code"),
            AuthError::Disabled => write!(f, "this account is disabled"),
            AuthError::UnknownUser(id) => write!(f, "no such user: {id}"),
            AuthError::UserExists(id) => write!(f, "user already exists: {id}"),
            AuthError::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for AuthError {}

fn random_bytes<const N: usize>() -> [u8; N] {
    let mut buf = [0u8; N];
    rand::rngs::OsRng
        .try_fill_bytes(&mut buf)
        .expect("the OS random source is available");
    buf
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

/// The in-memory credential store. Cheap to read (an `RwLock` over maps),
/// and the only thing that can turn a bearer token into a person.
pub struct AuthStore {
    users: RwLock<HashMap<String, StoredUser>>,
    tokens: RwLock<HashMap<String, StoredToken>>,
    /// One-time codes, hashed → record. Memory-only (see [`StoredCode`]).
    codes: RwLock<HashMap<String, StoredCode>>,
    /// Where users are snapshotted. `None` for tests that want a pure
    /// in-memory store with no disk footprint at all.
    path: Option<PathBuf>,
}

impl AuthStore {
    /// A store with no backing file: everything is lost on drop.
    pub fn in_memory() -> Self {
        Self {
            users: RwLock::new(HashMap::new()),
            tokens: RwLock::new(HashMap::new()),
            codes: RwLock::new(HashMap::new()),
            path: None,
        }
    }

    /// Default location: `$MYCO_HOME/v3/auth.json`.
    pub fn default_path() -> Result<PathBuf, String> {
        Ok(crate::util::data_root()?.join("auth.json"))
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
        Ok(Self {
            users: RwLock::new(users),
            tokens: RwLock::new(HashMap::new()),
            codes: RwLock::new(HashMap::new()),
            path: Some(path),
        })
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
        crate::util::atomically_write(path, &bytes).map_err(AuthError::Io)
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
                    created_at: Utc::now(),
                    disabled: false,
                },
            );
        }
        self.persist()?;
        self.get(&id).ok_or(AuthError::UnknownUser(id))
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

    /// Remove a user entirely: sessions and outstanding codes die with
    /// them. Entries they already authored keep their name, since a
    /// transcript is a record of what happened, not of who currently has an
    /// account. (Passkey forgetting joins this in the passkey PR.)
    pub fn remove_user(&self, id: &str) -> Result<(), AuthError> {
        let id = normalize_id(id);
        {
            let mut users = self.users.write().unwrap_or_else(|e| e.into_inner());
            if users.remove(&id).is_none() {
                return Err(AuthError::UnknownUser(id));
            }
        }
        self.revoke_all_for(&id);
        self.codes
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|_, c| c.user_id != id);
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

    /// Live (unexpired) tokens per user id — what the admin surface shows
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
    /// the route that calls this requires the operator's identity.
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

    // -- the grant ---------------------------------------------------------

    /// Mint a token for a user without presenting a credential.
    ///
    /// The caller must already have authority — this is how the server hands
    /// the operator a boot token, not a login path.
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

    #[cfg(test)]
    pub(crate) fn expire_all_tokens_for_test(&self) {
        let mut tokens = self.tokens.write().unwrap_or_else(|e| e.into_inner());
        for t in tokens.values_mut() {
            t.expires_at = Utc::now() - Duration::seconds(1);
        }
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

#[cfg(test)]
mod tests;
