//! Ported from v2's passwordless auth suite; the passkey tests follow the
//! passkey PR. Sign-in is codes-only here, exercised the way a person does
//! it: mint, redeem, present the bearer token.

use super::*;

fn store_with_user() -> AuthStore {
    let store = AuthStore::in_memory();
    store.add_user("ada", "Ada Lovelace").expect("add");
    store
}

/// Sign ada in the way a person does: mint a code, redeem it.
fn sign_in(store: &AuthStore, id: &str) -> IssuedToken {
    let minted = store.mint_code(id).expect("mint");
    store.redeem_code(id, &minted.code).expect("redeem")
}

#[test]
fn a_redeemed_code_issues_a_usable_token() {
    let store = store_with_user();
    let issued = sign_in(&store, "ada");
    assert_eq!(issued.user.id, "ada");
    assert_eq!(issued.expires_in_seconds, ACCESS_TOKEN_TTL_HOURS * 3600);

    let who = store
        .authenticate_token(&issued.access_token)
        .expect("token resolves");
    assert_eq!(who.id, "ada");
    assert_eq!(who.name, "Ada Lovelace");
}

#[test]
fn an_unknown_or_tampered_token_resolves_to_nobody() {
    let store = store_with_user();
    let issued = sign_in(&store, "ada");

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

#[test]
fn disabling_and_removing_a_user_kills_their_sessions() {
    let store = store_with_user();

    let issued = sign_in(&store, "ada");
    store.set_disabled("ada", true).expect("disable");
    assert!(store.authenticate_token(&issued.access_token).is_none());
    assert_eq!(store.mint_code("ada").unwrap_err(), AuthError::Disabled);

    store.set_disabled("ada", false).expect("enable");
    let issued = sign_in(&store, "ada");
    assert!(store.authenticate_token(&issued.access_token).is_some());

    store.remove_user("ada").expect("remove");
    assert!(store.authenticate_token(&issued.access_token).is_none());
    assert!(store.get("ada").is_none());
}

/// "Forget a user" means all of it: an outstanding code minted before the
/// removal must not survive the name being re-added.
#[test]
fn removing_a_user_voids_their_outstanding_code() {
    let store = store_with_user();
    let minted = store.mint_code("ada").expect("mint");
    store.remove_user("ada").expect("remove");
    store.add_user("ada", "A Different Ada").expect("re-add");
    assert!(
        store.redeem_code("ada", &minted.code).is_err(),
        "the old code must not sign in the re-added user"
    );
}

#[test]
fn expired_tokens_are_refused_and_swept() {
    let store = store_with_user();
    let issued = sign_in(&store, "ada");

    store.expire_all_tokens_for_test();
    assert!(store.authenticate_token(&issued.access_token).is_none());
    // Refusing it also drops it, so the map cannot grow without bound.
    assert_eq!(store.session_counts(), vec![]);

    let issued = sign_in(&store, "ada");
    assert_eq!(store.sweep_expired(), 0, "a live token survives a sweep");
    assert!(store.authenticate_token(&issued.access_token).is_some());
}

#[test]
fn ids_are_case_insensitive_handles() {
    let store = AuthStore::in_memory();
    store.add_user("  Ada  ", "Ada Lovelace").expect("add");
    assert_eq!(store.get("ADA").unwrap().id, "ada");
    assert_eq!(
        store.add_user("ADA", "Someone Else").unwrap_err(),
        AuthError::UserExists("ada".into())
    );
    let minted = store.mint_code("Ada").expect("mint");
    assert!(store.redeem_code("ADA", &minted.code).is_ok());
}

/// Users survive a restart; sessions and codes deliberately do not.
#[test]
fn users_persist_across_reopen_but_tokens_and_codes_do_not() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("auth.json");

    let (issued, minted) = {
        let store = AuthStore::open(&path).expect("open");
        store.add_user("ada", "Ada Lovelace").expect("add");
        store.add_user("grace", "Grace Hopper").expect("add");
        store.set_disabled("grace", true).expect("disable");
        let issued = sign_in(&store, "ada");
        let minted = store.mint_code("ada").expect("mint");
        (issued, minted)
    };

    let reopened = AuthStore::open(&path).expect("reopen");
    assert_eq!(reopened.users().len(), 2);
    assert!(
        reopened.get("grace").unwrap().disabled,
        "the disabled flag survives"
    );
    assert!(
        reopened.authenticate_token(&issued.access_token).is_none(),
        "a restart is a logout"
    );
    assert!(
        reopened.redeem_code("ada", &minted.code).is_err(),
        "a restart voids outstanding codes"
    );

    // The snapshot must not contain a usable credential at all.
    let raw = std::fs::read_to_string(&path).expect("snapshot");
    assert!(
        !raw.contains(&issued.access_token),
        "the access token is not on disk"
    );
    assert!(!raw.contains(&minted.code), "the code is not on disk");
}

/// An `auth.json` written by the v2 password era (with `password_hash`
/// fields) still opens: the fields are ignored, and the next write drops
/// them — v2 stores migrate by being read.
#[test]
fn a_password_era_snapshot_still_opens() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("auth.json");
    std::fs::write(
        &path,
        r#"{"users": [{
            "id": "ada", "name": "Ada Lovelace",
            "password_hash": "pbkdf2_sha256$600000$AAAA$BBBB",
            "created_at": "2026-01-01T00:00:00Z",
            "password_set_at": "2026-01-01T00:00:00Z"
        }]}"#,
    )
    .unwrap();

    let store = AuthStore::open(&path).expect("open");
    assert_eq!(store.get("ada").unwrap().name, "Ada Lovelace");
    store.add_user("grace", "Grace Hopper").expect("add");
    let raw = std::fs::read_to_string(&path).expect("snapshot");
    assert!(
        !raw.contains("password_hash"),
        "the rewrite drops the dead field: {raw}"
    );
}

#[test]
fn a_missing_snapshot_starts_empty_rather_than_failing() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = AuthStore::open(dir.path().join("nope.json")).expect("open");
    assert!(store.is_empty());
}

#[test]
fn session_counts_report_live_tokens_per_user() {
    let store = store_with_user();
    store.add_user("grace", "Grace Hopper").expect("add");

    sign_in(&store, "ada");
    sign_in(&store, "ada");
    sign_in(&store, "grace");
    assert_eq!(
        store.session_counts(),
        vec![("ada".to_string(), 2), ("grace".to_string(), 1)]
    );

    assert_eq!(store.revoke_all_for("ada"), 2);
    assert_eq!(store.session_counts(), vec![("grace".to_string(), 1)]);
}

// ---------------------------------------------------------------------------
// One-time codes
// ---------------------------------------------------------------------------

#[test]
fn a_code_redeems_once_for_its_user_within_its_ttl() {
    let store = AuthStore::in_memory();
    store.add_user("ada", "Ada").unwrap();
    store.add_user("grace", "Grace").unwrap();

    let minted = store.mint_code("ada").expect("mint");
    assert_eq!(minted.user_id, "ada");
    assert_eq!(minted.code.len(), 11, "XXXXX-XXXXX");

    // Bound to its user; a guess burns nothing.
    assert!(store.redeem_code("grace", &minted.code).is_err());
    assert!(store.redeem_code("ada", "WRONG-WRONG").is_err());

    // Redeems exactly once, and the token it mints speaks as ada.
    let issued = store.redeem_code("ada", &minted.code).expect("redeem");
    assert_eq!(issued.user.id, "ada");
    assert!(store.authenticate_token(&issued.access_token).is_some());
    assert!(
        store.redeem_code("ada", &minted.code).is_err(),
        "single use"
    );
}

#[test]
fn minting_again_replaces_the_previous_code() {
    let store = AuthStore::in_memory();
    store.add_user("ada", "Ada").unwrap();
    let first = store.mint_code("ada").expect("mint");
    let second = store.mint_code("ada").expect("mint again");
    assert!(
        store.redeem_code("ada", &first.code).is_err(),
        "a mis-sent code dies the moment its replacement exists"
    );
    assert!(store.redeem_code("ada", &second.code).is_ok());
}

#[test]
fn disabled_and_unknown_users_cannot_be_minted_for() {
    let store = AuthStore::in_memory();
    store.add_user("ada", "Ada").unwrap();
    store.set_disabled("ada", true).unwrap();
    assert!(store.mint_code("ada").is_err());
    assert!(store.mint_code("nobody").is_err());
}
