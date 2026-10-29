use std::sync::Arc;

use rusqlite::{OptionalExtension as _, TransactionBehavior};

use crate::config::{OsRandom, RandomSource};
use crate::error::AppError;
use crate::model::{
    ClientId, CreatePublisherKey, CreatedPublisherKey, KeyOwnerUpdate, OrgId, OwnerBackfillResult,
    PublisherKeySummary, UpdatePublisherKey, UpdatedPublisherKey,
};
use crate::persistence::db::{self, DbPool};
use crate::security::audit::{AuditEvent, MutationAudit};

use super::operations::{
    backfill_key_owner, backfill_key_owner_in_transaction, create_key, list_keys,
    normalize_verified_owner, revoke_key, seed_keys_from_env, set_key_owner, update_key,
    update_key_state,
};
use super::validation::database_failure;

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

    pub async fn create_key_audited(
        &self,
        request: CreatePublisherKey,
        audit: MutationAudit,
        audit_key: [u8; 32],
    ) -> Result<CreatedPublisherKey, AppError> {
        let random = Arc::clone(&self.random);
        db::interact(&self.pool, move |conn| {
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(|_| AppError::Internal)?;
            let created = create_key(&tx, &request, random.as_ref())?;
            let audit = audit.for_target_tenant(&created.org.0)?;
            let event = AuditEvent {
                operation: "key.create".to_owned(),
                target_type: "publisher_key".to_owned(),
                target_id: created.client_id.0.clone(),
                result: "success".to_owned(),
                classification: "credential_created".to_owned(),
                revision: None,
            };
            crate::security::audit::append_in_transaction(
                &tx,
                &audit_key,
                &audit.event_id()?,
                audit.context(),
                &event,
            )?;
            tx.commit().map_err(|_| AppError::Internal)?;
            Ok(created)
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

    pub async fn revoke_key_audited(
        &self,
        client_id: ClientId,
        audit: MutationAudit,
        audit_key: [u8; 32],
    ) -> Result<bool, AppError> {
        db::interact(&self.pool, move |conn| {
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(|_| AppError::Internal)?;
            let Some(org) = tx
                .query_row(
                    "SELECT org FROM api_keys WHERE client_id=?1 AND revoked_at IS NULL",
                    [&client_id.0],
                    |row| row.get::<_, String>(0),
                )
                .optional()
                .map_err(|error| database_failure("load key", &error))?
            else {
                tx.commit().map_err(|_| AppError::Internal)?;
                return Ok(false);
            };
            let changed = revoke_key(&tx, &client_id.0)?;
            if !changed {
                tx.commit().map_err(|_| AppError::Internal)?;
                return Ok(false);
            }
            let event = AuditEvent {
                operation: "key.revoke".to_owned(),
                target_type: "publisher_key".to_owned(),
                target_id: client_id.0.clone(),
                result: "success".to_owned(),
                classification: "credential_revoked".to_owned(),
                revision: None,
            };
            let audit = audit.for_target_tenant(&org)?;
            crate::security::audit::append_in_transaction(
                &tx,
                &audit_key,
                &audit.event_id()?,
                audit.context(),
                &event,
            )?;
            tx.commit().map_err(|_| AppError::Internal)?;
            Ok(true)
        })
        .await
    }

    pub async fn update_key(
        &self,
        client_id: ClientId,
        request: UpdatePublisherKey,
    ) -> Result<Option<UpdatedPublisherKey>, AppError> {
        db::interact(&self.pool, move |conn| {
            update_key(conn, &client_id.0, &request)
        })
        .await
    }

    pub async fn update_key_audited(
        &self,
        client_id: ClientId,
        request: UpdatePublisherKey,
        audit: MutationAudit,
        audit_key: [u8; 32],
    ) -> Result<Option<UpdatedPublisherKey>, AppError> {
        db::interact(&self.pool, move |conn| {
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(|_| AppError::Internal)?;
            let Some((updated, changed)) = update_key_state(&tx, &client_id.0, &request)? else {
                tx.commit().map_err(|_| AppError::Internal)?;
                return Ok(None);
            };
            if changed {
                let event = AuditEvent {
                    operation: "key.update".to_owned(),
                    target_type: "publisher_key".to_owned(),
                    target_id: client_id.0.clone(),
                    result: "success".to_owned(),
                    classification: "credential_metadata_updated".to_owned(),
                    revision: None,
                };
                let audit = audit.for_target_tenant(&updated.org.0)?;
                crate::security::audit::append_in_transaction(
                    &tx,
                    &audit_key,
                    &audit.event_id()?,
                    audit.context(),
                    &event,
                )?;
            }
            tx.commit().map_err(|_| AppError::Internal)?;
            Ok(Some(updated))
        })
        .await
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

    pub async fn set_key_owner_audited(
        &self,
        client_id: ClientId,
        owner_email: Option<String>,
        audit: MutationAudit,
        audit_key: [u8; 32],
    ) -> Result<Option<KeyOwnerUpdate>, AppError> {
        db::interact(&self.pool, move |conn| {
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(|_| AppError::Internal)?;
            let Some(org) = tx
                .query_row(
                    "SELECT org FROM api_keys WHERE client_id=?1",
                    [&client_id.0],
                    |row| row.get::<_, String>(0),
                )
                .optional()
                .map_err(|error| database_failure("load key owner", &error))?
            else {
                tx.commit().map_err(|_| AppError::Internal)?;
                return Ok(None);
            };
            let next = normalize_verified_owner(&tx, owner_email.as_deref(), &org)?;
            let current = tx
                .query_row(
                    "SELECT owner_email FROM api_keys WHERE client_id=?1",
                    [&client_id.0],
                    |row| row.get::<_, Option<String>>(0),
                )
                .map_err(|error| database_failure("load key owner", &error))?;
            let updated = KeyOwnerUpdate {
                client_id: client_id.clone(),
                org: OrgId(org),
                owner_email: next.clone(),
            };
            if current == next {
                tx.commit().map_err(|_| AppError::Internal)?;
                return Ok(Some(updated));
            }
            tx.execute(
                "UPDATE api_keys SET owner_email=?1 WHERE client_id=?2",
                rusqlite::params![next, client_id.0],
            )
            .map_err(|error| database_failure("set key owner", &error))?;
            let event = AuditEvent {
                operation: "key.owner.set".to_owned(),
                target_type: "publisher_key".to_owned(),
                target_id: client_id.0.clone(),
                result: "success".to_owned(),
                classification: "owner_assignment".to_owned(),
                revision: None,
            };
            let audit = audit.for_target_tenant(&updated.org.0)?;
            crate::security::audit::append_in_transaction(
                &tx,
                &audit_key,
                &audit.event_id()?,
                audit.context(),
                &event,
            )?;
            tx.commit().map_err(|_| AppError::Internal)?;
            Ok(Some(updated))
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

    pub async fn backfill_key_owner_audited(
        &self,
        client_id: ClientId,
        owner_email: String,
        confirm: bool,
        audit: MutationAudit,
        audit_key: [u8; 32],
    ) -> Result<Option<OwnerBackfillResult>, AppError> {
        if !confirm {
            return self.backfill_key_owner(client_id, owner_email, false).await;
        }
        db::interact(&self.pool, move |conn| {
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(|_| AppError::Internal)?;
            let result = backfill_key_owner_in_transaction(&tx, &client_id.0, &owner_email, true)?;
            let Some(result) = result else {
                tx.commit().map_err(|_| AppError::Internal)?;
                return Ok(None);
            };
            if result.updated == 0 {
                tx.commit().map_err(|_| AppError::Internal)?;
                return Ok(Some(result));
            }
            let audit = audit.for_target_tenant(&result.org.0)?;
            let event = AuditEvent {
                operation: "key.owner.backfill".to_owned(),
                target_type: "publisher_key".to_owned(),
                target_id: client_id.0.clone(),
                result: "success".to_owned(),
                classification: "owner_backfill_confirmed".to_owned(),
                revision: None,
            };
            crate::security::audit::append_in_transaction(
                &tx,
                &audit_key,
                &audit.event_id()?,
                audit.context(),
                &event,
            )?;
            tx.commit().map_err(|_| AppError::Internal)?;
            Ok(Some(result))
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
