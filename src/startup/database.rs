//! Database stage: open and migrate SQLite, report effective pragmas, and seed publisher keys.

use std::sync::Arc;

use crate::{
    config::{AppConfig, SeedKeys},
    error::AppError,
    persistence::{
        db::{self, Database, DbPool},
        keys::{self, KeyStore},
        migrations::{self, MigrationContext},
    },
    security::{
        audit::{AuditEvent, MutationAudit, parse_hmac_key},
        crypto::{WebhookUrlProtection, warn_if_webhook_encryption_disabled},
    },
};

/// Build the webhook URL protection from the configured key and warn when it is absent.
///
/// # Errors
///
/// Returns an error when the configured key is invalid.
pub fn webhook_protection(config: &AppConfig) -> Result<Arc<WebhookUrlProtection>, AppError> {
    let protection = Arc::new(WebhookUrlProtection::from_config_key(
        config.webhook_enc_key.as_ref(),
    )?);
    if config.webhook_enc_key.is_none() {
        let _ = warn_if_webhook_encryption_disabled(None);
    }
    Ok(protection)
}

/// Open the SQLite pool and apply migrations on a blocking thread.
///
/// # Errors
///
/// Returns an error when the database cannot be opened or migrated.
pub async fn open_database(
    config: &AppConfig,
    protection: &Arc<WebhookUrlProtection>,
) -> Result<DbPool, AppError> {
    let data_dir = config.data_dir.clone();
    let migration_context = migration_context(config);
    let protection_for_open = Arc::clone(protection);
    tokio::task::spawn_blocking(move || {
        let cipher = protection_for_open
            .cipher()
            .map(|cipher| cipher as &dyn migrations::WebhookUrlCipher);
        Database::open_with(&data_dir, &migration_context, cipher)
    })
    .await
    .map_err(|error| {
        tracing::error!(error = %error, "database bootstrap task failed");
        AppError::Internal
    })?
}

/// Verify and log the schema version and the effective connection pragmas.
///
/// # Errors
///
/// Returns an error when the pragmas do not match the pinned values or cannot be read.
pub async fn log_effective_pragmas(pool: &DbPool) -> Result<(), AppError> {
    let (schema_version, pragmas) = effective_pragmas(pool).await?;
    tracing::info!(schema_version, "schema version ready");
    tracing::info!(
        journal_mode = %pragmas.journal_mode,
        synchronous = pragmas.synchronous,
        busy_timeout_ms = pragmas.busy_timeout,
        wal_autocheckpoint_pages = pragmas.wal_autocheckpoint,
        foreign_keys = pragmas.foreign_keys,
        page_size = pragmas.page_size,
        "effective database pragmas"
    );
    Ok(())
}

/// Decode the required audit-ledger HMAC key.
///
/// # Errors
///
/// Returns `AppError::Validation` when the key is missing or malformed.
pub fn audit_ledger_key(config: &AppConfig) -> Result<[u8; 32], AppError> {
    parse_hmac_key(
        config.audit_ledger_hmac_key.as_ref().ok_or_else(|| AppError::Validation("AUDIT_LEDGER_HMAC_KEY is required; refusing to start without a tamper-evident audit ledger".to_owned()))?.expose(),
    )
}

/// Seed the configured publisher keys and warn when no active key remains.
///
/// # Errors
///
/// Returns an error when seeding or the key listing fails.
pub async fn seed_publisher_keys(
    pool: &DbPool,
    config: &AppConfig,
    audit_key: [u8; 32],
) -> Result<KeyStore, AppError> {
    let seeded = seed_configured_keys(pool, config.seed_keys.clone(), audit_key).await?;
    tracing::info!(seeded_keys = seeded, "publisher key seed complete");
    let key_store = KeyStore::new(pool.clone());
    if seeded == 0
        && key_store
            .list_keys()
            .await?
            .iter()
            .all(|key| key.revoked_at.is_some())
    {
        tracing::warn!("no active publisher keys configured; publishing is disabled");
    }
    Ok(key_store)
}

#[derive(Debug)]
struct EffectivePragmas {
    journal_mode: String,
    synchronous: i64,
    busy_timeout: i64,
    wal_autocheckpoint: i64,
    foreign_keys: i64,
    page_size: i64,
}

fn migration_context(config: &AppConfig) -> MigrationContext {
    MigrationContext {
        org_email_domains: config
            .access
            .domain_orgs
            .iter()
            .map(|(domain, org)| format!("{domain}:{org}"))
            .collect::<Vec<_>>()
            .join(","),
    }
}

#[tracing::instrument(skip_all)]
async fn seed_configured_keys(
    pool: &DbPool,
    seed_keys: SeedKeys,
    audit_key: [u8; 32],
) -> Result<u64, AppError> {
    for client_id in &seed_keys.ignored_placeholders {
        tracing::warn!(
            client_id = %client_id,
            "ignoring placeholder publisher key secret"
        );
    }
    let pool = pool.clone();
    db::interact(&pool, move |conn| {
        let tx = conn.transaction().map_err(|_| AppError::Internal)?;
        let mut statement = tx
            .prepare(
                "INSERT INTO api_keys (client_id, org, key_hash) VALUES (?1, ?2, ?3) \
                 ON CONFLICT(client_id) DO NOTHING",
            )
            .map_err(|error| {
                tracing::error!(error = %error, "publisher key seed prepare failed");
                AppError::Internal
            })?;
        let mut seeded = 0_u64;
        for entry in seed_keys.entries {
            let hash = keys::key_hash(&entry.secret);
            let changed = statement
                .execute((&entry.client_id.0, &entry.org.0, hash))
                .map_err(|error| {
                    tracing::error!(error = %error, "publisher key seed failed");
                    AppError::Internal
                })?;
            seeded = seeded.saturating_add(u64::try_from(changed).unwrap_or(u64::MAX));
        }
        drop(statement);
        if seeded > 0 {
            let audit = MutationAudit::maintenance()?;
            let event = AuditEvent {
                operation: "key.seed".to_owned(),
                target_type: "key_set".to_owned(),
                target_id: String::new(),
                result: "success".to_owned(),
                classification: "bootstrap_key_seed".to_owned(),
                revision: None,
            };
            crate::security::audit::append_in_transaction(
                &tx,
                &audit_key,
                &audit.event_id()?,
                audit.context(),
                &event,
            )?;
        }
        tx.commit().map_err(|_| AppError::Internal)?;
        Ok(seeded)
    })
    .await
}

async fn effective_pragmas(pool: &DbPool) -> Result<(i64, EffectivePragmas), AppError> {
    db::interact(pool, |conn| {
        db::verify_pragmas(conn)?;
        let schema_version = migrations::current_version(conn).map_err(|error| {
            tracing::error!(error = %error, "schema version read failed");
            AppError::Unavailable("database schema version unavailable".to_owned())
        })?;
        Ok((
            schema_version,
            EffectivePragmas {
                journal_mode: conn
                    .query_row("PRAGMA journal_mode", [], |row| row.get(0))
                    .map_err(pragma_read_error)?,
                synchronous: pragma_integer(conn, "PRAGMA synchronous")?,
                busy_timeout: pragma_integer(conn, "PRAGMA busy_timeout")?,
                wal_autocheckpoint: pragma_integer(conn, "PRAGMA wal_autocheckpoint")?,
                foreign_keys: pragma_integer(conn, "PRAGMA foreign_keys")?,
                page_size: pragma_integer(conn, "PRAGMA page_size")?,
            },
        ))
    })
    .await
}

fn pragma_integer(conn: &rusqlite::Connection, sql: &str) -> Result<i64, AppError> {
    conn.query_row(sql, [], |row| row.get(0))
        .map_err(pragma_read_error)
}

fn pragma_read_error(error: rusqlite::Error) -> AppError {
    tracing::error!(error = %error, "effective pragma read failed");
    AppError::Unavailable("database pragmas unavailable".to_owned())
}
