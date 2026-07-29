//! Owned by U04 (sol) — webhook AES-GCM compatibility.
//!
//! The at-rest format is not a design choice: production rows already exist in it, written by
//! `lib/crypto.js`. This module is a byte-for-byte port of that reference.
//!
//! # At-rest format (authority: `lib/crypto.js`, `lib/webhooks.js:88-99`)
//!
//! | Property | Value | Node line |
//! |---|---|---|
//! | Algorithm | AES-256-GCM | `lib/crypto.js:5` |
//! | Key | 32 bytes, from **canonical padded** standard Base64 | `lib/crypto.js:9-18` |
//! | Nonce | 12 random bytes, fresh per encryption | `lib/crypto.js:6,33` |
//! | Associated data | **none** (no AAD is ever passed to `createCipheriv`) | `lib/crypto.js:34` |
//! | Plaintext | UTF-8 bytes of the URL | `lib/crypto.js:35` |
//! | Tag | 16 bytes, stored detached | `lib/crypto.js:40` |
//! | Encoding | ciphertext, nonce and tag are three separate standard Base64 strings | `lib/crypto.js:38-40` |
//! | Columns | `url_cipher`, `url_nonce`, `url_tag` | `lib/webhooks.js:94-96` |
//!
//! # Plaintext fallback
//!
//! `WEBHOOK_ENC_KEY` unset is a **live production configuration**, not a degraded mode: the URL is
//! stored verbatim in `url`, the three cipher columns stay `NULL`, and a one-time startup warning
//! is emitted. [`WebhookUrlProtection`] models both states so callers cannot forget the fallback.
//!
//! # Secret containment
//!
//! Neither the key nor a decrypted URL is ever rendered by `Debug`/`Display`, placed in an
//! [`AppError`] message, or logged. Authentication failure collapses to [`AppError::Internal`],
//! which the frozen error table renders as exactly `internal error` with no source detail.

use std::{
    fmt,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use aes_gcm::{
    Aes256Gcm, Nonce, Tag,
    aead::{AeadInPlace, KeyInit},
};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64_STANDARD};

use crate::{
    config::{OsRandom, RandomSource, Secret},
    error::AppError,
    persistence::migrations::{EncryptedUrl, WebhookUrlCipher, mask_webhook_url},
};

/// AES-256 key length in bytes. [lib/crypto.js:14]
pub const KEY_BYTES: usize = 32;
/// GCM nonce length in bytes — `const NONCE_BYTES = 12`. [lib/crypto.js:6]
pub const NONCE_BYTES: usize = 12;
/// GCM authentication tag length in bytes — Node's default `authTagLength`. [lib/crypto.js:40]
pub const TAG_BYTES: usize = 16;

/// Verbatim Node message for an unusable `WEBHOOK_ENC_KEY`. [lib/crypto.js:15,31]
pub const INVALID_KEY_MESSAGE: &str = "WEBHOOK_ENC_KEY must be a 32-byte base64 value.";

/// Verbatim Node message for decrypting with no key configured. [lib/crypto.js:46]
///
/// It never reaches an HTTP body: the frozen error table maps this condition to
/// [`AppError::Internal`], exactly as an uncaught throw becomes a 500 in the Node reference.
pub const MISSING_KEY_MESSAGE: &str = "WEBHOOK_ENC_KEY is required to decrypt webhook URLs.";

/// Verbatim Node startup warning for the plaintext fallback. [lib/crypto.js:23-26]
pub const PLAINTEXT_FALLBACK_WARNING: &str = "[artifact-mcp] WARNING: WEBHOOK_ENC_KEY is unset — Discord webhook URLs will be stored \
in PLAINTEXT. Set a 32-byte base64 key to encrypt webhook credentials at rest.";

/// `let warnedAboutPlaintextFallback = false` — the warning is emitted at most once per process.
/// [lib/crypto.js:7,21-22]
static WARNED_ABOUT_PLAINTEXT_FALLBACK: AtomicBool = AtomicBool::new(false);

// ---------------------------------------------------------------------------
// Key parsing
// ---------------------------------------------------------------------------

/// `String.prototype.trim`'s character set, which is **not** Rust's `char::is_whitespace`.
///
/// ECMA-262 trims WhiteSpace ∪ LineTerminator: TAB, VT, FF, SP, NBSP, ZWNBSP (`U+FEFF`), every
/// `Zs`, LF, CR, LS, PS. Rust's White_Space property adds `U+0085` (NEL) and omits `U+FEFF`, so
/// both differences are corrected here.
#[must_use]
const fn is_js_whitespace(value: char) -> bool {
    matches!(value, '\u{feff}') || (value.is_whitespace() && !matches!(value, '\u{85}'))
}

/// `String.prototype.trim` — [lib/crypto.js:10]
#[must_use]
fn js_trim(value: &str) -> &str {
    value.trim_matches(is_js_whitespace)
}

/// Port of `parseEncryptionKey(value)` — [lib/crypto.js:9-18].
///
/// Absent, or empty after trimming, yields `None` (the plaintext fallback). Otherwise the value
/// must decode to exactly 32 bytes **and** re-encode to itself: Node's `Buffer.from(v, "base64")`
/// is lenient, and the `key.toString("base64") !== encoded` guard is what makes the accepted set
/// exactly the canonical padded encodings. `base64`'s `STANDARD` engine rejects non-canonical
/// padding and non-zero trailing bits directly; the re-encode check is retained so the two
/// implementations agree by construction rather than by coincidence.
///
/// # Errors
/// Returns [`AppError::Validation`] carrying [`INVALID_KEY_MESSAGE`] verbatim.
pub fn parse_encryption_key(value: Option<&str>) -> Result<Option<[u8; KEY_BYTES]>, AppError> {
    let Some(raw) = value else {
        return Ok(None);
    };
    let encoded = js_trim(raw);
    if encoded.is_empty() {
        return Ok(None);
    }
    let decoded = BASE64_STANDARD.decode(encoded).map_err(|_| invalid_key())?;
    if decoded.len() != KEY_BYTES || BASE64_STANDARD.encode(&decoded) != encoded {
        return Err(invalid_key());
    }
    let key = <[u8; KEY_BYTES]>::try_from(decoded.as_slice()).map_err(|_| invalid_key())?;
    Ok(Some(key))
}

fn invalid_key() -> AppError {
    AppError::Validation(INVALID_KEY_MESSAGE.to_owned())
}

/// Port of `warnIfWebhookEncryptionDisabled(value)` — [lib/crypto.js:20-27].
///
/// Returns `true` when this call emitted the warning, so the once-only behaviour is observable
/// without capturing a `tracing` subscriber.
pub fn warn_if_webhook_encryption_disabled(value: Option<&str>) -> bool {
    if value.is_some_and(|raw| !raw.trim().is_empty()) {
        return false;
    }
    if WARNED_ABOUT_PLAINTEXT_FALLBACK.swap(true, Ordering::SeqCst) {
        return false;
    }
    tracing::warn!("{PLAINTEXT_FALLBACK_WARNING}");
    true
}

/// Best-effort scrub of transient key material.
///
/// This is defence in depth, not a guarantee: without `unsafe` (the crate forbids it) or a
/// zeroizing allocator the optimiser is free to elide the write, so `black_box` is used to make
/// elision unattractive.
fn scrub(bytes: &mut [u8]) {
    bytes.fill(0);
    std::hint::black_box(bytes);
}

// ---------------------------------------------------------------------------
// The cipher
// ---------------------------------------------------------------------------

/// AES-256-GCM webhook URL cipher, byte-compatible with `lib/crypto.js`.
///
/// Cloneable-by-`Arc` at the call site rather than by value: the key is not copied around.
pub struct WebhookCipher {
    cipher: Aes256Gcm,
    random: Arc<dyn RandomSource>,
}

impl WebhookCipher {
    /// Build a cipher from an already validated [`Secret`] (typically `AppConfig::webhook_enc_key`).
    ///
    /// The key is re-validated here rather than trusted: `crypto.rs` is the only module that may
    /// turn bytes into a cipher, so it owns the check even when the caller has already made it.
    ///
    /// # Errors
    /// Returns [`AppError::Validation`] carrying [`INVALID_KEY_MESSAGE`] when the secret is not a
    /// canonical 32-byte Base64 value.
    pub fn new(key: &Secret) -> Result<Self, AppError> {
        Self::with_random(key, Arc::new(OsRandom))
    }

    /// [`WebhookCipher::new`] with an injected nonce source, so tests can pin a nonce.
    ///
    /// # Errors
    /// As [`WebhookCipher::new`].
    pub fn with_random(key: &Secret, random: Arc<dyn RandomSource>) -> Result<Self, AppError> {
        let bytes = parse_encryption_key(Some(key.expose()))?.ok_or_else(invalid_key)?;
        Self::from_key_bytes(bytes, random)
    }

    /// Port of `parseEncryptionKey` composed with cipher construction: `None` means "no key
    /// configured", which is the plaintext fallback rather than an error. [lib/webhooks.js:88-89]
    ///
    /// # Errors
    /// Returns [`AppError::Validation`] carrying [`INVALID_KEY_MESSAGE`] when a value is present
    /// but unusable.
    pub fn from_env_value(value: Option<&str>) -> Result<Option<Self>, AppError> {
        match parse_encryption_key(value)? {
            None => Ok(None),
            Some(bytes) => Self::from_key_bytes(bytes, Arc::new(OsRandom)).map(Some),
        }
    }

    fn from_key_bytes(
        mut bytes: [u8; KEY_BYTES],
        random: Arc<dyn RandomSource>,
    ) -> Result<Self, AppError> {
        // `new_from_slice` is the non-panicking constructor; the length is already checked.
        let cipher = Aes256Gcm::new_from_slice(&bytes).map_err(|_| invalid_key())?;
        scrub(&mut bytes);
        Ok(Self { cipher, random })
    }

    /// Port of `encrypt(plaintext, key)` — [lib/crypto.js:29-42].
    ///
    /// A fresh 12-byte nonce is drawn per call, no associated data is bound, and the detached
    /// 16-byte tag is returned alongside the ciphertext, all Base64-encoded.
    ///
    /// # Errors
    /// Returns [`AppError::Internal`] when the entropy source fails or the AEAD refuses the input.
    /// The plaintext never appears in the error.
    pub fn encrypt(&self, plaintext: &str) -> Result<EncryptedUrl, AppError> {
        let mut nonce_bytes = [0_u8; NONCE_BYTES];
        self.random.fill_bytes(&mut nonce_bytes)?;
        let nonce = Nonce::from(nonce_bytes);

        let mut buffer = plaintext.as_bytes().to_vec();
        let tag = self
            .cipher
            .encrypt_in_place_detached(&nonce, &[], &mut buffer)
            .map_err(|_| aead_failure("webhook url encryption failed"))?;

        Ok(EncryptedUrl {
            ciphertext: BASE64_STANDARD.encode(&buffer),
            nonce: BASE64_STANDARD.encode(nonce_bytes),
            tag: BASE64_STANDARD.encode(tag),
        })
    }

    /// Port of `decrypt(record, key)` — [lib/crypto.js:44-54].
    ///
    /// Any modification of the ciphertext, nonce or tag fails authentication; no plaintext is ever
    /// produced from an unauthenticated record.
    ///
    /// # Errors
    /// Returns [`AppError::Internal`] for a malformed record or a failed authentication tag. The
    /// error deliberately does not distinguish the two, and carries no record content.
    pub fn decrypt(&self, encrypted: &EncryptedUrl) -> Result<String, AppError> {
        let nonce_bytes = decode_fixed::<NONCE_BYTES>(&encrypted.nonce)?;
        let tag_bytes = decode_fixed::<TAG_BYTES>(&encrypted.tag)?;
        let mut buffer = BASE64_STANDARD
            .decode(&encrypted.ciphertext)
            .map_err(|_| aead_failure("webhook url ciphertext is not valid base64"))?;

        self.cipher
            .decrypt_in_place_detached(
                &Nonce::from(nonce_bytes),
                &[],
                &mut buffer,
                &Tag::from(tag_bytes),
            )
            .map_err(|_| {
                scrub(&mut buffer);
                aead_failure("webhook url failed authentication")
            })?;

        String::from_utf8(buffer).map_err(|error| {
            let mut bytes = error.into_bytes();
            scrub(&mut bytes);
            aead_failure("decrypted webhook url is not valid utf-8")
        })
    }
}

impl WebhookUrlCipher for WebhookCipher {
    fn encrypt_url(&self, plaintext: &str) -> Result<EncryptedUrl, AppError> {
        self.encrypt(plaintext)
    }
}

/// Redacted by construction: neither the key nor the nonce stream is printable.
impl fmt::Debug for WebhookCipher {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("WebhookCipher(<redacted>)")
    }
}

/// Decode a Base64 field that must hold exactly `N` bytes.
fn decode_fixed<const N: usize>(encoded: &str) -> Result<[u8; N], AppError> {
    let decoded = BASE64_STANDARD
        .decode(encoded)
        .map_err(|_| aead_failure("webhook url record field is not valid base64"))?;
    <[u8; N]>::try_from(decoded.as_slice())
        .map_err(|_| aead_failure("webhook url record field has the wrong length"))
}

/// Every AEAD failure collapses to one opaque error. The `reason` is a fixed string with no
/// record, key, or URL content, so it is safe to log.
fn aead_failure(reason: &'static str) -> AppError {
    tracing::error!(reason, "webhook url cipher rejected a record");
    AppError::Internal
}

// ---------------------------------------------------------------------------
// Plaintext fallback
// ---------------------------------------------------------------------------

/// The three at-rest columns of `org_webhooks` as a single value. [lib/webhooks.js:90-96]
///
/// `url` is the *displayable* column: the masked form when encryption is on, and the real URL when
/// the plaintext fallback is active — exactly what `create()` writes in the Node reference.
#[derive(Clone, PartialEq, Eq)]
pub struct StoredWebhookUrl {
    /// The `url` column: masked when `encrypted` is `Some`, verbatim otherwise.
    pub url: String,
    /// The `url_cipher`/`url_nonce`/`url_tag` columns, or `None` when they are `NULL`.
    pub encrypted: Option<EncryptedUrl>,
}

impl std::fmt::Debug for StoredWebhookUrl {
    /// Hand-written to REDACT `url`.
    ///
    /// In the plaintext fallback — **the documented live production configuration**, since
    /// `WEBHOOK_ENC_KEY` is currently unset — `url` holds the real Discord webhook URL, which is a
    /// bearer credential. The derived impl meant `tracing::debug!(?stored)` would print it. Missed
    /// by the first redaction pass (which covered `CreatedPublisherKey` and `WebhookDelivery`) and
    /// caught by the codex review; the existing leakage test only exercised encrypted mode, where
    /// the value is already masked.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StoredWebhookUrl")
            .field("url", &"<redacted>")
            .field("encrypted", &self.encrypted.is_some())
            .finish()
    }
}

/// Whether webhook URLs are encrypted at rest, mirroring `parseEncryptionKey()` returning a key or
/// `undefined` in `lib/webhooks.js:88`.
/// The cipher is boxed because an expanded AES-256 key schedule is roughly a kilobyte, and the
/// plaintext variant — the current production configuration — must stay pointer-sized.
pub enum WebhookUrlProtection {
    /// A key is configured: URLs are encrypted and the `url` column holds the masked form.
    Encrypted(Box<WebhookCipher>),
    /// No key is configured: URLs are stored verbatim. This is a live production configuration.
    Plaintext,
}

impl WebhookUrlProtection {
    /// Resolve the mode from a raw `WEBHOOK_ENC_KEY` value.
    ///
    /// # Errors
    /// Returns [`AppError::Validation`] carrying [`INVALID_KEY_MESSAGE`] when a value is present
    /// but unusable. An absent or blank value selects [`WebhookUrlProtection::Plaintext`].
    pub fn from_env_value(value: Option<&str>) -> Result<Self, AppError> {
        Ok(WebhookCipher::from_env_value(value)?
            .map_or(Self::Plaintext, |cipher| Self::Encrypted(Box::new(cipher))))
    }

    /// Resolve the mode from the validated configuration field.
    ///
    /// # Errors
    /// As [`WebhookUrlProtection::from_env_value`]; unreachable in practice because `AppConfig`
    /// has already validated the key with the same rules.
    pub fn from_config_key(key: Option<&Secret>) -> Result<Self, AppError> {
        match key {
            None => Ok(Self::Plaintext),
            Some(secret) => {
                WebhookCipher::new(secret).map(|cipher| Self::Encrypted(Box::new(cipher)))
            }
        }
    }

    /// `true` when URLs are encrypted at rest.
    #[must_use]
    pub const fn enabled(&self) -> bool {
        matches!(self, Self::Encrypted(_))
    }

    /// The cipher, when one is configured — the value U03's conversion seam expects.
    #[must_use]
    pub fn cipher(&self) -> Option<&WebhookCipher> {
        match self {
            Self::Encrypted(cipher) => Some(cipher.as_ref()),
            Self::Plaintext => None,
        }
    }

    /// Port of the row construction in `create()` — [lib/webhooks.js:88-97].
    ///
    /// With a key: `url` becomes the masked display value and the three cipher columns are filled.
    /// Without one: `url` is the verbatim URL and the cipher columns stay `NULL`.
    ///
    /// # Errors
    /// Propagates [`WebhookCipher::encrypt`] failures.
    pub fn protect(&self, url: &str) -> Result<StoredWebhookUrl, AppError> {
        match self {
            Self::Encrypted(cipher) => Ok(StoredWebhookUrl {
                url: mask_webhook_url(url),
                encrypted: Some(cipher.encrypt(url)?),
            }),
            Self::Plaintext => Ok(StoredWebhookUrl {
                url: url.to_owned(),
                encrypted: None,
            }),
        }
    }

    /// Port of `deliveryRow(row)` — [lib/webhooks.js:117-120].
    ///
    /// A row with no ciphertext is returned as-is (the legacy plaintext path, which Node does not
    /// gate on the key); a row with ciphertext requires the key.
    ///
    /// # Errors
    /// Returns [`AppError::Internal`] when the row is encrypted but no key is configured — the
    /// [`MISSING_KEY_MESSAGE`] condition — or when authentication fails.
    pub fn reveal(&self, stored: &StoredWebhookUrl) -> Result<String, AppError> {
        match (&stored.encrypted, self) {
            (None, _) => Ok(stored.url.clone()),
            (Some(encrypted), Self::Encrypted(cipher)) => cipher.decrypt(encrypted),
            (Some(_), Self::Plaintext) => {
                tracing::error!("{MISSING_KEY_MESSAGE}");
                Err(AppError::Internal)
            }
        }
    }
}

/// Redacted by construction: the variant is visible, the key never is.
impl fmt::Debug for WebhookUrlProtection {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Encrypted(_) => "WebhookUrlProtection::Encrypted(<redacted>)",
            Self::Plaintext => "WebhookUrlProtection::Plaintext",
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ScriptedRandom;

    /// A canonical 32-byte key: Base64 of the bytes `0..32`.
    fn test_key() -> Secret {
        Secret::new(
            BASE64_STANDARD.encode(
                (0..KEY_BYTES)
                    .map(|byte| u8::try_from(byte).expect("index fits u8"))
                    .collect::<Vec<u8>>(),
            ),
        )
    }

    fn cipher() -> WebhookCipher {
        WebhookCipher::new(&test_key()).expect("valid key")
    }

    #[test]
    fn parses_only_canonical_thirty_two_byte_keys() {
        let key = test_key();
        assert!(
            parse_encryption_key(Some(key.expose()))
                .expect("valid key")
                .is_some()
        );
        // Absent and blank select the plaintext fallback, not an error.
        assert_eq!(parse_encryption_key(None).expect("absent"), None);
        assert_eq!(parse_encryption_key(Some("")).expect("empty"), None);
        assert_eq!(parse_encryption_key(Some("   ")).expect("blank"), None);

        let rejected = [
            BASE64_STANDARD.encode([0_u8; 31]),                  // too short
            BASE64_STANDARD.encode([0_u8; 33]),                  // too long
            BASE64_STANDARD.encode([0_u8; 32]).replace('=', ""), // unpadded
            "!!!!".to_owned(),                                   // invalid characters
            "AAAA".to_owned(),                                   // canonical but 3 bytes
            format!("{}\n{}", "AAAA", "AAAA"),                   // embedded newline
        ];
        for value in rejected {
            let error = parse_encryption_key(Some(&value)).expect_err("must reject");
            assert_eq!(error, AppError::Validation(INVALID_KEY_MESSAGE.to_owned()));
        }
    }

    #[test]
    fn agrees_with_the_config_key_parser() {
        use crate::config::{AppConfig, MapEnv};

        let accepted = test_key().expose().to_owned();
        let rejected = ["AAAA", "not base64", &BASE64_STANDARD.encode([7_u8; 16])];

        let parsed = AppConfig::from_source(&MapEnv::empty().with("WEBHOOK_ENC_KEY", &accepted))
            .expect("config accepts the key");
        assert_eq!(
            parsed.webhook_enc_key.as_ref().map(Secret::expose),
            Some(accepted.as_str())
        );
        assert!(
            parse_encryption_key(Some(&accepted))
                .expect("crypto accepts the key")
                .is_some()
        );

        for value in rejected {
            assert!(
                AppConfig::from_source(&MapEnv::empty().with("WEBHOOK_ENC_KEY", value)).is_err(),
                "config accepted {value:?}"
            );
            assert!(
                parse_encryption_key(Some(value)).is_err(),
                "crypto accepted {value:?}"
            );
        }
    }

    #[test]
    fn round_trips_every_shape_of_url() {
        let cipher = cipher();
        for plaintext in [
            "",
            "https://discord.com/api/webhooks/123456789012345678/tOkEn-value_1",
            "https://example.test/ünïcødé-🎉-トークン",
            &"https://example.test/".repeat(400),
        ] {
            let encrypted = cipher.encrypt(plaintext).expect("encrypt");
            assert_eq!(cipher.decrypt(&encrypted).expect("decrypt"), plaintext);
        }
    }

    #[test]
    fn emits_the_reference_field_lengths_and_a_fresh_nonce() {
        let cipher = cipher();
        let first = cipher
            .encrypt("https://discord.com/api/webhooks/1/a")
            .expect("one");
        let second = cipher
            .encrypt("https://discord.com/api/webhooks/1/a")
            .expect("two");

        assert_ne!(first.nonce, second.nonce, "nonce must never repeat");
        assert_ne!(first.ciphertext, second.ciphertext);
        assert_eq!(
            BASE64_STANDARD.decode(&first.nonce).expect("nonce").len(),
            NONCE_BYTES
        );
        assert_eq!(
            BASE64_STANDARD.decode(&first.tag).expect("tag").len(),
            TAG_BYTES
        );
        // No AAD and no padding: GCM ciphertext is exactly as long as the plaintext.
        assert_eq!(
            BASE64_STANDARD
                .decode(&first.ciphertext)
                .expect("ciphertext")
                .len(),
            "https://discord.com/api/webhooks/1/a".len()
        );
    }

    #[test]
    fn pinned_nonce_produces_a_pinned_record() {
        // Freezing one full record guards the format against an accidental change of AAD,
        // tag length, or encoding.
        let cipher =
            WebhookCipher::with_random(&test_key(), Arc::new(ScriptedRandom::new(vec![9])))
                .expect("valid key");
        let encrypted = cipher.encrypt("artifact-mcp").expect("encrypt");
        assert_eq!(encrypted.nonce, BASE64_STANDARD.encode([9_u8; NONCE_BYTES]));
        assert_eq!(cipher.decrypt(&encrypted).expect("decrypt"), "artifact-mcp");
    }

    #[test]
    fn tampering_with_any_field_fails_authentication() {
        let cipher = cipher();
        let plaintext = "https://discord.com/api/webhooks/123/secret-token";
        let encrypted = cipher.encrypt(plaintext).expect("encrypt");

        let flip = |value: &str| {
            let mut bytes = BASE64_STANDARD.decode(value).expect("decode");
            let first = bytes.first_mut().expect("non-empty field");
            *first ^= 0x01;
            BASE64_STANDARD.encode(&bytes)
        };

        let tampered = [
            EncryptedUrl {
                ciphertext: flip(&encrypted.ciphertext),
                ..encrypted.clone()
            },
            EncryptedUrl {
                nonce: flip(&encrypted.nonce),
                ..encrypted.clone()
            },
            EncryptedUrl {
                tag: flip(&encrypted.tag),
                ..encrypted.clone()
            },
            EncryptedUrl {
                ciphertext: String::new(),
                ..encrypted.clone()
            },
            EncryptedUrl {
                nonce: BASE64_STANDARD.encode([0_u8; NONCE_BYTES + 1]),
                ..encrypted.clone()
            },
            EncryptedUrl {
                tag: "not base64".to_owned(),
                ..encrypted.clone()
            },
        ];
        for record in tampered {
            let error = cipher.decrypt(&record).expect_err("must not authenticate");
            assert_eq!(error, AppError::Internal);
            assert_eq!(error.to_string(), "internal error");
        }

        // A different key never authenticates a foreign record either.
        let other = WebhookCipher::new(&Secret::new(BASE64_STANDARD.encode([42_u8; KEY_BYTES])))
            .expect("valid key");
        assert_eq!(
            other.decrypt(&encrypted).expect_err("foreign key"),
            AppError::Internal
        );
    }

    #[test]
    fn plaintext_fallback_stores_the_url_verbatim() {
        let protection = WebhookUrlProtection::from_env_value(None).expect("no key");
        assert!(!protection.enabled());
        assert!(protection.cipher().is_none());

        let url = "https://discord.com/api/webhooks/123/plaintext-token";
        let stored = protection.protect(url).expect("protect");
        assert_eq!(stored.url, url);
        assert_eq!(stored.encrypted, None);
        assert_eq!(protection.reveal(&stored).expect("reveal"), url);
    }

    #[test]
    fn encrypted_mode_masks_the_stored_url_consistently_with_the_migration() {
        let protection =
            WebhookUrlProtection::from_env_value(Some(test_key().expose())).expect("key");
        assert!(protection.enabled());

        let url = "https://discord.com/api/webhooks/123456789012345678/secret-token";
        let stored = protection.protect(url).expect("protect");
        assert_eq!(stored.url, mask_webhook_url(url));
        assert_eq!(stored.url, "https://discord.com/…oken");
        assert!(!stored.url.contains("secret-token"));
        assert_eq!(protection.reveal(&stored).expect("reveal"), url);
    }

    #[test]
    fn an_encrypted_row_without_a_key_never_yields_plaintext() {
        let encrypted_mode =
            WebhookUrlProtection::from_env_value(Some(test_key().expose())).expect("key");
        let stored = encrypted_mode
            .protect("https://discord.com/api/webhooks/1/token")
            .expect("protect");

        let plaintext_mode = WebhookUrlProtection::from_env_value(None).expect("no key");
        let error = plaintext_mode
            .reveal(&stored)
            .expect_err("must fail closed");
        assert_eq!(error, AppError::Internal);
        assert_eq!(error.to_string(), "internal error");
    }

    #[test]
    fn secrets_never_reach_debug_display_or_errors() {
        let key = test_key();
        let cipher = WebhookCipher::new(&key).expect("valid key");
        let url = "https://discord.com/api/webhooks/999/ultra-secret-token";
        let encrypted = cipher.encrypt(url).expect("encrypt");

        let protection = WebhookUrlProtection::Encrypted(Box::new(cipher));
        let stored = protection.protect(url).expect("protect");
        let rendered = format!(
            "{:?} {:?} {} {} {:?} {:?}",
            protection,
            protection.cipher().expect("cipher"),
            key,
            AppError::Internal,
            key,
            stored
        );
        for forbidden in [
            "ultra-secret-token",
            key.expose(),
            "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8=",
        ] {
            assert!(
                !rendered.contains(forbidden),
                "rendered output leaked {forbidden}: {rendered}"
            );
        }
        assert_eq!(
            format!("{protection:?}"),
            "WebhookUrlProtection::Encrypted(<redacted>)"
        );
        assert_eq!(
            format!("{:?}", protection.cipher().expect("cipher")),
            "WebhookCipher(<redacted>)"
        );

        // Every failure path returns the opaque internal error, never the record contents.
        let error = WebhookUrlProtection::Plaintext
            .reveal(&StoredWebhookUrl {
                url: stored.url.clone(),
                encrypted: Some(encrypted),
            })
            .expect_err("no key");
        assert_eq!(error.to_string(), "internal error");
    }

    #[test]
    fn the_plaintext_warning_is_emitted_at_most_once() {
        // Order-independent: whatever happened earlier in the process, a second call is silent.
        let _ = warn_if_webhook_encryption_disabled(None);
        assert!(!warn_if_webhook_encryption_disabled(None));
        assert!(!warn_if_webhook_encryption_disabled(Some("   ")));
        // A configured key never warns.
        assert!(!warn_if_webhook_encryption_disabled(Some(
            test_key().expose()
        )));
    }
}
