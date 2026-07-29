//! Owned by U09 (terra) — publisher key persistence.
//!
//! Node oracle: `lib/keys.js` (create/list/revoke) and `seedKeysFromEnv` in `lib/db.js:30-59`
//! (the bootstrap path). Both are ported literally, including error strings, because
//! `/settings/keys` returns a thrown message verbatim as `400 {"error": …}`
//! ([lib/app.js:277-279]).
//!
//! # The secret exists for exactly one response
//!
//! A publisher key's secret is generated, hashed, and shown once; only `sha256Hex(secret)` is
//! ever written to `api_keys.key_hash` ([lib/keys.js:38-39]). Inside this module the raw value
//! never exists as a bare `String`: it is a [`Secret`], whose `Debug`/`Display` are redacted, and
//! it is unwrapped only when the frozen [`CreatedPublisherKey`] is built for the caller. No error,
//! log line, or database column can therefore carry it.
//!
//! **Contract-delta request:** `CreatedPublisherKey.secret` is a plain `String` on a struct that
//! derives `Debug` (`src/model/admin.rs:23-29`), so `{:?}` on the *frozen model* prints the live
//! secret. `config::Secret` already exists for exactly this purpose. Changing the field type is a
//! U01-owned edit; U09 raises it rather than making it.
//!
//! # Bootstrap seeding
//!
//! `ARTIFACT_API_KEYS` seeding is `INSERT … ON CONFLICT(client_id) DO NOTHING`: it may create a
//! key, never update or un-revoke one, so keys managed in Settings stay authoritative
//! ([lib/db.js:28-29]). Documented placeholder secrets are refused outright ([lib/db.js:36-38]) —
//! copying `.env.example` unchanged must not leave a publicly known key live on `/mcp`.

use std::fmt::Write as _;
use std::sync::Arc;

use rusqlite::{Connection, OptionalExtension as _};

use crate::artifacts::digest::sha256_hex;
use crate::config::{OsRandom, RandomSource, Secret};
use crate::error::AppError;
use crate::model::{
    ClientId, CreatePublisherKey, CreatedPublisherKey, KeyOwnerUpdate, OrgId, OwnerBackfillResult,
    PublisherKeySummary, Timestamp,
};
use crate::persistence::db::{self, DbPool};
use crate::persistence::orgs::{js_trim, truncate_utf16};

/// `crypto.randomBytes(24)` — [lib/keys.js:38]. Rendered as 48 lowercase hex characters.
pub const SECRET_BYTES: usize = 24;

/// `label.trim().slice(0, 60)` — [lib/keys.js:28]
pub const KEY_LABEL_MAX_LENGTH: usize = 60;

/// `NAME_RE = /^[a-z0-9][a-z0-9._-]{1,40}$/i` — [lib/keys.js:18]
pub const CLIENT_ID_MIN_LENGTH: usize = 2;

/// See [`CLIENT_ID_MIN_LENGTH`].
pub const CLIENT_ID_MAX_LENGTH: usize = 41;

/// `ORG_RE = /^[a-z0-9][a-z0-9._-]{0,40}$/i` — [lib/keys.js:19]
pub const KEY_ORG_MAX_LENGTH: usize = 41;

/// Org assigned to a two-part `clientId:secret` seed entry — [lib/db.js:27]
pub const DEFAULT_SEED_ORG: &str = "default";

/// Secrets that are never seeded, because they are printed in `.env.example` and the README.
/// [lib/db.js:38]
pub const PLACEHOLDER_SECRETS: [&str; 2] = ["CHANGE_ME", "REPLACE_WITH_LONG_RANDOM_SECRET"];

/// `Name must be 2–41 characters: …` — [lib/keys.js:30]. The dash is U+2013 (EN DASH).
pub const INVALID_CLIENT_ID_MESSAGE: &str =
    "Name must be 2–41 characters: letters, numbers, dot, dash, underscore.";

/// `Org must be …` — [lib/keys.js:33]
pub const INVALID_ORG_MESSAGE: &str = "Org must be letters, numbers, dot, dash, or underscore.";
pub const INVALID_KEY_ROLE_MESSAGE: &str = "Role must be reader, author, or collaborator.";

/// `/^[a-z0-9][a-z0-9._-]{1,40}$/i` — [lib/keys.js:18]
#[must_use]
pub fn is_valid_client_id(value: &str) -> bool {
    let mut characters = value.chars();
    let Some(first) = characters.next() else {
        return false;
    };
    let rest = characters.clone().count();
    first.is_ascii_alphanumeric()
        && (1..=CLIENT_ID_MAX_LENGTH - 1).contains(&rest)
        && characters.all(is_name_character)
}

/// `/^[a-z0-9][a-z0-9._-]{0,40}$/i` — [lib/keys.js:19]
///
/// Identical to `orgs::is_valid_org_name`; both files carry the same regex and both are ported
/// against their own source line.
#[must_use]
pub fn is_valid_key_org(value: &str) -> bool {
    let mut characters = value.chars();
    let Some(first) = characters.next() else {
        return false;
    };
    first.is_ascii_alphanumeric()
        && characters.clone().count() < KEY_ORG_MAX_LENGTH
        && characters.all(is_name_character)
}

fn is_name_character(value: char) -> bool {
    value.is_ascii_alphanumeric() || matches!(value, '.' | '_' | '-')
}

/// `A key named "<id>" already exists.` — [lib/keys.js:36]
#[must_use]
pub fn duplicate_key_message(client_id: &str) -> String {
    format!("A key named \"{client_id}\" already exists.")
}

/// `sha256Hex(secret)` — [lib/auth.js:6], the only representation of a secret that is persisted.
///
/// Reuses U07's `createHash("sha256").update(x).digest("hex")` port so the project has exactly
/// one implementation of that primitive.
#[must_use]
pub fn key_hash(secret: &Secret) -> String {
    sha256_hex(secret.expose().as_bytes())
}

/// `crypto.randomBytes(24).toString("hex")` — [lib/keys.js:38]
///
/// # Errors
/// [`AppError::Internal`] when the entropy source fails.
pub fn generate_secret(random: &dyn RandomSource) -> Result<Secret, AppError> {
    let mut bytes = [0_u8; SECRET_BYTES];
    random.fill_bytes(&mut bytes)?;
    let mut hex = String::with_capacity(SECRET_BYTES * 2);
    for byte in bytes {
        // `Buffer.toString("hex")` is lowercase and zero padded.
        write!(hex, "{byte:02x}").map_err(|_| AppError::Internal)?;
    }
    Ok(Secret::new(hex))
}

/// Reports a SQLite failure without leaking the statement's parameters.
fn database_failure(operation: &str, error: &rusqlite::Error) -> AppError {
    tracing::error!(operation, error = %error, "publisher key persistence failed");
    AppError::Internal
}

// ---------------------------------------------------------------------------
// Synchronous operations
// ---------------------------------------------------------------------------

/// `listKeys()` — [lib/keys.js:9-11], [lib/keys.js:21-23]
///
/// Active keys first, then by org, then by client id — the order the Settings page renders.
///
/// # Errors
/// [`AppError::Internal`] when the query fails.
pub fn list_keys(conn: &Connection) -> Result<Vec<PublisherKeySummary>, AppError> {
    let mut statement = conn
        .prepare(
            "SELECT client_id, org, label, role, owner_email, created_at, revoked_at FROM api_keys \
             ORDER BY (revoked_at IS NOT NULL), org, client_id",
        )
        .map_err(|error| database_failure("list keys", &error))?;
    let rows = statement
        .query_map([], |row| {
            Ok(PublisherKeySummary {
                client_id: ClientId(row.get::<_, String>(0)?),
                org: OrgId(row.get::<_, String>(1)?),
                label: row.get::<_, String>(2)?,
                role: row.get::<_, String>(3)?,
                owner_email: row.get(4)?,
                created_at: Timestamp(row.get::<_, String>(5)?),
                revoked_at: row.get::<_, Option<String>>(6)?.map(Timestamp),
            })
        })
        .map_err(|error| database_failure("list keys", &error))?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(|error| database_failure("list keys", &error))
}

/// `createKey({ clientId, org, label })` — [lib/keys.js:25-41]
///
/// The generated secret is returned once and never stored; `api_keys.key_hash` holds
/// [`key_hash`] alone.
///
/// # Errors
/// [`AppError::Validation`] for a malformed client id or org, or a client id already in use;
/// [`AppError::Internal`] when the entropy source or the insert fails.
pub fn create_key(
    conn: &Connection,
    request: &CreatePublisherKey,
    random: &dyn RandomSource,
) -> Result<CreatedPublisherKey, AppError> {
    let client_id = js_trim(&request.client_id.0).to_owned();
    let org = js_trim(&request.org.0).to_owned();
    let label = truncate_utf16(js_trim(&request.label), KEY_LABEL_MAX_LENGTH);
    let role = match js_trim(&request.role) {
        "" => "author".to_owned(),
        value => value.to_owned(),
    };
    let owner_email = normalize_verified_owner(conn, request.owner_email.as_deref(), &org)?;
    if !is_valid_client_id(&client_id) {
        return Err(AppError::Validation(INVALID_CLIENT_ID_MESSAGE.to_owned()));
    }
    if !is_valid_key_org(&org) {
        return Err(AppError::Validation(INVALID_ORG_MESSAGE.to_owned()));
    }
    if !matches!(role.as_str(), "reader" | "author" | "collaborator") {
        return Err(AppError::Validation(INVALID_KEY_ROLE_MESSAGE.to_owned()));
    }
    if key_exists(conn, &client_id)? {
        return Err(AppError::Validation(duplicate_key_message(&client_id)));
    }

    let secret = generate_secret(random)?;
    conn.execute(
        "INSERT INTO api_keys (client_id, org, label, role, owner_email, key_hash) VALUES (?, ?, ?, ?, ?, ?)",
        rusqlite::params![client_id, org, label, role, owner_email, key_hash(&secret)],
    )
    .map_err(|error| {
        // `client_id` is the primary key: the only way that constraint fires after the check
        // above is a concurrent insert of the same name, so it is reported as the same conflict
        // rather than as an opaque internal error. Any other constraint — notably the
        // `key_hash` uniqueness a repeated secret would violate — stays internal.
        if violates_client_id_uniqueness(&error) {
            AppError::Validation(duplicate_key_message(&client_id))
        } else {
            database_failure("create key", &error)
        }
    })?;

    Ok(CreatedPublisherKey {
        client_id: ClientId(client_id),
        org: OrgId(org),
        label,
        role,
        owner_email,
        // The single point where the raw secret leaves this module.
        secret: secret.expose().to_owned(),
    })
}

/// A key owner must be an already verified member of exactly this organization.  This mirrors
/// the identity directory's explicit-email or registered-domain routing without accepting a
/// free-form display label as an authorization principal.
fn normalize_verified_owner(
    conn: &Connection,
    candidate: Option<&str>,
    org: &str,
) -> Result<Option<String>, AppError> {
    let email = js_trim(candidate.unwrap_or_default()).to_lowercase();
    if email.is_empty() {
        return Ok(None);
    }
    let domain = email
        .rsplit_once('@')
        .map(|(_, domain)| domain)
        .unwrap_or_default();
    let verified = conn
        .query_row(
            "SELECT 1 WHERE EXISTS (SELECT 1 FROM org_email_members WHERE email = ?1 AND org = ?2) \
             OR EXISTS (SELECT 1 FROM org_domains WHERE domain = ?3 AND org = ?2)",
            rusqlite::params![email, org, domain],
            |_| Ok(()),
        )
        .optional()
        .map_err(|error| database_failure("verify key owner", &error))?
        .is_some();
    if !verified {
        return Err(AppError::Validation(
            "Owner must be a verified member of this organization.".to_owned(),
        ));
    }
    Ok(Some(email))
}

/// `revokeKey(clientId)` — [lib/keys.js:43-45]
///
/// Node passes the client id through untrimmed, so a padded id revokes nothing; preserved.
/// Revoking an already-revoked key returns `false` because the statement requires
/// `revoked_at IS NULL`.
///
/// # Errors
/// [`AppError::Internal`] when the update fails.
pub fn revoke_key(conn: &Connection, client_id: &str) -> Result<bool, AppError> {
    conn.execute(
        "UPDATE api_keys SET revoked_at = datetime('now') WHERE client_id = ? AND revoked_at IS NULL",
        [client_id],
    )
    .map(|changes| changes > 0)
    .map_err(|error| database_failure("revoke key", &error))
}

/// Change a verified owner on the key only. Artifact rows contain their own immutable snapshots,
/// so this affects future publishes and cannot transfer visibility control retroactively.
pub fn set_key_owner(
    conn: &Connection,
    client_id: &str,
    owner_email: Option<&str>,
) -> Result<Option<KeyOwnerUpdate>, AppError> {
    let Some(org) = conn
        .query_row(
            "SELECT org FROM api_keys WHERE client_id = ?1",
            [client_id],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(|error| database_failure("load key owner", &error))?
    else {
        return Ok(None);
    };
    let owner_email = normalize_verified_owner(conn, owner_email, &org)?;
    conn.execute(
        "UPDATE api_keys SET owner_email = ?1 WHERE client_id = ?2",
        rusqlite::params![owner_email, client_id],
    )
    .map_err(|error| database_failure("set key owner", &error))?;
    Ok(Some(KeyOwnerUpdate {
        client_id: ClientId(client_id.to_owned()),
        org: OrgId(org),
        owner_email,
    }))
}

/// Preview (the default) or confirm a null-owner-only migration for exactly one key/current-org.
/// The update predicate never overwrites owner attribution, including during a concurrent retry.
pub fn backfill_key_owner(
    conn: &mut Connection,
    client_id: &str,
    owner_email: &str,
    confirm: bool,
) -> Result<Option<OwnerBackfillResult>, AppError> {
    let transaction = conn
        .transaction()
        .map_err(|error| database_failure("start owner backfill", &error))?;
    let Some(org) = transaction
        .query_row(
            "SELECT org FROM api_keys WHERE client_id = ?1",
            [client_id],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(|error| database_failure("load backfill key", &error))?
    else {
        return Ok(None);
    };
    let Some(owner_email) = normalize_verified_owner(&transaction, Some(owner_email), &org)? else {
        return Err(AppError::Validation(
            "Owner is required for backfill.".to_owned(),
        ));
    };
    let matched: i64 = transaction
        .query_row(
            "SELECT COUNT(*) FROM artifacts WHERE client_id = ?1 AND org = ?2 AND owner_email IS NULL",
            rusqlite::params![client_id, org],
            |row| row.get(0),
        )
        .map_err(|error| database_failure("count owner backfill", &error))?;
    let updated = if confirm {
        transaction
            .execute(
                "UPDATE artifacts SET owner_email = ?1 \
                 WHERE client_id = ?2 AND org = ?3 AND owner_email IS NULL",
                rusqlite::params![owner_email, client_id, org],
            )
            .map_err(|error| database_failure("apply owner backfill", &error))?
    } else {
        0
    };
    transaction
        .commit()
        .map_err(|error| database_failure("commit owner backfill", &error))?;
    Ok(Some(OwnerBackfillResult {
        client_id: ClientId(client_id.to_owned()),
        org: OrgId(org),
        owner_email,
        matched: u64::try_from(matched).unwrap_or_default(),
        updated: u64::try_from(updated).unwrap_or_default(),
        confirmed: confirm,
    }))
}

/// `seedKeysFromEnv(sha256Hex, raw)` — [lib/db.js:30-59]
///
/// Accepts `clientId:org:secret` and the two-part `clientId:secret` back-compat form (org
/// `default`). Every part is trimmed individually *before* a three-or-more-part secret is
/// rejoined on `:`, so `a : b : c : d` seeds the secret `c:d`. Entries with an empty client id or
/// secret are skipped, as are the documented placeholder secrets.
///
/// Returns the number of rows actually inserted.
///
/// # Errors
/// [`AppError::Internal`] when the insert fails — including the `key_hash` uniqueness violation
/// two identical secrets would cause.
pub fn seed_keys_from_env(conn: &Connection, raw: &str) -> Result<u64, AppError> {
    if js_trim(raw).is_empty() {
        return Ok(0);
    }
    let mut statement = conn
        .prepare(
            "INSERT INTO api_keys (client_id, org, key_hash) VALUES (?, ?, ?) \
             ON CONFLICT(client_id) DO NOTHING",
        )
        .map_err(|error| database_failure("seed keys", &error))?;

    let mut seeded = 0_usize;
    for entry in raw.split(',') {
        let parts: Vec<&str> = entry.split(':').map(js_trim).collect();
        let (client_id, org, secret) = match parts.len() {
            0 | 1 => continue,
            2 => (parts[0], DEFAULT_SEED_ORG, parts[1].to_owned()),
            _ => (parts[0], parts[1], parts[2..].join(":")),
        };
        if client_id.is_empty() || secret.is_empty() {
            continue;
        }
        if PLACEHOLDER_SECRETS.contains(&secret.as_str()) {
            // [lib/db.js:52] — same wording as the Node warning, and no secret in the log.
            tracing::warn!(
                "[artifact-mcp] ignoring placeholder key secret for \"{client_id}\" — set a real ARTIFACT_API_KEYS value"
            );
            continue;
        }
        let org = if org.is_empty() {
            DEFAULT_SEED_ORG
        } else {
            org
        };
        let hash = key_hash(&Secret::new(secret));
        let changes = statement
            .execute(rusqlite::params![client_id, org, hash])
            .map_err(|error| database_failure("seed keys", &error))?;
        seeded += changes;
    }
    Ok(u64::try_from(seeded).unwrap_or(u64::MAX))
}

/// `existsStmt` — [lib/keys.js:12]
fn key_exists(conn: &Connection, client_id: &str) -> Result<bool, AppError> {
    conn.query_row(
        "SELECT 1 FROM api_keys WHERE client_id = ?",
        [client_id],
        |_| Ok(()),
    )
    .optional()
    .map(|found| found.is_some())
    .map_err(|error| database_failure("key exists", &error))
}

/// True only for `UNIQUE constraint failed: api_keys.client_id`.
fn violates_client_id_uniqueness(error: &rusqlite::Error) -> bool {
    match error {
        rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error {
                code: rusqlite::ErrorCode::ConstraintViolation,
                ..
            },
            Some(message),
        ) => message.contains("api_keys.client_id"),
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// Pooled adapter
// ---------------------------------------------------------------------------

/// The `AdminService` publisher-key surface backed by U03's pool.
///
/// See [`crate::persistence::orgs::OrgStore`] for why the frozen trait is not implemented here.
#[derive(Clone)]
pub struct KeyStore {
    pool: DbPool,
    random: Arc<dyn RandomSource>,
}

impl KeyStore {
    /// Wrap a pool with operating-system entropy.
    #[must_use]
    pub fn new(pool: DbPool) -> Self {
        Self::with_random(pool, Arc::new(OsRandom))
    }

    /// Wrap a pool with an injected entropy source (deterministic in tests).
    #[must_use]
    pub const fn with_random(pool: DbPool, random: Arc<dyn RandomSource>) -> Self {
        Self { pool, random }
    }

    /// See [`list_keys`].
    ///
    /// # Errors
    /// Propagates the synchronous operation's error.
    pub async fn list_keys(&self) -> Result<Vec<PublisherKeySummary>, AppError> {
        db::interact(&self.pool, |conn| list_keys(conn)).await
    }

    /// See [`create_key`].
    ///
    /// # Errors
    /// Propagates the synchronous operation's error.
    pub async fn create_key(
        &self,
        request: CreatePublisherKey,
    ) -> Result<CreatedPublisherKey, AppError> {
        let random = Arc::clone(&self.random);
        db::interact(&self.pool, move |conn| {
            create_key(conn, &request, random.as_ref())
        })
        .await
    }

    /// See [`revoke_key`].
    ///
    /// # Errors
    /// Propagates the synchronous operation's error.
    pub async fn revoke_key(&self, client_id: &ClientId) -> Result<bool, AppError> {
        let client_id = client_id.0.clone();
        db::interact(&self.pool, move |conn| revoke_key(conn, &client_id)).await
    }

    pub async fn set_key_owner(
        &self,
        client_id: ClientId,
        owner_email: Option<String>,
    ) -> Result<Option<KeyOwnerUpdate>, AppError> {
        db::interact(&self.pool, move |conn| {
            set_key_owner(conn, &client_id.0, owner_email.as_deref())
        })
        .await
    }

    pub async fn backfill_key_owner(
        &self,
        client_id: ClientId,
        owner_email: String,
        confirm: bool,
    ) -> Result<Option<OwnerBackfillResult>, AppError> {
        db::interact(&self.pool, move |conn| {
            backfill_key_owner(conn, &client_id.0, &owner_email, confirm)
        })
        .await
    }

    /// See [`seed_keys_from_env`].
    ///
    /// # Errors
    /// Propagates the synchronous operation's error.
    pub async fn seed_from_env(&self, raw: &str) -> Result<u64, AppError> {
        let raw = raw.to_owned();
        db::interact(&self.pool, move |conn| seed_keys_from_env(conn, &raw)).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::SeededRandom;

    #[test]
    fn accepts_only_node_shaped_client_ids() {
        assert!(is_valid_client_id("ab"));
        assert!(is_valid_client_id("A.b_c-9"));
        assert!(is_valid_client_id(&format!("a{}", "b".repeat(40))));
        assert!(!is_valid_client_id(&format!("a{}", "b".repeat(41))));
        assert!(!is_valid_client_id("a"));
        assert!(!is_valid_client_id(""));
        assert!(!is_valid_client_id(".ab"));
        assert!(!is_valid_client_id("a b"));
    }

    #[test]
    fn accepts_single_character_orgs_but_not_single_character_names() {
        assert!(is_valid_key_org("a"));
        assert!(!is_valid_client_id("a"));
    }

    #[test]
    fn generates_a_48_character_lowercase_hex_secret() {
        let secret = generate_secret(&SeededRandom::new(7)).expect("generate");
        assert_eq!(secret.expose().len(), SECRET_BYTES * 2);
        assert!(
            secret
                .expose()
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        );
    }

    #[test]
    fn never_renders_a_secret_through_debug_or_display() {
        let secret = generate_secret(&SeededRandom::new(11)).expect("generate");
        let rendered = format!("{secret:?} {secret}");
        assert!(!rendered.contains(secret.expose()));
        assert_eq!(rendered, "Secret(<redacted>) <redacted>");
    }

    #[test]
    fn hashes_the_secret_exactly_as_node_does() {
        // `crypto.createHash("sha256").update("abc").digest("hex")`
        assert_eq!(
            key_hash(&Secret::new("abc")),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
