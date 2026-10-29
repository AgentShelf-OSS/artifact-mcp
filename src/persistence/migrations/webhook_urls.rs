//! At-rest encryption for webhook URLs: the cipher seam, display masking, and the plaintext
//! conversion pass. Re-exported from [`super`] so public paths stay stable.

use std::fmt::Display;

use rusqlite::Connection;

use crate::error::AppError;

/// Encrypted webhook URL record produced by U04's cipher.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EncryptedUrl {
    /// Base64 ciphertext.
    pub ciphertext: String,
    /// Base64 nonce.
    pub nonce: String,
    /// Base64 authentication tag.
    pub tag: String,
}

/// Injection seam for the U04 webhook cipher.
///
/// U03 owns the at-rest conversion of legacy plaintext webhook rows (blueprint A4 startup step 5)
/// but must not own the AEAD itself. `src/security/crypto.rs` (U04) provides the implementation;
/// when no key is configured the caller passes `None` and rows stay plaintext, matching
/// `encryptPlaintextWebhookUrls` returning `0` in the Node reference.
pub trait WebhookUrlCipher: Send + Sync {
    /// Encrypts one webhook URL, returning the three at-rest columns.
    ///
    /// # Errors
    /// Returns an [`AppError`] when the configured key cannot encrypt the value.
    fn encrypt_url(&self, plaintext: &str) -> Result<EncryptedUrl, AppError>;
}

/// Port of `maskWebhookUrl` in `lib/migrations.js`.
///
/// The masked value is what remains in the plaintext `url` column once the row is encrypted, so it
/// must never contain the token segment. Node builds `${protocol}//${host}/…${url.slice(-4)}` and
/// falls back to `…${url.slice(-4)}` for unparsable values.
#[must_use]
pub fn mask_webhook_url(value: &str) -> String {
    let suffix = last_four(value);
    url::Url::parse(value).map_or_else(
        |_| format!("…{suffix}"),
        |parsed| {
            let mut host = parsed.host_str().unwrap_or_default().to_owned();
            if let Some(port) = parsed.port() {
                host.push(':');
                host.push_str(&port.to_string());
            }
            format!("{}://{host}/…{suffix}", parsed.scheme())
        },
    )
}

/// Equivalent of JavaScript `value.slice(-4)` for the ASCII URLs stored in this column.
/// Port of JavaScript `url.slice(-4)` — the last four **UTF-16 code units**, not characters.
///
/// `String::chars()` diverges for astral characters: for `https://example.test/🎉🎉🎉`, Node yields
/// `🎉🎉` (each emoji is two UTF-16 units) while a `chars()`-based slice yields `/🎉🎉🎉`. This value
/// is written into the `url` column during plaintext→encrypted conversion, so the divergence would
/// persist at rest. Flagged by U12, which verified the Node behaviour empirically.
///
/// If the 4-unit window splits a surrogate pair, JavaScript emits a lone surrogate; Rust cannot
/// represent one, so the straddling character is dropped (same policy U09 adopted for UTF-16
/// truncation).
fn last_four(value: &str) -> String {
    let units: Vec<u16> = value.encode_utf16().collect();
    let start = units.len().saturating_sub(4);
    let mut window = &units[start..];
    // A leading trailing-surrogate is the orphaned half of a pair straddling the boundary.
    if window
        .first()
        .is_some_and(|u| (0xDC00..=0xDFFF).contains(u))
    {
        window = &window[1..];
    }
    String::from_utf16_lossy(window)
}

/// Port of `encryptPlaintextWebhookUrls`: converts legacy plaintext rows at rest.
///
/// Selects every row whose ciphertext column is still NULL and whose URL is non-blank, then
/// rewrites `url` to the masked display value and fills the three encrypted columns in one
/// transaction. The UPDATE keeps the `url_cipher IS NULL` guard so a concurrent conversion cannot
/// double-encrypt. Returns the number of converted rows.
///
/// This is blocking work; run it inside `spawn_blocking` or the synchronous bootstrap path.
///
/// # Errors
/// Returns [`AppError::Internal`] if the database rejects the read or write, or propagates the
/// cipher's error. No SQL, path, or URL detail is ever placed in the returned error.
pub fn encrypt_plaintext_webhook_urls(
    conn: &mut Connection,
    cipher: &dyn WebhookUrlCipher,
) -> Result<usize, AppError> {
    let tx = conn.transaction().map_err(internal)?;
    let mut changed = 0_usize;
    // Both tables use the exact same at-rest record. Table/identifier strings are constants, not
    // caller input, so the dynamic SQL cannot expose an injection surface.
    for (table, key) in [
        ("org_webhooks", "id"),
        ("org_discord_discussion_connections", "org"),
    ] {
        let rows: Vec<(String, String)> = {
            let mut stmt = tx
                .prepare(&format!(
                    "SELECT {key}, url FROM {table} WHERE url_cipher IS NULL AND trim(url) <> ''"
                ))
                .map_err(internal)?;
            stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
                .map_err(internal)?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(internal)?
        };
        let mut update = tx
            .prepare(&format!(
                "UPDATE {table} SET url = ?1, url_cipher = ?2, url_nonce = ?3, url_tag = ?4 \
                 WHERE {key} = ?5 AND url_cipher IS NULL"
            ))
            .map_err(internal)?;
        for (id, url) in rows {
            let encrypted = cipher.encrypt_url(&url)?;
            changed += update
                .execute((
                    mask_webhook_url(&url),
                    encrypted.ciphertext,
                    encrypted.nonce,
                    encrypted.tag,
                    id,
                ))
                .map_err(internal)?;
        }
    }
    tx.commit().map_err(internal)?;

    tracing::info!(
        converted = changed,
        "encrypted existing webhook URL(s) at rest"
    );
    Ok(changed)
}

/// Database faults never leak SQL, paths, or row contents into an HTTP body.
fn internal(error: impl Display) -> AppError {
    tracing::error!(error = %error, "webhook url conversion failed");
    AppError::Internal
}
