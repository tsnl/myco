use super::*;

const PASSWORD: &str = "correct horse battery staple";

/// Fixtures use a token work factor: the KDF itself is exercised by
/// `a_hash_round_trips_and_is_salted`, and every other test is about the
/// store's logic, not about how long a hash takes.
const FAST: u32 = 1;

fn store_with_user() -> AuthStore {
    let store = AuthStore::in_memory().with_work_factor(FAST);
    store.add_user("ada", "Ada Lovelace").expect("add");
    store.set_password("ada", PASSWORD).expect("set password");
    store
}

#[test]
fn a_hash_round_trips_and_is_salted() {
    let a = hash_password(PASSWORD);
    let b = hash_password(PASSWORD);
    assert!(verify_password(PASSWORD, &a));
    assert!(verify_password(PASSWORD, &b));
    assert!(!verify_password("wrong", &a));
    // Same password, different salts: identical hashes would let one crack
    // reveal every account that shares a password.
    assert_ne!(a, b, "each hash must carry its own salt");
    assert!(
        a.starts_with(&format!("pbkdf2_sha256${PBKDF2_ITERATIONS}$")),
        "{a}"
    );
}

/// The work factor is written into the hash and read back at verification.
/// If those two ever disagree, every credential in the store silently stops
/// verifying — so pin both directions.
#[test]
fn a_hash_records_the_work_factor_it_was_computed_with() {
    for iterations in [1u32, 17, 5_000] {
        let hash = hash_password_with(PASSWORD, iterations);
        assert!(
            hash.starts_with(&format!("pbkdf2_sha256${iterations}$")),
            "{hash}"
        );
        assert!(verify_password(PASSWORD, &hash), "{iterations}");
        assert!(!verify_password("wrong", &hash), "{iterations}");
    }
    // A hash written at one factor still verifies after the default changes,
    // which is what makes raising PBKDF2_ITERATIONS a safe edit.
    let old = hash_password_with(PASSWORD, 3);
    assert!(verify_password(PASSWORD, &old));
}

/// A corrupt or unknown-format record must fail closed. The dangerous bug
/// here is a parse error that ends up treated as a match.
#[test]
fn a_malformed_hash_never_verifies() {
    for bad in [
        "",
        "$",
        "not-a-hash",
        "pbkdf2_sha256$notanumber$AAAA$AAAA",
        "pbkdf2_sha256$0$AAAA$AAAA",
        "md5$1$AAAA$AAAA",
        "pbkdf2_sha256$600000$!!!!$????",
        // Right shape, wrong digest length.
        "pbkdf2_sha256$600000$AAAAAAAAAAAAAAAAAAAAAA$AAAA",
    ] {
        assert!(!verify_password(PASSWORD, bad), "{bad:?} must not verify");
        assert!(!verify_password("", bad), "{bad:?} must not verify");
    }
}

#[test]
fn the_password_grant_issues_a_usable_token() {
    let store = store_with_user();
    let issued = store.login("ada", PASSWORD).expect("login");
    assert_eq!(issued.user.id, "ada");
    assert_eq!(issued.expires_in_seconds, ACCESS_TOKEN_TTL_HOURS * 3600);

    let who = store
        .authenticate_token(&issued.access_token)
        .expect("token resolves");
    assert_eq!(who.id, "ada");
    assert_eq!(
        who.author(),
        Author::User {
            id: "ada".into(),
            name: "Ada Lovelace".into()
        }
    );
}

#[test]
fn bad_credentials_are_indistinguishable() {
    let store = store_with_user();
    store.add_user("grace", "Grace Hopper").expect("add");

    // Wrong password, unknown user, and a user with no password set all
    // report the same thing: the response must not enumerate accounts.
    assert_eq!(
        store.login("ada", "wrong").unwrap_err(),
        AuthError::InvalidCredentials
    );
    assert_eq!(
        store.login("nobody", PASSWORD).unwrap_err(),
        AuthError::InvalidCredentials
    );
    assert_eq!(
        store.login("grace", PASSWORD).unwrap_err(),
        AuthError::InvalidCredentials
    );
}

#[test]
fn an_unknown_or_tampered_token_resolves_to_nobody() {
    let store = store_with_user();
    let issued = store.login("ada", PASSWORD).expect("login");

    assert!(store.authenticate_token("").is_none());
    assert!(store.authenticate_token("nope").is_none());
    // A prefix of a live token is not a token.
    assert!(
        store
            .authenticate_token(&issued.access_token[..16])
            .is_none()
    );
    // Neither is one with a character changed.
    let mut tampered = issued.access_token.clone();
    tampered.pop();
    tampered.push(if issued.access_token.ends_with('A') {
        'B'
    } else {
        'A'
    });
    assert!(store.authenticate_token(&tampered).is_none());
}

/// Changing a password must end the sessions it protected — otherwise the
/// fix for a compromised account leaves the attacker logged in.
#[test]
fn changing_a_password_revokes_existing_tokens() {
    let store = store_with_user();
    let old = store.login("ada", PASSWORD).expect("login");
    assert!(store.authenticate_token(&old.access_token).is_some());

    store
        .set_password("ada", "a different password")
        .expect("chpass");
    assert!(
        store.authenticate_token(&old.access_token).is_none(),
        "the old session must be gone"
    );
    assert_eq!(
        store.login("ada", PASSWORD).unwrap_err(),
        AuthError::InvalidCredentials
    );
    let fresh = store.login("ada", "a different password").expect("login");
    assert!(store.authenticate_token(&fresh.access_token).is_some());
}

#[test]
fn disabling_and_removing_a_user_kills_their_sessions() {
    let store = store_with_user();

    let issued = store.login("ada", PASSWORD).expect("login");
    store.set_disabled("ada", true).expect("disable");
    assert!(store.authenticate_token(&issued.access_token).is_none());
    assert_eq!(
        store.login("ada", PASSWORD).unwrap_err(),
        AuthError::Disabled
    );

    store.set_disabled("ada", false).expect("enable");
    let issued = store.login("ada", PASSWORD).expect("login again");
    assert!(store.authenticate_token(&issued.access_token).is_some());

    store.remove_user("ada").expect("remove");
    assert!(store.authenticate_token(&issued.access_token).is_none());
    assert!(store.get("ada").is_none());
}

#[test]
fn expired_tokens_are_refused_and_swept() {
    let store = store_with_user();
    let issued = store.login("ada", PASSWORD).expect("login");

    // Reach in and age the token past its expiry.
    {
        let mut tokens = store.tokens.write().unwrap();
        for t in tokens.values_mut() {
            t.expires_at = Utc::now() - Duration::seconds(1);
        }
    }
    assert!(store.authenticate_token(&issued.access_token).is_none());
    // Refusing it also drops it, so the map cannot grow without bound.
    assert!(store.tokens.read().unwrap().is_empty());

    let issued = store.login("ada", PASSWORD).expect("login");
    assert_eq!(store.sweep_expired(), 0, "a live token survives a sweep");
    assert!(store.authenticate_token(&issued.access_token).is_some());
}

#[test]
fn ids_are_case_insensitive_handles() {
    let store = AuthStore::in_memory().with_work_factor(FAST);
    store.add_user("  Ada  ", "Ada Lovelace").expect("add");
    assert_eq!(store.get("ADA").unwrap().id, "ada");
    assert_eq!(
        store.add_user("ADA", "Someone Else").unwrap_err(),
        AuthError::UserExists("ada".into())
    );
    store.set_password("Ada", PASSWORD).expect("chpass");
    assert!(store.login("ADA", PASSWORD).is_ok());
}

#[test]
fn short_passwords_are_refused() {
    let store = AuthStore::in_memory().with_work_factor(FAST);
    store.add_user("ada", "Ada").expect("add");
    assert!(matches!(
        store.set_password("ada", "short"),
        Err(AuthError::WeakPassword(_))
    ));
    assert!(!store.get("ada").unwrap().can_authenticate());
}

/// Users survive a restart; sessions deliberately do not.
#[test]
fn users_persist_across_reopen_but_tokens_do_not() {
    let dir = crate::test_support::temp_dir("auth-persist");
    let path = dir.path().join("auth.json");

    let issued = {
        let store = AuthStore::open(&path).expect("open").with_work_factor(FAST);
        store.add_user("ada", "Ada Lovelace").expect("add");
        store.set_password("ada", PASSWORD).expect("chpass");
        store.login("ada", PASSWORD).expect("login")
    };

    let reopened = AuthStore::open(&path)
        .expect("reopen")
        .with_work_factor(FAST);
    assert_eq!(reopened.users().len(), 1);
    assert!(
        reopened.login("ada", PASSWORD).is_ok(),
        "credentials survive"
    );
    assert!(
        reopened.authenticate_token(&issued.access_token).is_none(),
        "a restart is a logout"
    );

    // The snapshot must not contain a usable credential in the clear.
    let raw = std::fs::read_to_string(&path).expect("snapshot");
    assert!(!raw.contains(PASSWORD), "the password is not on disk");
    assert!(
        !raw.contains(&issued.access_token),
        "the access token is not on disk"
    );
    assert!(raw.contains("pbkdf2_sha256$"), "only the hash is: {raw}");
}

#[test]
fn a_missing_snapshot_starts_empty_rather_than_failing() {
    let dir = crate::test_support::temp_dir("auth-missing");
    let store = AuthStore::open(dir.path().join("nope.json")).expect("open");
    assert!(store.is_empty());
}

#[test]
fn session_counts_report_live_tokens_per_user() {
    let store = store_with_user();
    store.add_user("grace", "Grace Hopper").expect("add");
    store.set_password("grace", PASSWORD).expect("chpass");

    store.login("ada", PASSWORD).expect("login");
    store.login("ada", PASSWORD).expect("login");
    store.login("grace", PASSWORD).expect("login");
    assert_eq!(
        store.session_counts(),
        vec![("ada".to_string(), 2), ("grace".to_string(), 1)]
    );

    assert_eq!(store.revoke_all_for("ada"), 2);
    assert_eq!(store.session_counts(), vec![("grace".to_string(), 1)]);
}
