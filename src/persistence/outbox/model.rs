use crate::error::AppError;
use crate::persistence::db::{self, DbPool};
use rusqlite::TransactionBehavior;
use std::sync::Arc;

use super::{
    accepted, claim_next, dead_letter, enqueue_many_in_transaction, internal, persist_rate_limit,
    queue_status, retry,
};

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
