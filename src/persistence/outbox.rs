//! Durable provider-delivery storage; this module never performs network I/O.
//!
//! A durability intent keeps a row `blocked` until the producer's filesystem work succeeds.
//! Worker restart is bounded at-least-once: an expired lease is retryable and records duplicate
//! risk instead of silently claiming an outcome.

use crate::error::AppError;
use crate::persistence::db::{self, DbPool};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use sha2::{Digest, Sha256};
use std::sync::Arc;

pub const MAX_PAYLOAD_BYTES: usize = 32 * 1024;
pub const MAX_QUEUE_GLOBAL: i64 = 10_000;
pub const MAX_QUEUE_TENANT: i64 = 1_000;
pub const LEASE_MILLIS: i64 = 30_000;
pub const MAX_ATTEMPTS: i64 = 8;
pub const MAX_TARGET_KEY_BYTES: usize = 160;
pub const MAX_SECRET_REF_BYTES: usize = 128;
pub const MAX_BUCKET_KEY_BYTES: usize = 128;
pub const MAX_ORDERING_KEY_BYTES: usize = 160;
pub const MAX_DEPENDENCY_ID_BYTES: usize = 128;

pub const DELIVERY_KIND_EVENT: &str = "event";
pub const DELIVERY_KIND_DISCUSSION_THREAD: &str = "discussion_thread";
pub const DELIVERY_KIND_DISCUSSION_MESSAGE: &str = "discussion_message";
pub const DELIVERY_KIND_DISCUSSION_TOMBSTONE: &str = "discussion_tombstone";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EnqueueDelivery {
    pub event_id: String,
    pub tenant: String,
    pub event_type: String,
    pub target_key: String,
    pub secret_ref: String,
    pub payload: Vec<u8>,
    pub payload_sha256: Option<String>,
    pub durability_intent_id: Option<String>,
    /// `event` preserves PBI-056 webhook behavior; discussion values are PBI-079 only.
    pub delivery_kind: String,
    /// Independent FIFO identity. Legacy event rows use their target key.
    pub ordering_key: String,
    /// A root discussion job has no predecessor; later jobs wait for its accepted outcome.
    pub depends_on_outbox_id: Option<String>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeliveryRecord {
    pub id: String,
    pub event_id: String,
    pub tenant: String,
    pub event_type: String,
    pub target_key: String,
    pub bucket_id: String,
    pub secret_ref: String,
    pub payload: Vec<u8>,
    pub payload_sha256: String,
    pub durability_intent_id: Option<String>,
    pub delivery_kind: String,
    pub ordering_key: String,
    pub depends_on_outbox_id: Option<String>,
    pub state: String,
    pub attempts: i64,
    pub next_attempt_at: i64,
    pub lease_owner: Option<String>,
    pub lease_expires_at: Option<i64>,
    pub lease_token: Option<String>,
    pub lease_version: i64,
    pub result_classification: String,
    pub duplicate_risk: bool,
    pub discord_message_id: Option<String>,
    pub terminal_error: String,
    pub created_at: i64,
    pub updated_at: i64,
    pub completed_at: Option<i64>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RetryTransition {
    pub next_attempt_at: i64,
    pub classification: String,
    pub duplicate_risk: bool,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeadLetterTransition {
    pub classification: String,
    pub error: String,
    pub duplicate_risk: bool,
}

/// Aggregate delivery state for operator telemetry. This intentionally contains no tenant,
/// webhook, event, payload, or provider-response data.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct QueueStatus {
    pub active: u64,
    pub ready: u64,
    pub retrying: u64,
    pub dead_letter: u64,
    pub attempts: u64,
    /// Rows whose latest result could represent a duplicate Discord post.
    pub ambiguous: u64,
    pub oldest_active_age_millis: u64,
    pub rate_limited_global: u64,
    pub rate_limited_target: u64,
    pub rate_limited_bucket: u64,
    pub max_rate_limit_delay_millis: u64,
    /// Discussion mirrors by fixed, privacy-safe lifecycle state.
    pub discussion_connected: u64,
    pub discussion_pending: u64,
    pub discussion_paused: u64,
    pub discussion_failed: u64,
    pub discussion_local_only: u64,
    pub discussion_pending_threads: u64,
    pub discussion_oldest_pending_thread_age_millis: u64,
    pub discussion_terminal_failures: u64,
}

struct QueueStatusRow {
    active: i64,
    ready: i64,
    retrying: i64,
    dead_letter: i64,
    attempts: i64,
    ambiguous: i64,
    oldest: Option<i64>,
    rate_limited_global: i64,
    rate_limited_target: i64,
    rate_limited_bucket: i64,
    max_rate_limit_delay_millis: i64,
    discussion_connected: i64,
    discussion_pending: i64,
    discussion_paused: i64,
    discussion_failed: i64,
    discussion_local_only: i64,
    discussion_pending_threads: i64,
    discussion_oldest_pending_thread_created_at: Option<i64>,
    discussion_terminal_failures: i64,
}
pub trait OutboxClock: Send + Sync {
    fn now_millis(&self) -> i64;
}
pub trait OutboxIdGenerator: Send + Sync {
    fn next_id(&self) -> String;
}
pub struct SystemOutboxClock;
impl OutboxClock for SystemOutboxClock {
    fn now_millis(&self) -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
    }
}
pub struct RandomOutboxId;
impl OutboxIdGenerator for RandomOutboxId {
    fn next_id(&self) -> String {
        nanoid::nanoid!()
    }
}

pub struct OutboxRepository {
    pool: DbPool,
    clock: Arc<dyn OutboxClock>,
    ids: Arc<dyn OutboxIdGenerator>,
}
impl OutboxRepository {
    #[must_use]
    pub fn new(pool: DbPool) -> Self {
        Self::with_clock_and_ids(pool, Arc::new(SystemOutboxClock), Arc::new(RandomOutboxId))
    }
    #[must_use]
    pub fn with_clock_and_ids(
        pool: DbPool,
        clock: Arc<dyn OutboxClock>,
        ids: Arc<dyn OutboxIdGenerator>,
    ) -> Self {
        Self { pool, clock, ids }
    }
    pub async fn enqueue(&self, input: EnqueueDelivery) -> Result<DeliveryRecord, AppError> {
        let now = self.clock.now_millis();
        let id = self.ids.next_id();
        let pool = self.pool.clone();
        db::interact(&pool, move |conn| {
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(internal)?;
            let mut rows = enqueue_many_in_transaction(&tx, &[(input, id)], now)?;
            tx.commit().map_err(internal)?;
            Ok(rows.remove(0))
        })
        .await
    }
    pub async fn claim_next(&self, worker: String) -> Result<Option<DeliveryRecord>, AppError> {
        let now = self.clock.now_millis();
        let token = self.ids.next_id();
        let pool = self.pool.clone();
        db::interact(&pool, move |conn| claim_next(conn, &worker, &token, now)).await
    }
    pub async fn accepted(
        &self,
        id: String,
        worker: String,
        token: String,
        lease_version: i64,
        message: String,
    ) -> Result<bool, AppError> {
        let now = self.clock.now_millis();
        let pool = self.pool.clone();
        db::interact(&pool, move |c| {
            accepted(c, &id, &worker, &token, lease_version, &message, now)
        })
        .await
    }
    pub async fn retry(
        &self,
        id: String,
        worker: String,
        token: String,
        lease_version: i64,
        update: RetryTransition,
    ) -> Result<bool, AppError> {
        let now = self.clock.now_millis();
        let pool = self.pool.clone();
        db::interact(&pool, move |c| {
            retry(c, &id, &worker, &token, lease_version, update, now)
        })
        .await
    }
    pub async fn dead_letter(
        &self,
        id: String,
        worker: String,
        token: String,
        lease_version: i64,
        update: DeadLetterTransition,
    ) -> Result<bool, AppError> {
        let now = self.clock.now_millis();
        let pool = self.pool.clone();
        db::interact(&pool, move |c| {
            dead_letter(c, &id, &worker, &token, lease_version, update, now)
        })
        .await
    }
    pub async fn persist_rate_limit(
        &self,
        scope: String,
        target: String,
        bucket: String,
        secret: String,
        blocked_until: i64,
    ) -> Result<(), AppError> {
        let now = self.clock.now_millis();
        let pool = self.pool.clone();
        db::interact(&pool, move |conn| {
            persist_rate_limit(conn, &scope, &target, &bucket, &secret, blocked_until, now)
        })
        .await
    }

    /// Read low-cardinality queue health without claiming or mutating a delivery row.
    pub async fn status(&self) -> Result<QueueStatus, AppError> {
        let now = self.clock.now_millis();
        let pool = self.pool.clone();
        db::interact(&pool, move |conn| queue_status(conn, now)).await
    }
}

pub fn queue_status(conn: &Connection, now: i64) -> Result<QueueStatus, AppError> {
    let row = conn
        .query_row(
            "SELECT \
                COALESCE(SUM(CASE WHEN state IN ('blocked', 'ready', 'leased', 'retry') THEN 1 ELSE 0 END), 0), \
                COALESCE(SUM(CASE WHEN state = 'ready' THEN 1 ELSE 0 END), 0), \
                COALESCE(SUM(CASE WHEN state = 'retry' THEN 1 ELSE 0 END), 0), \
                COALESCE(SUM(CASE WHEN state = 'dead_letter' THEN 1 ELSE 0 END), 0), \
                COALESCE(SUM(attempts), 0), \
                COALESCE(SUM(CASE WHEN duplicate_risk = 1 THEN 1 ELSE 0 END), 0), \
                MIN(CASE WHEN state IN ('blocked', 'ready', 'leased', 'retry') THEN created_at END), \
                (SELECT COUNT(*) FROM provider_delivery_rate_limits WHERE scope = 'global' AND blocked_until > ?1), \
                (SELECT COUNT(*) FROM provider_delivery_rate_limits WHERE scope = 'target' AND blocked_until > ?1), \
                (SELECT COUNT(*) FROM provider_delivery_rate_limits WHERE scope = 'bucket' AND blocked_until > ?1), \
                COALESCE((SELECT MAX(blocked_until - ?1) FROM provider_delivery_rate_limits WHERE blocked_until > ?1), 0), \
                (SELECT COUNT(*) FROM artifact_discussions WHERE mode = 'discord_mirror' AND state = 'connected'), \
                (SELECT COUNT(*) FROM artifact_discussions WHERE mode = 'discord_mirror' AND state = 'pending'), \
                (SELECT COUNT(*) FROM artifact_discussions WHERE mode = 'artifact_only' AND state = 'paused'), \
                (SELECT COUNT(*) FROM artifact_discussions WHERE state = 'failed'), \
                ((SELECT COUNT(*) FROM artifact_discussions WHERE mode = 'artifact_only' AND state = 'local') + \
                 (SELECT COUNT(*) FROM artifacts AS a WHERE NOT EXISTS (SELECT 1 FROM artifact_discussions AS d WHERE d.artifact_id = a.id AND d.org = a.org))), \
                (SELECT COUNT(*) FROM provider_delivery_outbox WHERE delivery_kind = 'discussion_thread' AND state IN ('blocked', 'ready', 'leased', 'retry')), \
                (SELECT MIN(created_at) FROM provider_delivery_outbox WHERE delivery_kind = 'discussion_thread' AND state IN ('blocked', 'ready', 'leased', 'retry')), \
                (SELECT COUNT(*) FROM provider_delivery_outbox WHERE delivery_kind IN ('discussion_thread', 'discussion_message', 'discussion_tombstone') AND state = 'dead_letter') \
             FROM provider_delivery_outbox",
            [now],
            |row| {
                Ok(QueueStatusRow {
                    active: row.get(0)?,
                    ready: row.get(1)?,
                    retrying: row.get(2)?,
                    dead_letter: row.get(3)?,
                    attempts: row.get(4)?,
                    ambiguous: row.get(5)?,
                    oldest: row.get(6)?,
                    rate_limited_global: row.get(7)?,
                    rate_limited_target: row.get(8)?,
                    rate_limited_bucket: row.get(9)?,
                    max_rate_limit_delay_millis: row.get(10)?,
                    discussion_connected: row.get(11)?,
                    discussion_pending: row.get(12)?,
                    discussion_paused: row.get(13)?,
                    discussion_failed: row.get(14)?,
                    discussion_local_only: row.get(15)?,
                    discussion_pending_threads: row.get(16)?,
                    discussion_oldest_pending_thread_created_at: row.get(17)?,
                    discussion_terminal_failures: row.get(18)?,
                })
            },
        )
        .map_err(internal)?;
    Ok(QueueStatus {
        active: u64::try_from(row.active).unwrap_or_default(),
        ready: u64::try_from(row.ready).unwrap_or_default(),
        retrying: u64::try_from(row.retrying).unwrap_or_default(),
        dead_letter: u64::try_from(row.dead_letter).unwrap_or_default(),
        attempts: u64::try_from(row.attempts).unwrap_or_default(),
        ambiguous: u64::try_from(row.ambiguous).unwrap_or_default(),
        oldest_active_age_millis: row
            .oldest
            .map(|created| u64::try_from(now.saturating_sub(created)).unwrap_or_default())
            .unwrap_or_default(),
        rate_limited_global: u64::try_from(row.rate_limited_global).unwrap_or_default(),
        rate_limited_target: u64::try_from(row.rate_limited_target).unwrap_or_default(),
        rate_limited_bucket: u64::try_from(row.rate_limited_bucket).unwrap_or_default(),
        max_rate_limit_delay_millis: u64::try_from(row.max_rate_limit_delay_millis)
            .unwrap_or_default(),
        discussion_connected: u64::try_from(row.discussion_connected).unwrap_or_default(),
        discussion_pending: u64::try_from(row.discussion_pending).unwrap_or_default(),
        discussion_paused: u64::try_from(row.discussion_paused).unwrap_or_default(),
        discussion_failed: u64::try_from(row.discussion_failed).unwrap_or_default(),
        discussion_local_only: u64::try_from(row.discussion_local_only).unwrap_or_default(),
        discussion_pending_threads: u64::try_from(row.discussion_pending_threads)
            .unwrap_or_default(),
        discussion_oldest_pending_thread_age_millis: row
            .discussion_oldest_pending_thread_created_at
            .map(|created| u64::try_from(now.saturating_sub(created)).unwrap_or_default())
            .unwrap_or_default(),
        discussion_terminal_failures: u64::try_from(row.discussion_terminal_failures)
            .unwrap_or_default(),
    })
}

/// Atomically creates all new fan-out rows with a single global/tenant capacity decision.
pub fn enqueue_many_in_transaction(
    tx: &Transaction<'_>,
    inputs: &[(EnqueueDelivery, String)],
    now: i64,
) -> Result<Vec<DeliveryRecord>, AppError> {
    for (input, _) in inputs {
        validate(input)?;
        if let Some(supplied) = &input.payload_sha256
            && supplied != &hash(&input.payload)
        {
            return Err(AppError::Validation(
                "payload hash does not match payload".into(),
            ));
        }
    }
    let mut records = Vec::with_capacity(inputs.len());
    let mut fresh = Vec::new();
    for (input, id) in inputs {
        if let Some(row) = find_event(tx, input)? {
            if row.payload_sha256 != hash(&input.payload)
                || row.event_type != input.event_type
                || row.secret_ref != input.secret_ref
                || row.durability_intent_id != input.durability_intent_id
                || row.delivery_kind != input.delivery_kind
                || row.ordering_key != input.ordering_key
                || row.depends_on_outbox_id != input.depends_on_outbox_id
            {
                return Err(AppError::Conflict("outbox idempotency conflict".into()));
            }
            records.push(row);
        } else {
            fresh.push((input, id));
        }
    }
    let active: i64 = tx.query_row("SELECT COUNT(*) FROM provider_delivery_outbox WHERE state IN ('blocked', 'ready', 'leased', 'retry')", [], |r| r.get(0)).map_err(internal)?;
    if active + i64::try_from(fresh.len()).unwrap_or(i64::MAX) > MAX_QUEUE_GLOBAL {
        return Err(AppError::RateLimited);
    }
    for tenant in fresh
        .iter()
        .map(|(i, _)| &i.tenant)
        .collect::<std::collections::BTreeSet<_>>()
    {
        let count: i64 = tx.query_row("SELECT COUNT(*) FROM provider_delivery_outbox WHERE tenant = ?1 AND state IN ('blocked', 'ready', 'leased', 'retry')", [tenant], |r| r.get(0)).map_err(internal)?;
        let additions = fresh.iter().filter(|(i, _)| &i.tenant == tenant).count();
        if count + i64::try_from(additions).unwrap_or(i64::MAX) > MAX_QUEUE_TENANT {
            return Err(AppError::RateLimited);
        }
    }
    for (input, id) in fresh {
        validate_dependency_contract(tx, input)?;
        let state = if input.durability_intent_id.is_some() {
            "blocked"
        } else {
            "ready"
        };
        let prior_created_at: Option<i64> = tx
            .query_row(
                "SELECT MAX(created_at) FROM provider_delivery_outbox WHERE ordering_key = ?1",
                [&input.ordering_key],
                |row| row.get(0),
            )
            .map_err(internal)?;
        let created_at = prior_created_at.map_or(Ok(now), |prior| {
            prior
                .checked_add(1)
                .map(|next| now.max(next))
                .ok_or(AppError::Internal)
        })?;
        let digest = hash(&input.payload);
        tx.execute("INSERT INTO provider_delivery_outbox (id, provider, event_id, tenant, event_type, target_key, bucket_id, secret_ref, payload, payload_sha256, durability_intent_id, delivery_kind, ordering_key, depends_on_outbox_id, state, next_attempt_at, created_at, updated_at) VALUES (?1, 'discord', ?2, ?3, ?4, ?5, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?14)", params![id, input.event_id, input.tenant, input.event_type, input.target_key, input.secret_ref, input.payload, digest, input.durability_intent_id, input.delivery_kind, input.ordering_key, input.depends_on_outbox_id, state, now, created_at]).map_err(internal)?;
        records.push(get(tx, id)?.ok_or(AppError::Internal)?);
    }
    Ok(records)
}
pub fn enqueue_in_transaction(
    tx: &Transaction<'_>,
    input: &EnqueueDelivery,
    id: &str,
    now: i64,
) -> Result<DeliveryRecord, AppError> {
    enqueue_many_in_transaction(tx, &[(input.clone(), id.to_owned())], now)
        .map(|mut rows| rows.remove(0))
}

/// Producer success: release every matching blocked row, clear its FK, then delete the intent.
pub fn finalize_durability_success_in_transaction(
    tx: &Transaction<'_>,
    intent: &str,
    now: i64,
) -> Result<usize, AppError> {
    let changed = tx.execute("UPDATE provider_delivery_outbox SET state = 'ready', durability_intent_id = NULL, updated_at = ?1 WHERE durability_intent_id = ?2 AND state = 'blocked'", params![now, intent]).map_err(internal)?;
    if tx
        .execute(
            "DELETE FROM artifact_durability_intents WHERE id = ?1",
            [intent],
        )
        .map_err(internal)?
        != 1
    {
        return Err(AppError::Internal);
    }
    Ok(changed)
}
/// Producer compensation: remove blocked outbox work first so the restricted intent FK stays safe.
pub fn compensate_durability_in_transaction(
    tx: &Transaction<'_>,
    intent: &str,
) -> Result<usize, AppError> {
    let changed = tx.execute("DELETE FROM provider_delivery_outbox WHERE durability_intent_id = ?1 AND state = 'blocked'", [intent]).map_err(internal)?;
    if tx
        .execute(
            "DELETE FROM artifact_durability_intents WHERE id = ?1",
            [intent],
        )
        .map_err(internal)?
        != 1
    {
        return Err(AppError::Internal);
    }
    Ok(changed)
}

pub fn claim_next(
    conn: &mut Connection,
    worker: &str,
    token: &str,
    now: i64,
) -> Result<Option<DeliveryRecord>, AppError> {
    if worker.trim().is_empty() || token.trim().is_empty() {
        return Err(AppError::Validation(
            "worker and lease token are required".into(),
        ));
    }

    // Most polling turns find no work.  Do that check outside a write transaction: a
    // `BEGIN IMMEDIATE` poll would otherwise briefly take SQLite's sole writer slot even when
    // the queue is empty. That creates needless contention with lifecycle and audited mutation
    // transactions. A false negative is harmless (the next poll or wake will claim
    // newly-enqueued work); a positive result is rechecked under the immediate transaction below
    // before a lease is granted.
    if !has_claimable_work(conn, now)? {
        return Ok(None);
    }

    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(internal)?;
    tx.execute("UPDATE provider_delivery_outbox SET state = CASE WHEN attempts >= ?1 THEN 'dead_letter' ELSE 'retry' END, lease_owner = NULL, lease_expires_at = NULL, lease_token = NULL, result_classification = CASE WHEN attempts >= ?1 THEN 'attempts_exhausted_after_worker_restart' ELSE 'ambiguous_worker_restart' END, duplicate_risk = 1, terminal_error = CASE WHEN attempts >= ?1 THEN 'delivery attempts exhausted after worker restart' ELSE '' END, next_attempt_at = ?2, updated_at = ?2, completed_at = CASE WHEN attempts >= ?1 THEN ?2 ELSE NULL END WHERE state = 'leased' AND lease_expires_at <= ?2", params![MAX_ATTEMPTS, now]).map_err(internal)?;
    propagate_terminal_dependencies(&tx, now)?;
    let id: Option<String> = tx.query_row("SELECT o.id FROM provider_delivery_outbox o WHERE o.state IN ('ready', 'retry') AND o.durability_intent_id IS NULL AND o.attempts < ?1 AND o.next_attempt_at <= ?2 AND (o.depends_on_outbox_id IS NULL OR EXISTS (SELECT 1 FROM provider_delivery_outbox predecessor WHERE predecessor.id = o.depends_on_outbox_id AND predecessor.state = 'accepted')) AND NOT EXISTS (SELECT 1 FROM provider_delivery_rate_limits r WHERE r.provider = o.provider AND r.blocked_until > ?2 AND (r.scope = 'global' OR (r.scope = 'target' AND r.target_key = o.target_key) OR (r.scope = 'bucket' AND r.bucket_id = o.bucket_id AND r.top_level_secret_ref = o.secret_ref))) AND NOT EXISTS (SELECT 1 FROM provider_delivery_outbox earlier WHERE earlier.ordering_key = o.ordering_key AND earlier.state IN ('blocked', 'ready', 'leased', 'retry') AND (earlier.created_at < o.created_at OR (earlier.created_at = o.created_at AND earlier.id < o.id))) ORDER BY o.created_at ASC, o.id ASC LIMIT 1", params![MAX_ATTEMPTS, now], |r| r.get(0)).optional().map_err(internal)?;
    let Some(id) = id else {
        tx.commit().map_err(internal)?;
        return Ok(None);
    };
    if tx.execute("UPDATE provider_delivery_outbox SET state = 'leased', attempts = attempts + 1, lease_owner = ?1, lease_expires_at = ?2, lease_token = ?3, lease_version = lease_version + 1, updated_at = ?4 WHERE id = ?5 AND state IN ('ready', 'retry') AND durability_intent_id IS NULL", params![worker, now + LEASE_MILLIS, token, now, id]).map_err(internal)? != 1 { return Err(AppError::Internal); }
    let row = get(&tx, &id)?.ok_or(AppError::Internal)?;
    tx.commit().map_err(internal)?;
    Ok(Some(row))
}

/// Returns whether a claim turn needs SQLite's writer slot.
///
/// Expired leases are included because their recovery transition is itself durable work even
/// when it will dead-letter rather than make a row available to this worker.  This deliberately
/// uses no explicit transaction, so the read snapshot is released before `claim_next` begins its
/// immediate transaction.
fn has_claimable_work(conn: &Connection, now: i64) -> Result<bool, AppError> {
    conn.query_row(
        "SELECT EXISTS(
             SELECT 1
               FROM provider_delivery_outbox
              WHERE state = 'leased'
                AND lease_expires_at <= ?1
             UNION ALL
             SELECT 1
               FROM provider_delivery_outbox o
              WHERE o.state IN ('ready', 'retry')
                AND o.durability_intent_id IS NULL
                AND o.attempts < ?2
                AND o.next_attempt_at <= ?1
                AND (
                    o.depends_on_outbox_id IS NULL
                    OR EXISTS (
                        SELECT 1
                          FROM provider_delivery_outbox predecessor
                         WHERE predecessor.id = o.depends_on_outbox_id
                           AND predecessor.state = 'accepted'
                    )
                )
                AND NOT EXISTS (
                    SELECT 1
                      FROM provider_delivery_rate_limits r
                     WHERE r.provider = o.provider
                       AND r.blocked_until > ?1
                       AND (
                           r.scope = 'global'
                           OR (r.scope = 'target' AND r.target_key = o.target_key)
                           OR (
                               r.scope = 'bucket'
                               AND r.bucket_id = o.bucket_id
                               AND r.top_level_secret_ref = o.secret_ref
                           )
                       )
                )
                AND NOT EXISTS (
                    SELECT 1
                      FROM provider_delivery_outbox earlier
                      WHERE earlier.ordering_key = o.ordering_key
                       AND earlier.state IN ('blocked', 'ready', 'leased', 'retry')
                       AND (
                           earlier.created_at < o.created_at
                           OR (earlier.created_at = o.created_at AND earlier.id < o.id)
                       )
                )
             UNION ALL
             SELECT 1
               FROM provider_delivery_outbox dependent
               JOIN provider_delivery_outbox predecessor
                 ON predecessor.id = dependent.depends_on_outbox_id
              WHERE dependent.state IN ('ready', 'retry')
                AND dependent.durability_intent_id IS NULL
                AND predecessor.state = 'dead_letter'
         )",
        params![now, MAX_ATTEMPTS],
        |row| row.get::<_, bool>(0),
    )
    .map_err(internal)
}

/// A terminal predecessor is never silently bypassed.  Mark all currently runnable descendants
/// terminal in the same claim transaction; the loop also propagates a root failure through an
/// already-created chain.  Retrying an artifact discussion is a new generation, not a mutation
/// of an old dead-letter row.
fn propagate_terminal_dependencies(tx: &Transaction<'_>, now: i64) -> Result<(), AppError> {
    loop {
        let changed = tx
            .execute(
                "UPDATE provider_delivery_outbox
                    SET state = 'dead_letter',
                        lease_owner = NULL,
                        lease_expires_at = NULL,
                        lease_token = NULL,
                        next_attempt_at = ?1,
                        result_classification = 'dependency_failed',
                        duplicate_risk = 0,
                        terminal_error = 'dependency_failed',
                        updated_at = ?1,
                        completed_at = ?1
                  WHERE state IN ('ready', 'retry')
                    AND durability_intent_id IS NULL
                    AND EXISTS (
                        SELECT 1
                          FROM provider_delivery_outbox predecessor
                         WHERE predecessor.id = provider_delivery_outbox.depends_on_outbox_id
                           AND predecessor.state = 'dead_letter'
                    )",
                [now],
            )
            .map_err(internal)?;
        if changed == 0 {
            return Ok(());
        }
    }
}
pub fn accepted(
    conn: &Connection,
    id: &str,
    worker: &str,
    token: &str,
    lease_version: i64,
    message: &str,
    now: i64,
) -> Result<bool, AppError> {
    accepted_guarded(conn, id, worker, token, lease_version, message, now)
}

/// Guarded acceptance for a caller which already owns a larger transaction (for example,
/// accepting a discussion delivery while recording its Discord message IDs).
pub fn accepted_in_transaction(
    tx: &Transaction<'_>,
    id: &str,
    worker: &str,
    token: &str,
    lease_version: i64,
    message: &str,
    now: i64,
) -> Result<bool, AppError> {
    accepted_guarded(tx, id, worker, token, lease_version, message, now)
}

fn accepted_guarded(
    conn: &Connection,
    id: &str,
    worker: &str,
    token: &str,
    lease_version: i64,
    message: &str,
    now: i64,
) -> Result<bool, AppError> {
    transition(
        conn,
        id,
        worker,
        token,
        lease_version,
        Transition {
            state: "accepted",
            now,
            next_attempt_at: now,
            classification: "accepted",
            message,
            error: "",
            duplicate_risk: false,
            requires_exhaustion: false,
        },
    )
}
pub fn retry(
    conn: &Connection,
    id: &str,
    worker: &str,
    token: &str,
    lease_version: i64,
    update: RetryTransition,
    now: i64,
) -> Result<bool, AppError> {
    let classification = safe_retry_classification(&update.classification)?;
    Ok(conn
        .execute(
            "UPDATE provider_delivery_outbox
         SET state = CASE WHEN attempts >= ?1 THEN 'dead_letter' ELSE 'retry' END,
             lease_owner = NULL,
             lease_expires_at = NULL,
             lease_token = NULL,
             next_attempt_at = CASE WHEN attempts >= ?1 THEN ?2 ELSE ?3 END,
             result_classification = CASE WHEN attempts >= ?1 THEN 'attempts_exhausted' ELSE ?4 END,
             duplicate_risk = ?5,
             terminal_error = CASE WHEN attempts >= ?1 THEN 'attempts_exhausted' ELSE '' END,
             updated_at = ?2,
             completed_at = CASE WHEN attempts >= ?1 THEN ?2 ELSE NULL END
         WHERE id = ?6
           AND state = 'leased'
           AND lease_owner = ?7
           AND lease_token = ?8
           AND lease_version = ?9",
            params![
                MAX_ATTEMPTS,
                now,
                update.next_attempt_at,
                classification,
                i64::from(update.duplicate_risk),
                id,
                worker,
                token,
                lease_version
            ],
        )
        .map_err(internal)?
        == 1)
}
/// Persists global, target, or effective bucket throttling and bucket discovery atomically.
pub fn persist_rate_limit(
    conn: &mut Connection,
    scope: &str,
    target: &str,
    bucket: &str,
    secret: &str,
    blocked_until: i64,
    now: i64,
) -> Result<(), AppError> {
    let (stored_target, stored_bucket, stored_secret) = match scope {
        "global" if target.is_empty() && bucket.is_empty() && secret.is_empty() => ("", "", ""),
        "target" if opaque_target_key(target) && bucket.is_empty() && secret.is_empty() => {
            (target, "", "")
        }
        "bucket"
            if opaque_target_key(target)
                && opaque_bucket_key(bucket)
                && opaque_provider_secret_ref(secret) =>
        {
            ("", bucket, secret)
        }
        _ => return Err(AppError::Validation("invalid rate limit state".into())),
    };
    if blocked_until < 0 {
        return Err(AppError::Validation("invalid rate limit state".into()));
    }
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(internal)?;
    tx.execute("INSERT INTO provider_delivery_rate_limits (provider,scope,target_key,bucket_id,top_level_secret_ref,blocked_until,updated_at) VALUES ('discord',?1,?2,?3,?4,?5,?6) ON CONFLICT(provider,scope,target_key,bucket_id,top_level_secret_ref) DO UPDATE SET blocked_until=MAX(provider_delivery_rate_limits.blocked_until,excluded.blocked_until),updated_at=CASE WHEN excluded.blocked_until >= provider_delivery_rate_limits.blocked_until THEN excluded.updated_at ELSE provider_delivery_rate_limits.updated_at END", params![scope,stored_target,stored_bucket,stored_secret,blocked_until,now]).map_err(internal)?;
    if scope == "bucket" {
        tx.execute("UPDATE provider_delivery_outbox SET bucket_id=?1,updated_at=?2 WHERE target_key=?3 AND secret_ref=?4 AND state IN ('blocked','ready','retry')", params![bucket,now,target,secret]).map_err(internal)?;
    }
    tx.commit().map_err(internal)?;
    Ok(())
}
pub fn dead_letter(
    conn: &Connection,
    id: &str,
    worker: &str,
    token: &str,
    lease_version: i64,
    update: DeadLetterTransition,
    now: i64,
) -> Result<bool, AppError> {
    dead_letter_guarded(conn, id, worker, token, lease_version, update, now)
}

/// Guarded terminal transition for a caller which must update discussion mappings atomically.
pub fn dead_letter_in_transaction(
    tx: &Transaction<'_>,
    id: &str,
    worker: &str,
    token: &str,
    lease_version: i64,
    update: DeadLetterTransition,
    now: i64,
) -> Result<bool, AppError> {
    dead_letter_guarded(tx, id, worker, token, lease_version, update, now)
}

fn dead_letter_guarded(
    conn: &Connection,
    id: &str,
    worker: &str,
    token: &str,
    lease_version: i64,
    update: DeadLetterTransition,
    now: i64,
) -> Result<bool, AppError> {
    if update.duplicate_risk && update.classification != "attempts_exhausted" {
        return Err(AppError::Validation(
            "duplicate risk is valid only for exhausted delivery".into(),
        ));
    }
    transition(
        conn,
        id,
        worker,
        token,
        lease_version,
        Transition {
            state: "dead_letter",
            now,
            next_attempt_at: now,
            classification: &update.classification,
            message: "",
            error: &update.error,
            duplicate_risk: update.duplicate_risk,
            requires_exhaustion: update.classification.starts_with("attempts_exhausted"),
        },
    )
}
struct Transition<'a> {
    state: &'a str,
    now: i64,
    next_attempt_at: i64,
    classification: &'a str,
    message: &'a str,
    error: &'a str,
    duplicate_risk: bool,
    requires_exhaustion: bool,
}
fn transition(
    conn: &Connection,
    id: &str,
    worker: &str,
    token: &str,
    lease_version: i64,
    update: Transition<'_>,
) -> Result<bool, AppError> {
    let classification = match update.state {
        "accepted" if update.classification == "accepted" => update.classification,
        "dead_letter" => safe_terminal_classification(update.classification)?,
        _ => {
            return Err(AppError::Validation(
                "invalid outbox result classification".into(),
            ));
        }
    };
    Ok(conn.execute("UPDATE provider_delivery_outbox SET state = ?1, lease_owner = NULL, lease_expires_at = NULL, lease_token = NULL, next_attempt_at = ?2, result_classification = ?3, duplicate_risk = ?4, discord_message_id = CASE WHEN ?1 = 'accepted' THEN ?5 ELSE discord_message_id END, terminal_error = ?6, updated_at = ?7, completed_at = CASE WHEN ?1 IN ('accepted', 'dead_letter') THEN ?7 ELSE NULL END WHERE id = ?8 AND state = 'leased' AND lease_owner = ?9 AND lease_token = ?10 AND lease_version = ?11 AND (?12 = 0 OR attempts >= ?13)", params![update.state, update.next_attempt_at, classification, i64::from(update.duplicate_risk), update.message, redact_error(update.error), update.now, id, worker, token, lease_version, i64::from(update.requires_exhaustion), MAX_ATTEMPTS]).map_err(internal)? == 1)
}
pub fn verify_payload_hash(record: &DeliveryRecord) -> bool {
    hash(&record.payload) == record.payload_sha256
}
fn find_event(
    conn: &Connection,
    input: &EnqueueDelivery,
) -> Result<Option<DeliveryRecord>, AppError> {
    conn.query_row("SELECT id,event_id,tenant,event_type,target_key,bucket_id,secret_ref,payload,payload_sha256,durability_intent_id,delivery_kind,ordering_key,depends_on_outbox_id,state,attempts,next_attempt_at,lease_owner,lease_expires_at,lease_token,lease_version,result_classification,duplicate_risk,discord_message_id,terminal_error,created_at,updated_at,completed_at FROM provider_delivery_outbox WHERE provider='discord' AND tenant=?1 AND target_key=?2 AND event_id=?3", params![input.tenant,input.target_key,input.event_id], row).optional().map_err(internal)
}
fn get(conn: &Connection, id: &str) -> Result<Option<DeliveryRecord>, AppError> {
    conn.query_row("SELECT id,event_id,tenant,event_type,target_key,bucket_id,secret_ref,payload,payload_sha256,durability_intent_id,delivery_kind,ordering_key,depends_on_outbox_id,state,attempts,next_attempt_at,lease_owner,lease_expires_at,lease_token,lease_version,result_classification,duplicate_risk,discord_message_id,terminal_error,created_at,updated_at,completed_at FROM provider_delivery_outbox WHERE id=?1", [id], row).optional().map_err(internal)
}
fn row(r: &rusqlite::Row<'_>) -> rusqlite::Result<DeliveryRecord> {
    Ok(DeliveryRecord {
        id: r.get(0)?,
        event_id: r.get(1)?,
        tenant: r.get(2)?,
        event_type: r.get(3)?,
        target_key: r.get(4)?,
        bucket_id: r.get(5)?,
        secret_ref: r.get(6)?,
        payload: r.get(7)?,
        payload_sha256: r.get(8)?,
        durability_intent_id: r.get(9)?,
        delivery_kind: r.get(10)?,
        ordering_key: r.get(11)?,
        depends_on_outbox_id: r.get(12)?,
        state: r.get(13)?,
        attempts: r.get(14)?,
        next_attempt_at: r.get(15)?,
        lease_owner: r.get(16)?,
        lease_expires_at: r.get(17)?,
        lease_token: r.get(18)?,
        lease_version: r.get(19)?,
        result_classification: r.get(20)?,
        duplicate_risk: r.get::<_, i64>(21)? != 0,
        discord_message_id: r.get(22)?,
        terminal_error: r.get(23)?,
        created_at: r.get(24)?,
        updated_at: r.get(25)?,
        completed_at: r.get(26)?,
    })
}
fn hash(payload: &[u8]) -> String {
    hex::encode(Sha256::digest(payload))
}
fn validate(input: &EnqueueDelivery) -> Result<(), AppError> {
    if input.payload.len() > MAX_PAYLOAD_BYTES {
        return Err(AppError::PayloadTooLarge);
    }
    if [
        input.event_id.as_str(),
        input.tenant.as_str(),
        input.event_type.as_str(),
        input.target_key.as_str(),
        input.secret_ref.as_str(),
        input.delivery_kind.as_str(),
        input.ordering_key.as_str(),
    ]
    .iter()
    .any(|v| v.trim().is_empty() || v.contains('\0'))
        || !opaque_target_key(&input.target_key)
        || !opaque_ordering_key(&input.ordering_key)
    {
        return Err(AppError::Validation(
            "outbox identity fields are required".into(),
        ));
    }
    if input
        .depends_on_outbox_id
        .as_deref()
        .is_some_and(|id| !opaque_dependency_id(id))
    {
        return Err(AppError::Validation("invalid outbox dependency".into()));
    }
    match input.delivery_kind.as_str() {
        DELIVERY_KIND_EVENT
            if opaque_secret_ref(&input.secret_ref)
                && input.ordering_key == input.target_key
                && input.depends_on_outbox_id.is_none() => {}
        DELIVERY_KIND_DISCUSSION_THREAD if discussion_delivery_identity(input) => {}
        DELIVERY_KIND_DISCUSSION_MESSAGE | DELIVERY_KIND_DISCUSSION_TOMBSTONE
            if discussion_delivery_identity(input) && input.depends_on_outbox_id.is_some() => {}
        _ => {
            return Err(AppError::Validation(
                "invalid discussion delivery contract".into(),
            ));
        }
    }
    Ok(())
}

fn discussion_delivery_identity(input: &EnqueueDelivery) -> bool {
    !input.target_key.starts_with("discussion:")
        && opaque_target_key(&input.target_key)
        && input
            .secret_ref
            .strip_prefix("discussion:")
            .is_some_and(|connection_id| connection_id == input.target_key)
        && opaque_discussion_ref(&input.secret_ref)
        && input.ordering_key.starts_with("discussion:")
}

/// A root may wait on the exact publication notification selected by a notification-thread
/// connection. Replies and tombstones may wait only on their exact generation's root-create job.
/// These two shapes prevent cross-tenant dependencies, arbitrary chains, and threads racing ahead
/// of the notification message they attach to.
fn validate_dependency_contract(
    conn: &Connection,
    input: &EnqueueDelivery,
) -> Result<(), AppError> {
    let Some(dependency_id) = input.depends_on_outbox_id.as_deref() else {
        return Ok(());
    };
    if input.delivery_kind == DELIVERY_KIND_DISCUSSION_THREAD {
        let valid: bool = conn
            .query_row(
                "SELECT EXISTS(\
                   SELECT 1 \
                     FROM provider_delivery_outbox o \
                     JOIN org_discord_discussion_connections c \
                       ON c.notification_webhook_id = o.target_key \
                    WHERE o.id = ?1 \
                      AND o.provider = 'discord' \
                      AND o.delivery_kind = 'event' \
                      AND o.event_type = 'published' \
                      AND o.tenant = ?2 \
                      AND c.id = ?3 \
                      AND c.org = ?2 \
                      AND c.strategy = 'notification_thread'\
                 )",
                params![dependency_id, input.tenant, input.target_key],
                |row| row.get(0),
            )
            .map_err(internal)?;
        return if valid {
            Ok(())
        } else {
            Err(AppError::Validation(
                "invalid discussion delivery dependency".into(),
            ))
        };
    }
    let predecessor: Option<(String, String, String, String)> = conn
        .query_row(
            "SELECT tenant, target_key, secret_ref, ordering_key
               FROM provider_delivery_outbox
              WHERE id = ?1
                AND delivery_kind = 'discussion_thread'",
            [dependency_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .optional()
        .map_err(internal)?;
    let Some((tenant, target, secret, ordering)) = predecessor else {
        return Err(AppError::Validation(
            "invalid discussion delivery dependency".into(),
        ));
    };
    if tenant != input.tenant
        || target != input.target_key
        || secret != input.secret_ref
        || ordering != input.ordering_key
    {
        return Err(AppError::Validation(
            "invalid discussion delivery dependency".into(),
        ));
    }
    Ok(())
}
fn opaque_discussion_ref(value: &str) -> bool {
    value.strip_prefix("discussion:").is_some_and(|id| {
        !id.is_empty()
            && id.len() <= MAX_SECRET_REF_BYTES.saturating_sub("discussion:".len())
            && id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
    })
}
/// Rate-limit state is keyed by a non-secret provider reference. Event enqueue remains stricter:
/// it accepts only `webhook:` references, while a PBI-079 discussion delivery uses its immutable
/// `discussion:<connection-id>` reference to retain bucket throttling across attempts.
fn opaque_provider_secret_ref(value: &str) -> bool {
    opaque_secret_ref(value) || opaque_discussion_ref(value)
}
fn redact_error(error: &str) -> String {
    // Provider response bodies are untrusted and can contain webhook credentials.  Persist only
    // a closed set of worker-produced codes; all transport text, including token-like strings,
    // becomes the same safe operator-facing fallback.
    match error {
        "invalid_webhook"
        | "unknown_webhook"
        | "invalid_secret"
        | "decrypt_failed"
        | "allowlist_rejected"
        | "redirect"
        | "bad_request"
        | "unauthorized"
        | "forbidden"
        | "not_found"
        | "invalid_rate_limit_delay"
        | "client_error"
        | "contract_error"
        | "payload_hash_mismatch"
        | "permission_denied"
        | "validation_failed"
        | "provider_unavailable"
        | "attempts_exhausted"
        | "dependency_failed" => error.to_owned(),
        _ => "provider delivery failed".into(),
    }
}
fn safe_retry_classification(value: &str) -> Result<&str, AppError> {
    if matches!(
        value,
        "retry"
            | "network_retry"
            | "ambiguous_worker_restart"
            | "provider_unavailable"
            | "rate_limited"
            | "network"
            | "timeout"
            | "ambiguous"
            | "server_error"
    ) {
        Ok(value)
    } else {
        Err(AppError::Validation(
            "invalid outbox result classification".into(),
        ))
    }
}
fn safe_terminal_classification(value: &str) -> Result<&str, AppError> {
    if matches!(
        value,
        "attempts_exhausted"
            | "attempts_exhausted_after_worker_restart"
            | "invalid_webhook"
            | "permission_denied"
            | "validation_failed"
            | "provider_unavailable"
            | "dead_letter"
            | "server_error"
            | "invalid_secret"
            | "decrypt_failed"
            | "allowlist_rejected"
            | "redirect"
            | "bad_request"
            | "unauthorized"
            | "forbidden"
            | "not_found"
            | "invalid_rate_limit_delay"
            | "client_error"
            | "contract_error"
            | "payload_hash_mismatch"
            | "unknown_webhook"
            | "dependency_failed"
    ) {
        Ok(value)
    } else {
        Err(AppError::Validation(
            "invalid outbox result classification".into(),
        ))
    }
}
fn opaque_target_key(value: &str) -> bool {
    value.len() <= MAX_TARGET_KEY_BYTES
        && !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
}
fn opaque_secret_ref(value: &str) -> bool {
    let Some(identifier) = value.strip_prefix("webhook:") else {
        return false;
    };
    value.len() <= MAX_SECRET_REF_BYTES
        && !identifier.is_empty()
        && identifier
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
}
fn opaque_bucket_key(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_BUCKET_KEY_BYTES
        && !value.contains(['\0', '\r', '\n'])
        && !value.to_ascii_lowercase().starts_with("http://")
        && !value.to_ascii_lowercase().starts_with("https://")
        && !value.to_ascii_lowercase().contains("/api/webhooks/")
}
fn opaque_ordering_key(value: &str) -> bool {
    value.len() <= MAX_ORDERING_KEY_BYTES
        && !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.' | b':'))
}
fn opaque_dependency_id(value: &str) -> bool {
    value.len() <= MAX_DEPENDENCY_ID_BYTES
        && !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
}
fn internal(error: rusqlite::Error) -> AppError {
    tracing::error!(error = %error, "outbox database operation failed");
    AppError::Internal
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queue_status_reports_only_aggregate_delivery_and_rate_limit_state() {
        let conn = Connection::open_in_memory().expect("in-memory database");
        conn.execute_batch(
            "CREATE TABLE provider_delivery_outbox (
                state TEXT NOT NULL,
                attempts INTEGER NOT NULL,
                duplicate_risk INTEGER NOT NULL,
                created_at INTEGER NOT NULL,
                delivery_kind TEXT NOT NULL
              );
              CREATE TABLE provider_delivery_rate_limits (
                scope TEXT NOT NULL,
                blocked_until INTEGER NOT NULL
              );
              CREATE TABLE artifacts (id TEXT NOT NULL, org TEXT NOT NULL);
              CREATE TABLE artifact_discussions (
                artifact_id TEXT NOT NULL,
                org TEXT NOT NULL,
                mode TEXT NOT NULL,
                state TEXT NOT NULL
              );
              INSERT INTO provider_delivery_outbox VALUES
                ('ready', 1, 0, 900, 'event'),
                ('retry', 2, 1, 800, 'event'),
                ('dead_letter', 3, 1, 700, 'event');
              INSERT INTO provider_delivery_rate_limits VALUES
                ('global', 1_200), ('target', 1_100), ('bucket', 1_050), ('bucket', 900);",
        )
        .expect("fixture schema and rows");

        assert_eq!(
            queue_status(&conn, 1_000).expect("aggregate status"),
            QueueStatus {
                active: 2,
                ready: 1,
                retrying: 1,
                dead_letter: 1,
                attempts: 6,
                ambiguous: 2,
                oldest_active_age_millis: 200,
                rate_limited_global: 1,
                rate_limited_target: 1,
                rate_limited_bucket: 1,
                max_rate_limit_delay_millis: 200,
                discussion_connected: 0,
                discussion_pending: 0,
                discussion_paused: 0,
                discussion_failed: 0,
                discussion_local_only: 0,
                discussion_pending_threads: 0,
                discussion_oldest_pending_thread_age_millis: 0,
                discussion_terminal_failures: 0,
            }
        );
    }

    #[test]
    fn queue_status_reports_discussion_aggregates_without_tenant_identifiers() {
        let conn = Connection::open_in_memory().expect("in-memory database");
        conn.execute_batch(
            "CREATE TABLE provider_delivery_outbox (
                state TEXT NOT NULL, attempts INTEGER NOT NULL, duplicate_risk INTEGER NOT NULL,
                created_at INTEGER NOT NULL, delivery_kind TEXT NOT NULL
              );
              CREATE TABLE provider_delivery_rate_limits (scope TEXT NOT NULL, blocked_until INTEGER NOT NULL);
              CREATE TABLE artifacts (id TEXT NOT NULL, org TEXT NOT NULL);
              CREATE TABLE artifact_discussions (
                artifact_id TEXT NOT NULL, org TEXT NOT NULL, mode TEXT NOT NULL, state TEXT NOT NULL
              );
              INSERT INTO artifacts VALUES
                ('connected', 'alpha'), ('pending', 'alpha'), ('failed', 'alpha'),
                ('paused', 'beta'), ('stored-local', 'beta'), ('local', 'beta');
              INSERT INTO artifact_discussions VALUES
                ('connected', 'alpha', 'discord_mirror', 'connected'),
                ('pending', 'alpha', 'discord_mirror', 'pending'),
                ('failed', 'alpha', 'discord_mirror', 'failed'),
                ('paused', 'beta', 'artifact_only', 'paused'),
                ('stored-local', 'beta', 'artifact_only', 'local');
              INSERT INTO provider_delivery_outbox VALUES
                ('ready', 0, 0, 800, 'discussion_thread'),
                ('retry', 1, 0, 900, 'discussion_thread'),
                ('dead_letter', 2, 0, 700, 'discussion_thread'),
                ('dead_letter', 3, 0, 700, 'discussion_message'),
                ('dead_letter', 4, 0, 700, 'discussion_tombstone'),
                ('dead_letter', 5, 0, 700, 'event');",
        )
        .expect("fixture schema and rows");

        let status = queue_status(&conn, 1_000).expect("aggregate status");
        assert_eq!(status.discussion_connected, 1);
        assert_eq!(status.discussion_pending, 1);
        assert_eq!(status.discussion_paused, 1);
        assert_eq!(status.discussion_failed, 1);
        assert_eq!(
            status.discussion_local_only, 2,
            "stored local rows and the default no-row state share one safe bucket"
        );
        assert_eq!(status.discussion_pending_threads, 2);
        assert_eq!(status.discussion_oldest_pending_thread_age_millis, 200);
        assert_eq!(status.discussion_terminal_failures, 3);
        assert_eq!(
            status.dead_letter, 4,
            "event failures remain outside the discussion total"
        );
    }
}
