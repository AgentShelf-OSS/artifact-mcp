use rusqlite::{Connection, OptionalExtension as _, Transaction, TransactionBehavior};

use crate::config::{RandomSource, Secret};
use crate::error::AppError;
use crate::model::{
    ClientId, CreatePublisherKey, CreatedPublisherKey, KeyOwnerUpdate, OrgId, OwnerBackfillResult,
    PublisherKeySummary, Timestamp, UpdatePublisherKey, UpdatedPublisherKey,
};
use crate::persistence::orgs::{js_trim, truncate_utf16};

use super::validation::{
    DEFAULT_SEED_ORG, INVALID_CLIENT_ID_MESSAGE, INVALID_KEY_ROLE_MESSAGE, INVALID_ORG_MESSAGE,
    KEY_LABEL_MAX_LENGTH, PLACEHOLDER_SECRETS, database_failure, duplicate_key_message,
    generate_secret, is_valid_client_id, is_valid_key_org, key_hash,
};

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
pub(super) fn normalize_verified_owner(
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

pub(super) fn update_key_state(
    conn: &Connection,
    client_id: &str,
    request: &UpdatePublisherKey,
) -> Result<Option<(UpdatedPublisherKey, bool)>, AppError> {
    let current = conn
        .query_row(
            "SELECT org, label, role, owner_email FROM api_keys WHERE client_id = ?1",
            [client_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<String>>(3)?,
                ))
            },
        )
        .optional()
        .map_err(|error| database_failure("load key metadata", &error))?;
    let Some((org, current_label, current_role, current_owner)) = current else {
        return Ok(None);
    };
    let label = truncate_utf16(js_trim(&request.label), KEY_LABEL_MAX_LENGTH);
    let role = js_trim(&request.role).to_owned();
    if !matches!(role.as_str(), "reader" | "author" | "collaborator") {
        return Err(AppError::Validation(INVALID_KEY_ROLE_MESSAGE.to_owned()));
    }
    let owner_email = normalize_verified_owner(conn, request.owner_email.as_deref(), &org)?;
    let changed = label != current_label || role != current_role || owner_email != current_owner;
    if changed {
        conn.execute(
            "UPDATE api_keys SET label = ?1, role = ?2, owner_email = ?3 WHERE client_id = ?4",
            rusqlite::params![label, role, owner_email, client_id],
        )
        .map_err(|error| database_failure("update key metadata", &error))?;
    }
    Ok(Some((
        UpdatedPublisherKey {
            client_id: ClientId(client_id.to_owned()),
            org: OrgId(org),
            label,
            role,
            owner_email,
        },
        changed,
    )))
}

/// Update the mutable metadata on one credential. Identity, tenant, creation, and revocation
/// history are intentionally immutable.
pub fn update_key(
    conn: &Connection,
    client_id: &str,
    request: &UpdatePublisherKey,
) -> Result<Option<UpdatedPublisherKey>, AppError> {
    update_key_state(conn, client_id, request).map(|result| result.map(|(updated, _)| updated))
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
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|error| database_failure("start owner backfill", &error))?;
    let result = backfill_key_owner_in_transaction(&transaction, client_id, owner_email, confirm)?;
    transaction
        .commit()
        .map_err(|error| database_failure("commit owner backfill", &error))?;
    Ok(result)
}

pub(super) fn backfill_key_owner_in_transaction(
    transaction: &Transaction<'_>,
    client_id: &str,
    owner_email: &str,
    confirm: bool,
) -> Result<Option<OwnerBackfillResult>, AppError> {
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
    let Some(owner_email) = normalize_verified_owner(transaction, Some(owner_email), &org)? else {
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
