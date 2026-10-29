use super::*;

// ---------------------------------------------------------------------------
// Subscriptions and opt-ins
// ---------------------------------------------------------------------------

/// A validated subscription write.
#[derive(Clone)]
pub struct NewSubscription<'a> {
    pub org: &'a str,
    pub viewer_email: &'a str,
    pub endpoint: &'a str,
    pub p256dh: &'a str,
    pub auth: &'a str,
    pub label: &'a str,
}

/// Upsert by endpoint hash. An existing row moves to this viewer and org with new keys; a new row
/// gets `new_id`. `endpoint_ciphertext` is [`pack_endpoint_ciphertext`] of the encrypted endpoint.
/// The viewer then keeps at most [`SUBSCRIPTIONS_PER_VIEWER`] rows, oldest first out.
///
/// # Errors
/// [`AppError::Internal`] when SQLite fails.
pub fn save_subscription(
    conn: &mut Connection,
    endpoint_ciphertext: &str,
    subscription: &NewSubscription<'_>,
    new_id: String,
    now_text: &str,
) -> Result<String, AppError> {
    let email = normalize_email(subscription.viewer_email);
    let sha = endpoint_sha256(subscription.endpoint);
    let ciphertext = endpoint_ciphertext;
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(db_error)?;
    let existing: Option<(String, String, String)> = tx
        .query_row(
            "SELECT id, viewer_email, org FROM push_subscriptions WHERE endpoint_sha256 = ?",
            [&sha],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()
        .map_err(db_error)?;
    let id = if let Some((id, previous_email, previous_org)) = existing {
        // Queued messages were addressed under the previous owner's eligibility.
        if previous_email != email || previous_org != subscription.org {
            tx.execute(
                "DELETE FROM push_deliveries WHERE subscription_id = ? AND state = 'pending'",
                [&id],
            )
            .map_err(db_error)?;
        }
        tx.execute(
            "UPDATE push_subscriptions SET org = ?, viewer_email = ?, endpoint_ciphertext = ?, p256dh = ?, auth = ?, label = ?, failure_count = 0 WHERE id = ?",
            params![subscription.org, email, ciphertext, subscription.p256dh, subscription.auth, subscription.label, id],
        )
        .map_err(db_error)?;
        id
    } else {
        tx.execute(
            "INSERT INTO push_subscriptions (id, org, viewer_email, endpoint_ciphertext, endpoint_sha256, p256dh, auth, label, created_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
            params![new_id, subscription.org, email, ciphertext, sha, subscription.p256dh, subscription.auth, subscription.label, now_text],
        )
        .map_err(db_error)?;
        new_id
    };
    let others = {
        let mut stmt = tx
            .prepare("SELECT id FROM push_subscriptions WHERE viewer_email = ? ORDER BY created_at ASC, rowid ASC")
            .map_err(db_error)?;
        let rows = stmt
            .query_map([&email], |row| row.get::<_, String>(0))
            .map_err(db_error)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(db_error)?;
        rows.into_iter()
            .filter(|row| *row != id)
            .collect::<Vec<_>>()
    };
    let excess = others
        .len()
        .saturating_sub(SUBSCRIPTIONS_PER_VIEWER.saturating_sub(1));
    for old in others.iter().take(excess) {
        tx.execute("DELETE FROM push_subscriptions WHERE id = ?", [old])
            .map_err(db_error)?;
    }
    tx.commit().map_err(db_error)?;
    Ok(id)
}

/// Move every subscription of this viewer to their current organization, dropping pending
/// deliveries that were queued under the previous one. Returns the number of moved rows.
///
/// # Errors
/// [`AppError::Internal`] when SQLite fails.
pub fn refresh_subscription_org(
    conn: &mut Connection,
    viewer_email: &str,
    org: &str,
) -> Result<usize, AppError> {
    let email = normalize_email(viewer_email);
    let stale: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM push_subscriptions WHERE viewer_email = ? AND org <> ?",
            params![email, org],
            |row| row.get(0),
        )
        .map_err(db_error)?;
    if stale == 0 {
        return Ok(0);
    }
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(db_error)?;
    tx.execute(
        "DELETE FROM push_deliveries WHERE state = 'pending' AND subscription_id IN (SELECT id FROM push_subscriptions WHERE viewer_email = ? AND org <> ?)",
        params![email, org],
    )
    .map_err(db_error)?;
    let moved = tx
        .execute(
            "UPDATE push_subscriptions SET org = ? WHERE viewer_email = ? AND org <> ?",
            params![org, email, org],
        )
        .map_err(db_error)?;
    tx.commit().map_err(db_error)?;
    Ok(moved)
}

/// Remove the endpoint only when it belongs to this viewer.
///
/// # Errors
/// [`AppError::Internal`] when SQLite fails.
pub fn remove_subscription(
    conn: &Connection,
    viewer_email: &str,
    endpoint: &str,
) -> Result<bool, AppError> {
    conn.execute(
        "DELETE FROM push_subscriptions WHERE endpoint_sha256 = ? AND viewer_email = ?",
        params![endpoint_sha256(endpoint), normalize_email(viewer_email)],
    )
    .map(|changed| changed > 0)
    .map_err(db_error)
}

/// Subscriptions of this viewer that are eligible for an artifact in `org`: the same organization,
/// or the administrator tenant (administrators can read every organization).
///
/// # Errors
/// [`AppError::Internal`] when SQLite fails.
pub fn device_count(conn: &Connection, org: &str, viewer_email: &str) -> Result<i64, AppError> {
    conn.query_row(
        "SELECT COUNT(*) FROM push_subscriptions WHERE (org = ? OR org = 'admin') AND viewer_email = ?",
        params![org, normalize_email(viewer_email)],
        |row| row.get(0),
    )
    .map_err(db_error)
}

/// Whether the viewer opted in to this artifact.
///
/// # Errors
/// [`AppError::Internal`] when SQLite fails.
pub fn opted_in(
    conn: &Connection,
    artifact_id: &str,
    viewer_email: &str,
) -> Result<bool, AppError> {
    conn.query_row(
        "SELECT 1 FROM artifact_push_optins WHERE artifact_id = ? AND viewer_email = ?",
        params![artifact_id, normalize_email(viewer_email)],
        |_| Ok(()),
    )
    .optional()
    .map(|row| row.is_some())
    .map_err(db_error)
}

/// Record (or move) the per-artifact opt-in. `org` is the artifact's organization.
///
/// # Errors
/// [`AppError::Internal`] when SQLite fails.
pub fn opt_in(
    conn: &Connection,
    artifact_id: &str,
    org: &str,
    viewer_email: &str,
    now_text: &str,
) -> Result<(), AppError> {
    conn.execute(
        "INSERT INTO artifact_push_optins (artifact_id, viewer_email, org, created_at) VALUES (?, ?, ?, ?) ON CONFLICT(artifact_id, viewer_email) DO UPDATE SET org = excluded.org",
        params![artifact_id, normalize_email(viewer_email), org, now_text],
    )
    .map(|_| ())
    .map_err(db_error)
}

/// Remove the per-artifact opt-in. The device subscriptions stay.
///
/// # Errors
/// [`AppError::Internal`] when SQLite fails.
pub fn opt_out(conn: &Connection, artifact_id: &str, viewer_email: &str) -> Result<(), AppError> {
    conn.execute(
        "DELETE FROM artifact_push_optins WHERE artifact_id = ? AND viewer_email = ?",
        params![artifact_id, normalize_email(viewer_email)],
    )
    .map(|_| ())
    .map_err(db_error)
}
