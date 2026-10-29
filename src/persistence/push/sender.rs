use super::*;

// ---------------------------------------------------------------------------
// Sender
// ---------------------------------------------------------------------------

/// A pending delivery joined to its subscription.
#[derive(Clone)]
pub struct DueDelivery {
    pub id: i64,
    pub subscription_id: String,
    pub artifact_id: String,
    pub reminder_key: String,
    pub payload: String,
    pub attempts: i64,
    pub expires_at: i64,
    pub endpoint_ciphertext: String,
    pub p256dh: String,
    pub auth: String,
}

impl std::fmt::Debug for DueDelivery {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DueDelivery")
            .field("id", &self.id)
            .field("attempts", &self.attempts)
            .finish_non_exhaustive()
    }
}

/// Mark pending deliveries past `expires_at` as dead; returns how many.
///
/// # Errors
/// [`AppError::Internal`] when SQLite fails.
pub fn expire_overdue(conn: &Connection, now_ms: i64) -> Result<u64, AppError> {
    let now_text = crate::config::format_sqlite_datetime(now_ms.div_euclid(1000));
    conn.execute(
        "UPDATE push_deliveries SET state = 'dead', updated_at = ? WHERE state = 'pending' AND expires_at <= ?",
        params![now_text, now_ms],
    )
    .map(|changed| changed as u64)
    .map_err(db_error)
}

/// Lease length for a claimed delivery: the 10-second send timeout plus a 20-second margin.
pub const DELIVERY_LEASE_MS: i64 = 30_000;
/// Finished deliveries are kept this long for operator review.
pub const FINISHED_RETENTION_MS: i64 = 7 * 24 * 60 * 60 * 1000;

/// Claim due pending deliveries: in one `BEGIN IMMEDIATE` transaction, select rows with
/// `next_attempt_at <= now` and push their `next_attempt_at` to `now + lease_ms`, so a concurrent
/// claim cannot take the same rows while they are in flight. A crashed sender's rows become due
/// again when the lease ends (at-least-once).
///
/// # Errors
/// [`AppError::Internal`] when SQLite fails.
pub fn claim_due(
    conn: &mut Connection,
    now_ms: i64,
    limit: i64,
    lease_ms: i64,
) -> Result<Vec<DueDelivery>, AppError> {
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(db_error)?;
    let claimed = due_deliveries(&tx, now_ms, limit)?;
    for delivery in &claimed {
        tx.execute(
            "UPDATE push_deliveries SET next_attempt_at = ? WHERE id = ? AND state = 'pending'",
            params![now_ms + lease_ms, delivery.id],
        )
        .map_err(db_error)?;
    }
    tx.commit().map_err(db_error)?;
    Ok(claimed)
}

/// Delete `accepted` and `dead` deliveries last updated more than seven days ago.
///
/// # Errors
/// [`AppError::Internal`] when SQLite fails.
pub fn prune_finished(conn: &Connection, now_ms: i64) -> Result<u64, AppError> {
    let cutoff =
        crate::config::format_sqlite_datetime((now_ms - FINISHED_RETENTION_MS).div_euclid(1000));
    conn.execute(
        "DELETE FROM push_deliveries WHERE state IN ('accepted','dead') AND updated_at < ?",
        [cutoff],
    )
    .map(|changed| changed as u64)
    .map_err(db_error)
}

/// Pending deliveries whose `next_attempt_at` has passed, oldest first (no lease).
///
/// # Errors
/// [`AppError::Internal`] when SQLite fails.
pub fn due_deliveries(
    conn: &Connection,
    now_ms: i64,
    limit: i64,
) -> Result<Vec<DueDelivery>, AppError> {
    let mut stmt = conn
        .prepare("SELECT d.id, d.subscription_id, d.artifact_id, d.reminder_key, d.payload, d.attempts, d.expires_at, s.endpoint_ciphertext, s.p256dh, s.auth FROM push_deliveries d JOIN push_subscriptions s ON s.id = d.subscription_id WHERE d.state = 'pending' AND d.next_attempt_at <= ? ORDER BY d.next_attempt_at, d.id LIMIT ?")
        .map_err(db_error)?;
    stmt.query_map(params![now_ms, limit], |row| {
        Ok(DueDelivery {
            id: row.get(0)?,
            subscription_id: row.get(1)?,
            artifact_id: row.get(2)?,
            reminder_key: row.get(3)?,
            payload: row.get(4)?,
            attempts: row.get(5)?,
            expires_at: row.get(6)?,
            endpoint_ciphertext: row.get(7)?,
            p256dh: row.get(8)?,
            auth: row.get(9)?,
        })
    })
    .map_err(db_error)?
    .collect::<rusqlite::Result<Vec<_>>>()
    .map_err(db_error)
}

/// `200`/`201`/`202`: the push service accepted the message.
///
/// # Errors
/// [`AppError::Internal`] when SQLite fails.
pub fn mark_accepted(
    conn: &mut Connection,
    delivery: &DueDelivery,
    status: u16,
    now_ms: i64,
) -> Result<(), AppError> {
    let now_text = crate::config::format_sqlite_datetime(now_ms.div_euclid(1000));
    let tx = conn.transaction().map_err(db_error)?;
    tx.execute(
        "UPDATE push_deliveries SET state = 'accepted', attempts = attempts + 1, last_status = ?, updated_at = ? WHERE id = ?",
        params![status, now_text, delivery.id],
    )
    .map_err(db_error)?;
    tx.execute(
        "UPDATE push_subscriptions SET last_success_at = ?, failure_count = 0 WHERE id = ?",
        params![now_text, delivery.subscription_id],
    )
    .map_err(db_error)?;
    tx.commit().map_err(db_error)
}

/// `404`/`410`: delete the subscription; its deliveries cascade away.
///
/// # Errors
/// [`AppError::Internal`] when SQLite fails.
pub fn remove_subscription_by_id(
    conn: &mut Connection,
    subscription_id: &str,
) -> Result<bool, AppError> {
    let tx = conn.transaction().map_err(db_error)?;
    // Explicit, so the outcome does not depend on the connection's foreign-key pragma.
    tx.execute(
        "DELETE FROM push_deliveries WHERE subscription_id = ?",
        [subscription_id],
    )
    .map_err(db_error)?;
    let removed = tx
        .execute(
            "DELETE FROM push_subscriptions WHERE id = ?",
            [subscription_id],
        )
        .map_err(db_error)?;
    tx.commit().map_err(db_error)?;
    Ok(removed > 0)
}

/// Whether a retryable failure was rescheduled or ran out of time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RetryOutcome {
    Retrying,
    Dead,
}

/// `429`, `5xx`, network errors: count the failure and reschedule, or mark the delivery dead when
/// the next attempt would fall at or past `expires_at`.
///
/// # Errors
/// [`AppError::Internal`] when SQLite fails.
pub fn mark_retry(
    conn: &mut Connection,
    delivery: &DueDelivery,
    status: Option<u16>,
    next_attempt_at: i64,
    now_ms: i64,
) -> Result<RetryOutcome, AppError> {
    let now_text = crate::config::format_sqlite_datetime(now_ms.div_euclid(1000));
    let outcome = if next_attempt_at >= delivery.expires_at {
        RetryOutcome::Dead
    } else {
        RetryOutcome::Retrying
    };
    let tx = conn.transaction().map_err(db_error)?;
    tx.execute(
        "UPDATE push_deliveries SET state = ?, attempts = attempts + 1, next_attempt_at = ?, last_status = ?, updated_at = ? WHERE id = ?",
        params![
            if outcome == RetryOutcome::Dead { "dead" } else { "pending" },
            next_attempt_at,
            status,
            now_text,
            delivery.id
        ],
    )
    .map_err(db_error)?;
    tx.execute(
        "UPDATE push_subscriptions SET failure_count = failure_count + 1 WHERE id = ?",
        [&delivery.subscription_id],
    )
    .map_err(db_error)?;
    tx.commit().map_err(db_error)?;
    Ok(outcome)
}

/// Any other status, or an endpoint that no longer passes the allowlist: give up.
///
/// # Errors
/// [`AppError::Internal`] when SQLite fails.
pub fn mark_dead(
    conn: &Connection,
    delivery: &DueDelivery,
    status: Option<u16>,
    now_ms: i64,
) -> Result<(), AppError> {
    let now_text = crate::config::format_sqlite_datetime(now_ms.div_euclid(1000));
    conn.execute(
        "UPDATE push_deliveries SET state = 'dead', attempts = attempts + 1, last_status = ?, updated_at = ? WHERE id = ?",
        params![status, now_text, delivery.id],
    )
    .map(|_| ())
    .map_err(db_error)
}
