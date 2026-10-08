//! ADR-0012 Web Push protocol pieces: VAPID (RFC 8292) ES256 JWTs and `aes128gcm` message
//! encryption (RFC 8291 over RFC 8188).
//!
//! This module is pure: no I/O and no clock. The sender in [`super::push_runtime`] supplies the
//! time, the randomness, and the HTTP transport.

use aes_gcm::{
    Aes128Gcm, Nonce,
    aead::{Aead, KeyInit},
};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use hkdf::Hkdf;
use p256::{
    PublicKey, SecretKey,
    ecdsa::{Signature, SigningKey, signature::Signer},
    elliptic_curve::sec1::ToEncodedPoint,
};
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::{config::RandomSource, error::AppError, persistence::push::base64url_bytes};

/// RFC 8188 record size advertised in the header. One record carries the whole message.
pub const RECORD_SIZE: u32 = 4096;
/// AES-GCM tag (16) plus the padding delimiter (1) share the single record with the plaintext.
pub const MAX_PLAINTEXT_BYTES: usize = RECORD_SIZE as usize - 17;
/// VAPID JWT lifetime.
pub const JWT_LIFETIME_SECONDS: i64 = 12 * 60 * 60;

/// A VAPID signing identity derived from `WEB_PUSH_VAPID_PRIVATE_KEY`.
#[derive(Clone)]
pub struct VapidSigner {
    key: SigningKey,
    public_key: String,
    subject: String,
}

impl std::fmt::Debug for VapidSigner {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("VapidSigner")
            .field("public_key", &self.public_key)
            .finish_non_exhaustive()
    }
}

#[derive(Serialize)]
struct JwtHeader {
    typ: &'static str,
    alg: &'static str,
}

#[derive(Serialize)]
struct JwtClaims<'a> {
    aud: &'a str,
    exp: i64,
    sub: &'a str,
}

impl VapidSigner {
    /// Build a signer from the canonical unpadded base64url private scalar.
    ///
    /// # Errors
    /// [`AppError::Validation`] when the key is not a valid P-256 scalar.
    pub fn new(private_key_b64url: &str, subject: &str) -> Result<Self, AppError> {
        let invalid = || AppError::Validation("WEB_PUSH_VAPID_PRIVATE_KEY is invalid".to_owned());
        let scalar = URL_SAFE_NO_PAD
            .decode(private_key_b64url)
            .map_err(|_| invalid())?;
        let secret = SecretKey::from_slice(&scalar).map_err(|_| invalid())?;
        let public_key = URL_SAFE_NO_PAD.encode(secret.public_key().to_encoded_point(false));
        Ok(Self {
            key: SigningKey::from(secret),
            public_key,
            subject: subject.to_owned(),
        })
    }

    /// Uncompressed public point, base64url without padding (the `k=` parameter).
    #[must_use]
    pub fn public_key(&self) -> &str {
        &self.public_key
    }

    /// ES256 JWT with `aud` = push-service origin, `exp` = `now + 12h`, `sub` = the subject.
    #[must_use]
    pub fn jwt(&self, audience: &str, now_seconds: i64) -> String {
        let header = serde_json::to_vec(&JwtHeader {
            typ: "JWT",
            alg: "ES256",
        })
        .unwrap_or_default();
        let claims = serde_json::to_vec(&JwtClaims {
            aud: audience,
            exp: now_seconds + JWT_LIFETIME_SECONDS,
            sub: &self.subject,
        })
        .unwrap_or_default();
        let signing_input = format!(
            "{}.{}",
            URL_SAFE_NO_PAD.encode(header),
            URL_SAFE_NO_PAD.encode(claims)
        );
        let signature: Signature = self.key.sign(signing_input.as_bytes());
        format!(
            "{signing_input}.{}",
            URL_SAFE_NO_PAD.encode(signature.to_bytes())
        )
    }

    /// `Authorization: vapid t=<JWT>, k=<public key>`.
    #[must_use]
    pub fn authorization(&self, jwt: &str) -> String {
        format!("vapid t={jwt}, k={}", self.public_key)
    }
}

/// `https://host` of an endpoint, the JWT audience.
#[must_use]
pub fn audience(endpoint: &url::Url) -> String {
    endpoint.origin().ascii_serialization()
}

/// `Topic` header: the first 32 base64url characters of SHA-256(tag).
#[must_use]
pub fn topic(tag: &str) -> String {
    let mut encoded = URL_SAFE_NO_PAD.encode(Sha256::digest(tag.as_bytes()));
    encoded.truncate(32);
    encoded
}

/// Why a message could not be encrypted for a subscription.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum EncryptError {
    #[error("subscription keys are invalid")]
    BadKeys,
    #[error("payload exceeds one record")]
    TooLarge,
    #[error("encryption failed")]
    Crypto,
}

/// RFC 8291 `aes128gcm` with an explicit application-server key and salt (deterministic; used by
/// the RFC 8291 section 5 test vector). Production uses [`encrypt_payload`].
///
/// # Errors
/// [`EncryptError`] for an invalid user-agent key, an oversized payload, or an AEAD failure.
pub fn encrypt_with(
    plaintext: &[u8],
    ua_public: &[u8],
    auth_secret: &[u8],
    as_secret: &SecretKey,
    salt: &[u8; 16],
) -> Result<Vec<u8>, EncryptError> {
    if plaintext.len() > MAX_PLAINTEXT_BYTES {
        return Err(EncryptError::TooLarge);
    }
    if ua_public.len() != 65 || auth_secret.len() != 16 {
        return Err(EncryptError::BadKeys);
    }
    let ua_key = PublicKey::from_sec1_bytes(ua_public).map_err(|_| EncryptError::BadKeys)?;
    let as_public = as_secret.public_key().to_encoded_point(false);
    let shared = p256::ecdh::diffie_hellman(as_secret.to_nonzero_scalar(), ua_key.as_affine());

    // PRK_key = HKDF-Extract(auth_secret, ecdh_secret); IKM = HKDF-Expand(PRK_key, key_info, 32)
    let mut key_info = Vec::with_capacity(14 + 65 + 65);
    key_info.extend_from_slice(b"WebPush: info\0");
    key_info.extend_from_slice(ua_public);
    key_info.extend_from_slice(as_public.as_bytes());
    let mut ikm = [0_u8; 32];
    Hkdf::<Sha256>::new(Some(auth_secret), shared.raw_secret_bytes())
        .expand(&key_info, &mut ikm)
        .map_err(|_| EncryptError::Crypto)?;

    // PRK = HKDF-Extract(salt, IKM); CEK and NONCE per RFC 8188.
    let prk = Hkdf::<Sha256>::new(Some(salt), &ikm);
    let mut cek = [0_u8; 16];
    let mut nonce = [0_u8; 12];
    prk.expand(b"Content-Encoding: aes128gcm\0", &mut cek)
        .map_err(|_| EncryptError::Crypto)?;
    prk.expand(b"Content-Encoding: nonce\0", &mut nonce)
        .map_err(|_| EncryptError::Crypto)?;

    let mut record = Vec::with_capacity(plaintext.len() + 1);
    record.extend_from_slice(plaintext);
    record.push(0x02); // last-record padding delimiter, no padding
    let cipher = Aes128Gcm::new_from_slice(&cek).map_err(|_| EncryptError::Crypto)?;
    let ciphertext = cipher
        .encrypt(Nonce::from_slice(&nonce), record.as_slice())
        .map_err(|_| EncryptError::Crypto)?;

    let mut body = Vec::with_capacity(16 + 4 + 1 + 65 + ciphertext.len());
    body.extend_from_slice(salt);
    body.extend_from_slice(&RECORD_SIZE.to_be_bytes());
    body.push(65);
    body.extend_from_slice(as_public.as_bytes());
    body.extend_from_slice(&ciphertext);
    Ok(body)
}

/// Encrypt one message with a fresh ephemeral P-256 key and a fresh 16-byte salt.
///
/// # Errors
/// [`EncryptError::BadKeys`] when the stored `p256dh`/`auth` do not decode; otherwise as
/// [`encrypt_with`]. An entropy failure maps to [`EncryptError::Crypto`].
pub fn encrypt_payload(
    plaintext: &[u8],
    p256dh: &str,
    auth: &str,
    random: &dyn RandomSource,
) -> Result<Vec<u8>, EncryptError> {
    let ua_public = base64url_bytes(p256dh).ok_or(EncryptError::BadKeys)?;
    let auth_secret = base64url_bytes(auth).ok_or(EncryptError::BadKeys)?;
    let mut salt = [0_u8; 16];
    random
        .fill_bytes(&mut salt)
        .map_err(|_| EncryptError::Crypto)?;
    let ephemeral = loop {
        let mut scalar = [0_u8; 32];
        random
            .fill_bytes(&mut scalar)
            .map_err(|_| EncryptError::Crypto)?;
        // Out-of-range scalars (probability ~2^-32) are simply redrawn.
        if let Ok(secret) = SecretKey::from_slice(&scalar) {
            break secret;
        }
    };
    encrypt_with(plaintext, &ua_public, &auth_secret, &ephemeral, &salt)
}

#[cfg(test)]
mod tests {
    use super::*;
    use p256::ecdsa::{VerifyingKey, signature::Verifier};

    fn b64(value: &str) -> Vec<u8> {
        URL_SAFE_NO_PAD.decode(value).expect("base64url")
    }

    /// RFC 8291 section 5 / appendix A.
    #[test]
    fn rfc8291_section5_vector() {
        let as_secret =
            SecretKey::from_slice(&b64("yfWPiYE-n46HLnH0KqZOF1fJJU3MYrct3AELtAQ-oRw")).unwrap();
        assert_eq!(
            URL_SAFE_NO_PAD.encode(as_secret.public_key().to_encoded_point(false)),
            "BP4z9KsN6nGRTbVYI_c7VJSPQTBtkgcy27mlmlMoZIIgDll6e3vCYLocInmYWAmS6TlzAC8wEqKK6PBru3jl7A8"
        );
        let ua_public = b64(
            "BCVxsr7N_eNgVRqvHtD0zTZsEc6-VV-JvLexhqUzORcxaOzi6-AYWXvTBHm4bjyPjs7Vd8pZGH6SRpkNtoIAiw4",
        );
        let auth = b64("BTBZMqHH6r4Tts7J_aSIgg");
        let salt: [u8; 16] = b64("DGv6ra1nlYgDCS1FRnbzlw").try_into().unwrap();
        let body = encrypt_with(
            b"When I grow up, I want to be a watermelon",
            &ua_public,
            &auth,
            &as_secret,
            &salt,
        )
        .expect("encrypt");
        assert_eq!(
            URL_SAFE_NO_PAD.encode(body),
            "DGv6ra1nlYgDCS1FRnbzlwAAEABBBP4z9KsN6nGRTbVYI_c7VJSPQTBtkgcy27mlmlMoZIIgDll6e3vCYLocInmYWAmS6TlzAC8wEqKK6PBru3jl7A_yl95bQpu6cVPTpK4Mqgkf1CXztLVBSt2Ks3oZwbuwXPXLWyouBWLVWGNWQexSgSxsj_Qulcy4a-fN"
        );
    }

    #[test]
    fn jwt_is_a_verifiable_es256_token() {
        let signer = VapidSigner::new(
            "yfWPiYE-n46HLnH0KqZOF1fJJU3MYrct3AELtAQ-oRw",
            "mailto:ops@example.test",
        )
        .unwrap();
        let jwt = signer.jwt("https://fcm.googleapis.com", 1_000);
        let parts: Vec<&str> = jwt.split('.').collect();
        assert_eq!(parts.len(), 3);
        assert_eq!(b64(parts[0]), br#"{"typ":"JWT","alg":"ES256"}"#);
        let claims: serde_json::Value = serde_json::from_slice(&b64(parts[1])).unwrap();
        assert_eq!(claims["aud"], "https://fcm.googleapis.com");
        assert_eq!(claims["exp"], 1_000 + JWT_LIFETIME_SECONDS);
        assert_eq!(claims["sub"], "mailto:ops@example.test");
        let point = b64(signer.public_key());
        let verifier = VerifyingKey::from_sec1_bytes(&point).unwrap();
        let signature = Signature::from_slice(&b64(parts[2])).unwrap();
        verifier
            .verify(format!("{}.{}", parts[0], parts[1]).as_bytes(), &signature)
            .expect("signature verifies");
        assert!(signer.authorization(&jwt).starts_with("vapid t=ey"));
    }

    #[test]
    fn topic_is_32_url_safe_characters() {
        let value = topic("abc:diaper");
        assert_eq!(value.len(), 32);
        assert!(
            value
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        );
    }

    #[test]
    fn oversized_payloads_are_refused() {
        let as_secret =
            SecretKey::from_slice(&b64("yfWPiYE-n46HLnH0KqZOF1fJJU3MYrct3AELtAQ-oRw")).unwrap();
        let ua_public = as_secret.public_key().to_encoded_point(false);
        assert_eq!(
            encrypt_with(
                &vec![b'x'; MAX_PLAINTEXT_BYTES + 1],
                ua_public.as_bytes(),
                &[0; 16],
                &as_secret,
                &[0; 16]
            ),
            Err(EncryptError::TooLarge)
        );
    }
}
