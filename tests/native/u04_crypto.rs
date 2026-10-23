//! U04 — the webhook AEAD (`src/security/crypto.rs`) and its at-rest record format.
//!
//! Production rows were encrypted by the retired Node runtime, and `WEBHOOK_ENC_KEY` is unset in
//! production, so both the ciphertext format and the plaintext fallback are load-bearing. A
//! Rust→Rust round trip proves nothing about the format, so the format is pinned by frozen
//! vectors captured from the retired Node runtime:
//!
//! * **Node → Rust** — records Node encrypted must decrypt to the exact original UTF-8 string.
//! * **Rust → Node** — with a pinned nonce, Rust must emit the exact record Node's `decrypt()`
//!   accepted.
//! * **Key parsing** — every key shape Node accepted, treated as unset, or rejected is classified
//!   the same way, with the same verbatim messages.

use std::sync::Arc;

use artifact_mcp::config::{ScriptedRandom, Secret};
use artifact_mcp::error::AppError;
use artifact_mcp::persistence::db;
use artifact_mcp::persistence::migrations::{
    self, EncryptedUrl, MigrationContext, WebhookUrlCipher, mask_webhook_url,
};
use artifact_mcp::security::crypto::{
    INVALID_KEY_MESSAGE, KEY_BYTES, MISSING_KEY_MESSAGE, NONCE_BYTES, PLAINTEXT_FALLBACK_WARNING,
    TAG_BYTES, WebhookCipher, WebhookUrlProtection, parse_encryption_key,
    warn_if_webhook_encryption_disabled,
};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;

use crate::u03_support::TempDataDir;

/// A canonical 32-byte key: standard Base64 of the bytes `0..32`.
fn parity_key() -> String {
    BASE64.encode(
        (0..KEY_BYTES)
            .map(|byte| u8::try_from(byte).expect("index fits u8"))
            .collect::<Vec<u8>>(),
    )
}

fn cipher() -> WebhookCipher {
    WebhookCipher::new(&Secret::new(parity_key())).expect("parity key is valid")
}

// ---------------------------------------------------------------------------
// Direction 1: Node encrypted, Rust decrypts
// ---------------------------------------------------------------------------

#[test]
fn rust_decrypts_records_written_by_the_retired_node_runtime() {
    // Frozen vector: captured from the retired Node runtime at commit 7211de0.
    // `(plaintext, ciphertext, nonce, tag)` under `parity_key()`.
    let records = [
        ("", "", "25iAWWtqI4oS7Eu4", "JTf9m17ORCSy9QDeuB0HOg=="),
        (
            "https://discord.com/api/webhooks/123456789012345678/aB3-_xYz0TokenValue",
            "Vm3f0ZJ6JWHo9edqhWQ9h0Ips0m2gqpvnio+i6WmXy3RomWeAj2V9cQoGXLqbf+/fe572y2655ioQsoHXVlqvJ6QxbA9y+Y=",
            "QmdiyRudhdZvCzn+",
            "BJqujEGAzAAEP/uYXAvMAw==",
        ),
        (
            "https://discordapp.com/api/webhooks/1/ünïcødé-🎉-トークン-Ω≈ç√",
            "T/Gc6vS71zBRrYu6zgKyR3WKLy8gpoqcZ9piRDEOGApVfyTd2gkVHF1s9T2eMooSM5YEm6TRk9mfZUFVY6Qn6XuugrI54HYbtjEMWaUt",
            "SCLTH1xpAhuMkUc/",
            "T8G+0JNzBt3N4cj+l3HPlg==",
        ),
    ];

    let cipher = cipher();
    for (expected, ciphertext, nonce, tag) in records {
        let encrypted = EncryptedUrl {
            ciphertext: ciphertext.to_owned(),
            nonce: nonce.to_owned(),
            tag: tag.to_owned(),
        };

        // The at-rest shape itself is part of the contract.
        assert_eq!(
            BASE64.decode(&encrypted.nonce).expect("nonce base64").len(),
            NONCE_BYTES,
            "node nonce is not {NONCE_BYTES} bytes"
        );
        assert_eq!(
            BASE64.decode(&encrypted.tag).expect("tag base64").len(),
            TAG_BYTES,
            "node tag is not {TAG_BYTES} bytes"
        );
        assert_eq!(
            BASE64
                .decode(&encrypted.ciphertext)
                .expect("ciphertext base64")
                .len(),
            expected.len(),
            "node ciphertext length must equal the plaintext byte length (no AAD, no padding)"
        );

        let decrypted = cipher
            .decrypt(&encrypted)
            .expect("rust must decrypt a node record");
        assert_eq!(decrypted, expected, "node → rust plaintext diverged");
    }
}

// ---------------------------------------------------------------------------
// Direction 2: Rust encrypts a record Node decrypted
// ---------------------------------------------------------------------------

#[test]
fn a_pinned_nonce_produces_the_record_the_retired_node_runtime_decrypted() {
    // Guards the format against an accidental change of AAD, tag length, or field encoding: with
    // a fixed key and a fixed nonce the three columns are fully determined.
    let cipher = WebhookCipher::with_random(
        &Secret::new(parity_key()),
        Arc::new(ScriptedRandom::new(vec![0xAB])),
    )
    .expect("valid key");
    let record = cipher.encrypt("artifact-mcp").expect("encrypt");

    // Frozen vector: captured from the retired Node runtime at commit 7211de0.
    // Node's `decrypt()` returned "artifact-mcp" for exactly this record.
    assert_eq!(
        record,
        EncryptedUrl {
            ciphertext: "rarAIMJ5co8PMiVb".to_owned(),
            nonce: "q6urq6urq6urq6ur".to_owned(),
            tag: "WFTM+SaEQLncUUlYmCp88w==".to_owned(),
        }
    );
    assert_eq!(record.nonce, BASE64.encode([0xAB_u8; NONCE_BYTES]));
    assert_eq!(cipher.decrypt(&record).expect("decrypt"), "artifact-mcp");
}

#[test]
fn a_converted_legacy_row_decrypts_to_the_original_url() {
    // A row produced by U03's real bootstrap conversion, driven by this unit's cipher.
    let cipher = cipher();
    let (url, record) = convert_one_legacy_row(&cipher);
    assert_eq!(cipher.decrypt(&record).expect("decrypt"), url);
    for tampered in tampered_variants(&record) {
        assert_eq!(cipher.decrypt(&tampered), Err(AppError::Internal));
    }
}

/// Runs U03's `encrypt_plaintext_webhook_urls` against a real database with the real U04 cipher,
/// returning the original URL and the row it produced.
fn convert_one_legacy_row(cipher: &WebhookCipher) -> (String, EncryptedUrl) {
    let dir = TempDataDir::new("u04-convert");
    let url = "https://discord.com/api/webhooks/123456789012345678/legacy-plaintext-token";

    let mut conn = db::open_bootstrap_connection(&db::database_path(dir.path()))
        .expect("bootstrap connection");
    migrations::apply(&mut conn, &MigrationContext::empty()).expect("apply migrations");
    conn.execute("INSERT INTO orgs (name) VALUES ('u04')", [])
        .expect("insert org");
    conn.execute(
        "INSERT INTO org_webhooks (id, org, url) VALUES ('u04-hook', 'u04', ?1)",
        [url],
    )
    .expect("insert legacy webhook");

    let converted = migrations::encrypt_plaintext_webhook_urls(&mut conn, cipher).expect("convert");
    assert_eq!(converted, 1);

    let (stored_url, record) = conn
        .query_row(
            "SELECT url, url_cipher, url_nonce, url_tag FROM org_webhooks WHERE id = 'u04-hook'",
            [],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    EncryptedUrl {
                        ciphertext: row.get(1)?,
                        nonce: row.get(2)?,
                        tag: row.get(3)?,
                    },
                ))
            },
        )
        .expect("read the converted row");

    assert_eq!(stored_url, mask_webhook_url(url));
    assert!(
        !stored_url.contains("legacy-plaintext-token"),
        "the masked column still contains the token"
    );
    (url.to_owned(), record)
}

fn tampered_variants(record: &EncryptedUrl) -> Vec<EncryptedUrl> {
    let flip = |value: &str| {
        let mut bytes = BASE64.decode(value).expect("decode field");
        let first = bytes.first_mut().expect("field is non-empty");
        *first ^= 0x01;
        BASE64.encode(&bytes)
    };
    vec![
        EncryptedUrl {
            ciphertext: flip(&record.ciphertext),
            ..record.clone()
        },
        EncryptedUrl {
            nonce: flip(&record.nonce),
            ..record.clone()
        },
        EncryptedUrl {
            tag: flip(&record.tag),
            ..record.clone()
        },
    ]
}

// ---------------------------------------------------------------------------
// Key parsing and error strings
// ---------------------------------------------------------------------------

#[test]
fn key_parsing_and_error_strings_match_the_retired_node_runtime() {
    #[derive(Debug, PartialEq, Eq)]
    enum Verdict {
        Accepted,
        Unset,
        Rejected,
    }

    let valid = parity_key();
    // Frozen vector: captured from the retired Node runtime at commit 7211de0.
    let cases: Vec<(String, Verdict)> = vec![
        (valid.clone(), Verdict::Accepted),
        (format!("  {valid}  "), Verdict::Accepted), // trimmed
        (format!("\u{85}{valid}\u{85}"), Verdict::Rejected), // JS preserves NEL
        (format!("\u{feff}{valid}\u{feff}"), Verdict::Accepted), // JS trims BOM
        (BASE64.encode([0_u8; KEY_BYTES]), Verdict::Accepted), // all-zero but canonical
        (String::new(), Verdict::Unset),             // unset → plaintext fallback
        ("   ".to_owned(), Verdict::Unset),          // blank → plaintext fallback
        (BASE64.encode([0_u8; 31]), Verdict::Rejected), // 31 bytes
        (BASE64.encode([0_u8; 33]), Verdict::Rejected), // 33 bytes
        (valid.replace('=', ""), Verdict::Rejected), // unpadded
        (format!("{valid}="), Verdict::Rejected),    // over-padded
        ("AAAA".to_owned(), Verdict::Rejected),      // canonical, wrong length
        ("not base64!!".to_owned(), Verdict::Rejected), // invalid characters
        (
            "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8".to_owned(), // 32 bytes, unpadded
            Verdict::Rejected,
        ),
    ];

    for (candidate, expected) in cases {
        let verdict = match parse_encryption_key(Some(&candidate)) {
            Ok(Some(_)) => Verdict::Accepted,
            Ok(None) => Verdict::Unset,
            Err(error) => {
                assert_eq!(error, AppError::Validation(INVALID_KEY_MESSAGE.to_owned()));
                Verdict::Rejected
            }
        };
        assert_eq!(verdict, expected, "{candidate:?}");
    }

    // Frozen vector: captured from the retired Node runtime at commit 7211de0.
    assert_eq!(
        INVALID_KEY_MESSAGE,
        "WEBHOOK_ENC_KEY must be a 32-byte base64 value."
    );
    assert_eq!(
        MISSING_KEY_MESSAGE,
        "WEBHOOK_ENC_KEY is required to decrypt webhook URLs."
    );
    assert_eq!(
        PLAINTEXT_FALLBACK_WARNING,
        "[artifact-mcp] WARNING: WEBHOOK_ENC_KEY is unset — Discord webhook URLs will be stored \
         in PLAINTEXT. Set a 32-byte base64 key to encrypt webhook credentials at rest."
    );
}

// ---------------------------------------------------------------------------
// Rust-only behaviour: fallback, tamper, and containment
// ---------------------------------------------------------------------------

#[test]
fn the_plaintext_fallback_stores_urls_verbatim_and_leaves_the_columns_null() {
    let dir = TempDataDir::new("u04-plaintext");
    let url = "https://discord.com/api/webhooks/123/plaintext-token";

    let mut conn = db::open_bootstrap_connection(&db::database_path(dir.path()))
        .expect("bootstrap connection");
    migrations::apply(&mut conn, &MigrationContext::empty()).expect("apply migrations");
    conn.execute("INSERT INTO orgs (name) VALUES ('u04')", [])
        .expect("insert org");

    // `WEBHOOK_ENC_KEY` unset is the live production configuration: the row is written verbatim.
    let protection = WebhookUrlProtection::from_env_value(None).expect("no key");
    assert!(!protection.enabled());
    assert!(protection.cipher().is_none());
    let stored = protection.protect(url).expect("protect");
    conn.execute(
        "INSERT INTO org_webhooks (id, org, url, url_cipher, url_nonce, url_tag)
         VALUES ('plain', 'u04', ?1, ?2, ?3, ?4)",
        rusqlite::params![
            stored.url,
            stored.encrypted.as_ref().map(|e| e.ciphertext.clone()),
            stored.encrypted.as_ref().map(|e| e.nonce.clone()),
            stored.encrypted.as_ref().map(|e| e.tag.clone()),
        ],
    )
    .expect("insert webhook");

    let (row_url, ciphertext): (String, Option<String>) = conn
        .query_row(
            "SELECT url, url_cipher FROM org_webhooks WHERE id = 'plain'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("read row");
    assert_eq!(row_url, url);
    assert_eq!(ciphertext, None);
    assert_eq!(protection.reveal(&stored).expect("reveal"), url);

    // A conversion pass with no cipher configured is exactly Node's `return 0`.
    struct NeverCalled;
    impl WebhookUrlCipher for NeverCalled {
        fn encrypt_url(&self, _plaintext: &str) -> Result<EncryptedUrl, AppError> {
            unreachable!("the plaintext fallback must never invoke a cipher")
        }
    }
    let cipher = cipher();
    assert_eq!(
        migrations::encrypt_plaintext_webhook_urls(&mut conn, &cipher).expect("convert"),
        1,
        "a configured cipher converts the legacy row"
    );
    assert_eq!(
        migrations::encrypt_plaintext_webhook_urls(&mut conn, &NeverCalled).expect("convert"),
        0,
        "an already converted row is never re-encrypted"
    );
}

#[test]
fn a_tampered_row_never_produces_plaintext() {
    let cipher = cipher();
    let url = "https://discord.com/api/webhooks/123/tamper-token";
    let record = cipher.encrypt(url).expect("encrypt");

    for tampered in tampered_variants(&record) {
        let error = cipher
            .decrypt(&tampered)
            .expect_err("a tampered record must never authenticate");
        assert_eq!(error, AppError::Internal);
        assert_eq!(error.to_string(), "internal error");
        assert!(!error.to_string().contains("tamper-token"));
    }
}

#[test]
fn no_key_or_url_reaches_debug_display_or_an_error_message() {
    let key = parity_key();
    let url = "https://discord.com/api/webhooks/123456789012345678/never-log-this-token";
    let protection = WebhookUrlProtection::from_env_value(Some(&key)).expect("key is valid");
    let stored = protection.protect(url).expect("protect");
    let secret = Secret::new(key.clone());

    let rendered = format!(
        "{protection:?} {:?} {secret:?} {secret} {stored:?} {} {}",
        protection.cipher().expect("cipher"),
        AppError::Internal,
        protection
            .reveal(&stored)
            .map(|_| "revealed")
            .unwrap_or("error"),
    );
    for forbidden in [url, key.as_str(), "never-log-this-token"] {
        assert!(
            !rendered.contains(forbidden),
            "rendered output leaked a secret: {rendered}"
        );
    }
    assert!(rendered.contains("<redacted>"));

    // The masked column keeps only the last four characters of the token.
    assert_eq!(stored.url, "https://discord.com/…oken");
}

#[test]
fn the_plaintext_warning_is_available_and_emitted_at_most_once() {
    let _ = warn_if_webhook_encryption_disabled(None);
    assert!(
        !warn_if_webhook_encryption_disabled(None),
        "the fallback warning must be emitted at most once per process"
    );
    assert!(PLAINTEXT_FALLBACK_WARNING.contains("PLAINTEXT"));
}
