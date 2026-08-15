//! Web Push delivery: RFC 8291 message encryption (aes128gcm) and
//! RFC 8292 VAPID, hand-rolled over the RustCrypto pieces already in the
//! dependency tree's neighborhood — the same house rule as the SSE
//! parser and the CDP client: the sliver of protocol we speak is smaller
//! than any client library's surface.
//!
//! The delivery doctrine: push is a best-effort *wake*, never the
//! record. The inbox is the truth a woken client re-reads; a push that
//! never arrives costs a wake, not an item. Hence: live additions push,
//! reconcile's catch-up does not (booting a notifier must not replay a
//! month of mentions onto a phone), and a `404`/`410` from the push
//! service prunes the endpoint — the browser said that subscription is
//! dead, and dead endpoints do not come back.

use std::path::Path;

use aes_gcm::aead::Aead as _;
use aes_gcm::{Aes128Gcm, KeyInit as _};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use p256::ecdsa::signature::Signer as _;
use p256::elliptic_curve::sec1::ToEncodedPoint as _;
use serde_json::{Value, json};

/// A fresh P-256 scalar from OS randomness. Bytes >= the group order are
/// rejected and redrawn (probability ~2^-32 per draw) — this sidesteps
/// the rand-core version seam between the workspace's rand and the
/// RustCrypto stack rather than pinning a second rand in the tree.
pub(crate) fn random_secret() -> p256::SecretKey {
    loop {
        let mut raw = [0u8; 32];
        rand::Rng::fill(&mut rand::rng(), &mut raw[..]);
        if let Ok(secret) = p256::SecretKey::from_slice(&raw) {
            return secret;
        }
    }
}

/// One VAPID identity plus the client that speaks to push services.
/// Load-or-generate: the keypair must survive restarts, because a
/// subscription is bound to the public key the browser saw at subscribe
/// time — a new key strands every registered endpoint.
pub struct Pusher {
    vapid: p256::ecdsa::SigningKey,
    contact: String,
    http: reqwest::Client,
}

impl Pusher {
    pub fn load_or_generate(path: &Path, contact: &str) -> Result<Self, String> {
        let secret = match std::fs::read_to_string(path) {
            Ok(text) => {
                let raw = B64
                    .decode(text.trim())
                    .map_err(|e| format!("{}: not base64url: {e}", path.display()))?;
                p256::SecretKey::from_slice(&raw)
                    .map_err(|e| format!("{}: not a P-256 scalar: {e}", path.display()))?
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let secret = random_secret();
                let encoded = B64.encode(secret.to_bytes());
                if let Some(dir) = path.parent() {
                    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
                }
                std::fs::write(path, &encoded).map_err(|e| e.to_string())?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt as _;
                    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
                }
                secret
            }
            Err(e) => return Err(format!("{}: {e}", path.display())),
        };
        Ok(Self::from_secret(secret, contact, reqwest::Client::new()))
    }

    fn from_secret(secret: p256::SecretKey, contact: &str, http: reqwest::Client) -> Self {
        Self {
            vapid: p256::ecdsa::SigningKey::from(secret),
            contact: contact.to_string(),
            http,
        }
    }

    /// The application server key a browser subscribes with: the raw
    /// uncompressed point, base64url.
    pub fn public_key_b64(&self) -> String {
        let point = self.vapid.verifying_key().to_encoded_point(false);
        B64.encode(point.as_bytes())
    }

    /// Encrypt `payload` for `subscription` and POST it. Answers the push
    /// service's status code; the caller decides what a 410 means.
    pub async fn send(&self, subscription: &Value, payload: &[u8]) -> Result<u16, String> {
        let endpoint = subscription["endpoint"]
            .as_str()
            .ok_or("a subscription has an endpoint")?;
        let ua_public = B64
            .decode(
                subscription["keys"]["p256dh"]
                    .as_str()
                    .ok_or("keys.p256dh")?,
            )
            .map_err(|e| format!("p256dh: {e}"))?;
        let auth = B64
            .decode(subscription["keys"]["auth"].as_str().ok_or("keys.auth")?)
            .map_err(|e| format!("auth: {e}"))?;

        let body = encrypt(&ua_public, &auth, payload, None, None)?;
        let jwt = self.vapid_jwt(endpoint)?;
        let response = self
            .http
            .post(endpoint)
            .header("content-encoding", "aes128gcm")
            .header("ttl", "86400")
            .header("urgency", "normal")
            .header(
                "authorization",
                format!("vapid t={jwt}, k={}", self.public_key_b64()),
            )
            .body(body)
            .send()
            .await
            .map_err(|e| format!("push to {endpoint}: {e}"))?;
        Ok(response.status().as_u16())
    }

    /// The VAPID token: an ES256 JWT whose audience is the push service's
    /// origin — the service checks it against the `k` key, which is the
    /// same key the browser bound the subscription to.
    fn vapid_jwt(&self, endpoint: &str) -> Result<String, String> {
        let aud: String = endpoint.split('/').take(3).collect::<Vec<_>>().join("/");
        let exp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|e| e.to_string())?
            .as_secs()
            + 12 * 60 * 60;
        let header = B64.encode(r#"{"typ":"JWT","alg":"ES256"}"#);
        let claims = B64.encode(json!({ "aud": aud, "exp": exp, "sub": self.contact }).to_string());
        let signing_input = format!("{header}.{claims}");
        // A JWS ES256 signature is raw r‖s (64 bytes), not DER.
        let signature: p256::ecdsa::Signature = self.vapid.sign(signing_input.as_bytes());
        Ok(format!(
            "{signing_input}.{}",
            B64.encode(signature.to_bytes())
        ))
    }

    #[cfg(test)]
    pub(crate) fn for_tests(http: reqwest::Client) -> Self {
        Self::from_secret(random_secret(), "mailto:test@myco.invalid", http)
    }

    #[cfg(test)]
    pub(crate) fn verifying_key(&self) -> &p256::ecdsa::VerifyingKey {
        self.vapid.verifying_key()
    }
}

/// RFC 8291: one record, aes128gcm content coding. `eph` and `salt` are
/// injectable so tests can fix the randomness; callers pass `None`.
pub fn encrypt(
    ua_public: &[u8],
    auth: &[u8],
    plaintext: &[u8],
    eph: Option<p256::SecretKey>,
    salt: Option<[u8; 16]>,
) -> Result<Vec<u8>, String> {
    let ua_key = p256::PublicKey::from_sec1_bytes(ua_public).map_err(|e| format!("ua key: {e}"))?;
    let as_secret = eph.unwrap_or_else(random_secret);
    let as_public = as_secret.public_key().to_encoded_point(false);
    let shared = p256::ecdh::diffie_hellman(as_secret.to_nonzero_scalar(), ua_key.as_affine());

    // IKM = HKDF(salt=auth, ecdh).expand("WebPush: info" || 0x00 || ua_pub || as_pub)
    let mut info = b"WebPush: info\x00".to_vec();
    info.extend_from_slice(ua_key.to_encoded_point(false).as_bytes());
    info.extend_from_slice(as_public.as_bytes());
    let mut ikm = [0u8; 32];
    hkdf::Hkdf::<sha2::Sha256>::new(Some(auth), shared.raw_secret_bytes())
        .expand(&info, &mut ikm)
        .map_err(|e| e.to_string())?;

    let salt = salt.unwrap_or_else(|| {
        let mut salt = [0u8; 16];
        rand::Rng::fill(&mut rand::rng(), &mut salt[..]);
        salt
    });
    let kdf = hkdf::Hkdf::<sha2::Sha256>::new(Some(&salt), &ikm);
    let mut cek = [0u8; 16];
    kdf.expand(b"Content-Encoding: aes128gcm\x00", &mut cek)
        .map_err(|e| e.to_string())?;
    let mut nonce = [0u8; 12];
    kdf.expand(b"Content-Encoding: nonce\x00", &mut nonce)
        .map_err(|e| e.to_string())?;

    // The coding header: salt ‖ rs(4, BE) ‖ idlen(1) ‖ keyid(as_pub, 65) —
    // then one record: AEAD(plaintext ‖ 0x02), the 0x02 marking the last
    // (only) record.
    let mut record = plaintext.to_vec();
    record.push(0x02);
    let sealed = Aes128Gcm::new((&cek).into())
        .encrypt((&nonce).into(), record.as_slice())
        .map_err(|e| format!("seal: {e}"))?;

    let mut body = Vec::with_capacity(16 + 4 + 1 + 65 + sealed.len());
    body.extend_from_slice(&salt);
    body.extend_from_slice(&4096u32.to_be_bytes());
    body.push(65);
    body.extend_from_slice(as_public.as_bytes());
    body.extend_from_slice(&sealed);
    Ok(body)
}

/// The receiver's half of RFC 8291, for tests only: written
/// independently from the spec's derivation section, so a roundtrip
/// checks the info strings and framing rather than mirroring the
/// encryptor. (The RFC's Appendix A vector would pin exact bytes; the
/// RFC is unreachable from this build environment, so the layout
/// assertions here pin what the vector otherwise would.)
#[cfg(test)]
pub(crate) fn decrypt(ua_secret: &p256::SecretKey, auth: &[u8], body: &[u8]) -> Vec<u8> {
    let salt = &body[..16];
    let rs = u32::from_be_bytes(body[16..20].try_into().unwrap());
    assert_eq!(rs, 4096, "one full-size record");
    let idlen = body[20] as usize;
    assert_eq!(idlen, 65, "the keyid is the sender's uncompressed point");
    let as_public = p256::PublicKey::from_sec1_bytes(&body[21..21 + idlen]).unwrap();
    let ciphertext = &body[21 + idlen..];

    let shared = p256::ecdh::diffie_hellman(ua_secret.to_nonzero_scalar(), as_public.as_affine());
    let mut info = b"WebPush: info\x00".to_vec();
    info.extend_from_slice(ua_secret.public_key().to_encoded_point(false).as_bytes());
    info.extend_from_slice(as_public.to_encoded_point(false).as_bytes());
    let mut ikm = [0u8; 32];
    hkdf::Hkdf::<sha2::Sha256>::new(Some(auth), shared.raw_secret_bytes())
        .expand(&info, &mut ikm)
        .unwrap();
    let kdf = hkdf::Hkdf::<sha2::Sha256>::new(Some(salt), &ikm);
    let mut cek = [0u8; 16];
    kdf.expand(b"Content-Encoding: aes128gcm\x00", &mut cek)
        .unwrap();
    let mut nonce = [0u8; 12];
    kdf.expand(b"Content-Encoding: nonce\x00", &mut nonce)
        .unwrap();

    let mut record = Aes128Gcm::new((&cek).into())
        .decrypt((&nonce).into(), ciphertext)
        .expect("the seal opens");
    assert_eq!(record.pop(), Some(0x02), "the last-record delimiter");
    record
}

#[cfg(test)]
mod tests {
    use super::*;
    use p256::ecdsa::signature::Verifier as _;

    #[test]
    fn the_seal_roundtrips_and_the_layout_is_the_rfc_layout() {
        let ua_secret = random_secret();
        let ua_public = ua_secret.public_key().to_encoded_point(false);
        let mut auth = [0u8; 16];
        rand::Rng::fill(&mut rand::rng(), &mut auth[..]);

        let body = encrypt(
            ua_public.as_bytes(),
            &auth,
            b"When I grow up, I want to be a watermelon",
            None,
            None,
        )
        .unwrap();

        // 86 bytes of header, then AEAD(plaintext + delimiter + 16B tag).
        assert_eq!(body.len(), 16 + 4 + 1 + 65 + 41 + 1 + 16);
        let opened = decrypt(&ua_secret, &auth, &body);
        assert_eq!(opened, b"When I grow up, I want to be a watermelon");
    }

    #[test]
    fn the_vapid_token_verifies_and_names_the_origin() {
        let pusher = Pusher::for_tests(reqwest::Client::new());
        let jwt = pusher
            .vapid_jwt("https://push.example.net:8443/wpush/v2/token-abc")
            .unwrap();
        let mut parts = jwt.split('.');
        let (header, claims, signature) = (
            parts.next().unwrap(),
            parts.next().unwrap(),
            parts.next().unwrap(),
        );

        let claims_json: Value = serde_json::from_slice(&B64.decode(claims).unwrap()).unwrap();
        assert_eq!(claims_json["aud"], "https://push.example.net:8443");
        assert_eq!(claims_json["sub"], "mailto:test@myco.invalid");
        assert!(claims_json["exp"].as_u64().unwrap() > 0);

        let signature =
            p256::ecdsa::Signature::from_slice(&B64.decode(signature).unwrap()).unwrap();
        pusher
            .verifying_key()
            .verify(format!("{header}.{claims}").as_bytes(), &signature)
            .expect("the token verifies against the advertised key");
    }
}
