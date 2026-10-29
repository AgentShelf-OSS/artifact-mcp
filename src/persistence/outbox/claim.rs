use crate::error::AppError;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};

use super::{DeliveryRecord, LEASE_MILLIS, MAX_ATTEMPTS, get, internal};

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
