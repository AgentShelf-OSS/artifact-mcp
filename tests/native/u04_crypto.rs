//! U04 cross-runtime proof: the webhook AEAD must be byte-compatible with `lib/crypto.js`.
//!
//! Production rows are already encrypted by the Node reference and `WEBHOOK_ENC_KEY` is currently
//! unset in production, so both the ciphertext format and the plaintext fallback are load-bearing.
//! A Rust→Rust round trip proves nothing about either, so every parity assertion here drives the
//! real `lib/crypto.js` through `node -e`, in **both** directions:
//!
//! * **Node → Rust** — Node encrypts, Rust must recover the exact original UTF-8 string.
//! * **Rust → Node** — Rust encrypts (including a record produced by U03's real database
//!   conversion path), Node's `decrypt()` must recover the exact original.
//!
//! # Skip visibility
//!
//! When `node` or `lib/crypto.js` is unavailable these tests **skip** so `cargo test` still works
//! in a Rust-only environment. That is the hazard recorded in the U01 contract: a green run does
//! not by itself prove the parity check executed. Set `REQUIRE_NODE_REFERENCE=1` to convert every
//! skip into a hard failure, which is how CI must run this suite:
//!
//! ```text
//! REQUIRE_NODE_REFERENCE=1 cargo test
//! ```

use std::path::{Path, PathBuf};
use std::process::Command;
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
use serde_json::{Value, json};

use crate::u03_support::TempDataDir;

/// Setting this to `1` turns "Node is unavailable" from a skip into a failure.
const REQUIRE_NODE_REFERENCE: &str = "REQUIRE_NODE_REFERENCE";

/// The URL shapes the parity proof must cover: ASCII, non-ASCII/emoji, empty, long, and a
/// realistic Discord webhook URL.
fn samples() -> Vec<String> {
    vec![
        String::new(),
        "https://discord.com/api/webhooks/123456789012345678/aB3-_xYz0TokenValue".to_owned(),
        "https://discordapp.com/api/webhooks/1/ünïcødé-🎉-トークン-Ω≈ç√".to_owned(),
        "plain-ascii-no-scheme".to_owned(),
        format!(
            "https://discord.com/api/webhooks/1/{}",
            "long".repeat(512) // 2 KiB of token
        ),
    ]
}

/// A canonical 32-byte key: standard Base64 of the bytes `0..32`.
fn parity_key() -> String {
    BASE64.encode(
        (0..KEY_BYTES)
            .map(|byte| u8::try_from(byte).expect("index fits u8"))
            .collect::<Vec<u8>>(),
    )
}

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn require_node_reference() -> bool {
    std::env::var(REQUIRE_NODE_REFERENCE).is_ok_and(|value| value == "1")
}

/// Node reference availability.
///
/// Returns `false` (skip) only when `REQUIRE_NODE_REFERENCE=1` is not set; otherwise it fails the
/// test, so a CI job cannot silently green-pass without ever running the byte-parity proof.
fn node_reference_available(root: &Path) -> bool {
    let unavailable = if root.join("lib/crypto.js").is_file() {
        match Command::new("node").arg("--version").output() {
            Ok(output) if output.status.success() => None,
            _ => Some("node is not on PATH"),
        }
    } else {
        Some("lib/crypto.js is missing")
    };

    match unavailable {
        None => true,
        Some(reason) => {
            assert!(
                !require_node_reference(),
                "{REQUIRE_NODE_REFERENCE}=1 but the Node reference is unavailable ({reason}); \
                 the Node/Rust byte-parity proof did not run"
            );
            eprintln!("skipping U04 Node byte-parity proof: {reason}");
            eprintln!("set {REQUIRE_NODE_REFERENCE}=1 to make this a failure instead");
            false
        }
    }
}

/// One `node -e` invocation that exercises every `lib/crypto.js` entry point this unit ports.
///
/// `process.argv[1]` is the module URL and `process.argv[2]` the JSON request, matching the way
/// `u03_cross_runtime.rs` drives the reference.
const NODE_DRIVER: &str = r#"
import(process.argv[1]).then((crypto) => {
  const input = JSON.parse(process.argv[2]);
  const decryptAll = (records) => records.map((record) => {
    try { return { ok: true, value: crypto.decrypt(record, input.key) }; }
    catch (error) { return { ok: false, error: String(error && error.message) }; }
  });
  const nodeRecords = input.samples.map((sample) => crypto.encrypt(sample, input.key));
  const out = {
    nodeRecords,
    nodeSelfCheck: nodeRecords.map((record) => crypto.decrypt(record, input.key)),
    nodeDecrypts: decryptAll(input.rustRecords),
    nodeDecryptsTampered: decryptAll(input.tamperedRecords),
    keyResults: input.keyCandidates.map((candidate) => {
      try { return { accepted: Boolean(crypto.parseEncryptionKey(candidate)), error: null }; }
      catch (error) { return { accepted: false, error: String(error && error.message) }; }
    }),
    missingKeyError: null,
    invalidKeyError: null,
    warning: null
  };
  try { crypto.decrypt({}, ""); }
  catch (error) { out.missingKeyError = String(error && error.message); }
  try { crypto.encrypt("x", "not-a-key"); }
  catch (error) { out.invalidKeyError = String(error && error.message); }
  const warn = console.warn;
  console.warn = (message) => { out.warning = String(message); };
  crypto.warnIfWebhookEncryptionDisabled("");
  console.warn = warn;
  process.stdout.write(JSON.stringify(out));
}).catch((error) => { console.error(error); process.exit(1); });
"#;

fn run_node(root: &Path, request: &Value) -> Value {
    let module = format!("file://{}", root.join("lib/crypto.js").display());
    let output = Command::new("node")
        .current_dir(root)
        .arg("-e")
        .arg(NODE_DRIVER)
        .arg(&module)
        .arg(request.to_string())
        .env_remove("WEBHOOK_ENC_KEY")
        .output()
        .expect("run the node crypto reference");
    assert!(
        output.status.success(),
        "node reference failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("node reference emitted JSON")
}

fn request(
    key: &str,
    samples: &[String],
    rust: &[EncryptedUrl],
    tampered: &[EncryptedUrl],
) -> Value {
    json!({
        "key": key,
        "samples": samples,
        "rustRecords": rust.iter().map(record_json).collect::<Vec<Value>>(),
        "tamperedRecords": tampered.iter().map(record_json).collect::<Vec<Value>>(),
        "keyCandidates": Vec::<String>::new(),
    })
}

fn record_json(record: &EncryptedUrl) -> Value {
    json!({
        "ciphertext": record.ciphertext,
        "nonce": record.nonce,
        "tag": record.tag,
    })
}

fn record_from_json(value: &Value) -> EncryptedUrl {
    let field = |name: &str| {
        value
            .get(name)
            .and_then(Value::as_str)
            .unwrap_or_else(|| panic!("node record is missing {name}: {value}"))
            .to_owned()
    };
    EncryptedUrl {
        ciphertext: field("ciphertext"),
        nonce: field("nonce"),
        tag: field("tag"),
    }
}

fn array<'a>(response: &'a Value, name: &str) -> &'a Vec<Value> {
    response
        .get(name)
        .and_then(Value::as_array)
        .unwrap_or_else(|| panic!("node response is missing the {name} array"))
}

fn cipher() -> WebhookCipher {
    WebhookCipher::new(&Secret::new(parity_key())).expect("parity key is valid")
}

// ---------------------------------------------------------------------------
// Direction 1: Node encrypts, Rust decrypts
// ---------------------------------------------------------------------------

#[test]
fn rust_decrypts_records_written_by_the_node_reference() {
    let root = repo_root();
    if !node_reference_available(&root) {
        return;
    }

    let samples = samples();
    let key = parity_key();
    let response = run_node(&root, &request(&key, &samples, &[], &[]));

    // The reference must agree with itself first; otherwise the fixture, not the port, is wrong.
    let self_check: Vec<String> = array(&response, "nodeSelfCheck")
        .iter()
        .map(|value| {
            value
                .as_str()
                .expect("node self check is a string")
                .to_owned()
        })
        .collect();
    assert_eq!(self_check, samples, "the node reference is inconsistent");

    let records = array(&response, "nodeRecords");
    assert_eq!(records.len(), samples.len());

    let cipher = cipher();
    for (record, expected) in records.iter().zip(samples.iter()) {
        let encrypted = record_from_json(record);

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
        assert_eq!(&decrypted, expected, "node → rust plaintext diverged");
    }
}

// ---------------------------------------------------------------------------
// Direction 2: Rust encrypts, Node decrypts
// ---------------------------------------------------------------------------

#[test]
fn the_node_reference_decrypts_records_written_by_rust() {
    let root = repo_root();
    if !node_reference_available(&root) {
        return;
    }

    let key = parity_key();
    let cipher = cipher();
    let mut samples = samples();
    let mut records: Vec<EncryptedUrl> = samples
        .iter()
        .map(|sample| cipher.encrypt(sample).expect("rust encrypt"))
        .collect();

    // The strongest case: a row produced by U03's real bootstrap conversion, driven by this
    // unit's cipher, must be readable by the Node reference.
    let (converted_url, converted_record) = convert_one_legacy_row(&cipher);
    samples.push(converted_url);
    records.push(converted_record);

    // Tampering with any of the three columns must fail authentication in Node too.
    let tampered = tampered_variants(&records[1]);

    let response = run_node(&root, &request(&key, &[], &records, &tampered));

    let decrypts = array(&response, "nodeDecrypts");
    assert_eq!(decrypts.len(), samples.len());
    for (result, expected) in decrypts.iter().zip(samples.iter()) {
        assert_eq!(
            result.get("ok").and_then(Value::as_bool),
            Some(true),
            "node failed to decrypt a rust record: {result}"
        );
        assert_eq!(
            result.get("value").and_then(Value::as_str),
            Some(expected.as_str()),
            "rust → node plaintext diverged"
        );
    }

    let rejected = array(&response, "nodeDecryptsTampered");
    assert_eq!(rejected.len(), tampered.len());
    for result in rejected {
        assert_eq!(
            result.get("ok").and_then(Value::as_bool),
            Some(false),
            "node accepted a tampered rust record: {result}"
        );
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
fn key_parsing_and_error_strings_match_the_node_reference() {
    let root = repo_root();
    if !node_reference_available(&root) {
        return;
    }

    let valid = parity_key();
    let nel_wrapped = format!("\u{85}{valid}\u{85}");
    let bom_wrapped = format!("\u{feff}{valid}\u{feff}");
    let candidates: Vec<String> = vec![
        valid.clone(),
        format!("  {valid}  "),                                   // trimmed
        nel_wrapped,                                              // JS preserves NEL
        bom_wrapped,                                              // JS trims BOM
        BASE64.encode([0_u8; KEY_BYTES]),                         // all-zero but canonical
        String::new(),                                            // unset → plaintext fallback
        "   ".to_owned(),                                         // blank → plaintext fallback
        BASE64.encode([0_u8; 31]),                                // 31 bytes
        BASE64.encode([0_u8; 33]),                                // 33 bytes
        valid.replace('=', ""),                                   // unpadded
        format!("{valid}="),                                      // over-padded
        "AAAA".to_owned(),                                        // canonical, wrong length
        "not base64!!".to_owned(),                                // invalid characters
        "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8".to_owned(), // 32 bytes, unpadded
    ];

    let mut payload = request(&valid, &[], &[], &[]);
    payload["keyCandidates"] = json!(candidates);
    let response = run_node(&root, &payload);

    // Both runtimes must classify every candidate identically.
    let results = array(&response, "keyResults");
    assert_eq!(results.len(), candidates.len());
    assert_eq!(
        results[2].get("accepted").and_then(Value::as_bool),
        Some(false),
        "the real Node parser must preserve U+0085 NEL and reject the wrapped key"
    );
    assert_eq!(
        results[3].get("accepted").and_then(Value::as_bool),
        Some(true),
        "the real Node parser must trim U+FEFF BOM and accept the wrapped key"
    );
    for (result, candidate) in results.iter().zip(candidates.iter()) {
        let node_accepted = result
            .get("accepted")
            .and_then(Value::as_bool)
            .expect("node key result");
        let node_error = result.get("error").and_then(Value::as_str);
        match parse_encryption_key(Some(candidate)) {
            Ok(Some(_)) => assert!(
                node_accepted,
                "rust accepted a key node rejected: {candidate:?} ({node_error:?})"
            ),
            Ok(None) => {
                assert!(
                    !node_accepted,
                    "rust treated {candidate:?} as unset, node did not"
                );
                assert_eq!(
                    node_error, None,
                    "node threw for a blank key: {candidate:?}"
                );
            }
            Err(error) => {
                assert!(
                    !node_accepted,
                    "rust rejected a key node accepted: {candidate:?}"
                );
                assert_eq!(
                    node_error,
                    Some(INVALID_KEY_MESSAGE),
                    "node's rejection message diverged for {candidate:?}"
                );
                assert_eq!(error, AppError::Validation(INVALID_KEY_MESSAGE.to_owned()));
            }
        }
    }

    // The three verbatim Node strings this unit reproduces.
    assert_eq!(
        response.get("invalidKeyError").and_then(Value::as_str),
        Some(INVALID_KEY_MESSAGE)
    );
    assert_eq!(
        response.get("missingKeyError").and_then(Value::as_str),
        Some(MISSING_KEY_MESSAGE)
    );
    assert_eq!(
        response.get("warning").and_then(Value::as_str),
        Some(PLAINTEXT_FALLBACK_WARNING)
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
fn a_pinned_nonce_pins_the_whole_record() {
    // Guards the format against an accidental change of AAD, tag length, or field encoding: with
    // a fixed key and a fixed nonce the three columns are fully determined.
    let cipher = WebhookCipher::with_random(
        &Secret::new(parity_key()),
        Arc::new(ScriptedRandom::new(vec![0xAB])),
    )
    .expect("valid key");
    let record = cipher.encrypt("artifact-mcp").expect("encrypt");

    assert_eq!(record.nonce, BASE64.encode([0xAB_u8; NONCE_BYTES]));
    assert_eq!(
        BASE64.decode(&record.ciphertext).expect("ciphertext").len(),
        "artifact-mcp".len()
    );
    assert_eq!(BASE64.decode(&record.tag).expect("tag").len(), TAG_BYTES);
    assert_eq!(cipher.decrypt(&record).expect("decrypt"), "artifact-mcp");
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
