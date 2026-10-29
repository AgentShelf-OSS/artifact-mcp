use super::*;

// ---------------------------------------------------------------------------
// Sweeper
// ---------------------------------------------------------------------------

/// The notification payload. It never contains viewer emails.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, serde::Deserialize)]
pub struct PushPayload {
    pub title: String,
    pub body: String,
    pub url: String,
    pub tag: String,
}

/// Aggregate result of one sweeper transaction.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SweepReport {
    /// Reminders fired with recipient expansion (possibly zero recipients).
    pub fired: u64,
    /// Reminders more than six hours late, marked fired without delivery.
    pub stale: u64,
    /// `push_deliveries` rows queued.
    pub deliveries: u64,
}

/// Org-scope recipients: every opted-in viewer's subscriptions whose organization is the artifact's
/// *current* organization, or the administrator tenant. Joining `artifacts` makes an artifact move
/// take effect immediately, whatever org the reminder or opt-in row recorded.
const ORG_RECIPIENTS_SQL: &str = "SELECT s.id FROM artifact_push_optins o JOIN artifacts a ON a.id = o.artifact_id JOIN push_subscriptions s ON s.viewer_email = o.viewer_email AND (s.org = a.org OR s.org = 'admin') WHERE o.artifact_id = ? ORDER BY s.created_at, s.rowid";
/// Viewer-scope recipients: the owner's opt-in (required) and the owner's eligible subscriptions.
const VIEWER_RECIPIENTS_SQL: &str = "SELECT s.id FROM artifact_push_optins o JOIN artifacts a ON a.id = o.artifact_id JOIN push_subscriptions s ON s.viewer_email = o.viewer_email AND (s.org = a.org OR s.org = 'admin') WHERE o.artifact_id = ? AND o.viewer_email = ? ORDER BY s.created_at, s.rowid";

/// Claim up to [`SWEEP_BATCH`] due reminders, expand each into one delivery per recipient
/// subscription, and mark them fired, all in one `BEGIN IMMEDIATE` transaction.
///
/// # Errors
/// [`AppError::Internal`] when SQLite or payload encoding fails; nothing is committed.
pub fn sweep_due(
    conn: &mut Connection,
    now_ms: i64,
    public_base_url: &str,
) -> Result<SweepReport, AppError> {
    let now_text = crate::config::format_sqlite_datetime(now_ms.div_euclid(1000));
    let base = public_base_url.trim_end_matches('/');
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(db_error)?;
    let due = {
        let mut stmt = tx
            .prepare("SELECT artifact_id, scope, owner, key, org, fire_at, title, body FROM artifact_reminders WHERE state = 'armed' AND fire_at <= ? ORDER BY fire_at, artifact_id, key LIMIT ?")
            .map_err(db_error)?;
        stmt.query_map(params![now_ms, SWEEP_BATCH], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, i64>(5)?,
                row.get::<_, String>(6)?,
                row.get::<_, String>(7)?,
            ))
        })
        .map_err(db_error)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(db_error)?
    };
    let mut report = SweepReport::default();
    for (artifact_id, scope, owner, key, _recorded_org, fire_at, title, body) in due {
        if now_ms.saturating_sub(fire_at) <= STALE_AFTER_MS {
            let recipients = if scope == "viewer" {
                let mut stmt = tx.prepare_cached(VIEWER_RECIPIENTS_SQL).map_err(db_error)?;
                stmt.query_map(params![artifact_id, owner], |row| row.get::<_, String>(0))
                    .map_err(db_error)?
                    .collect::<rusqlite::Result<Vec<_>>>()
                    .map_err(db_error)?
            } else {
                let mut stmt = tx.prepare_cached(ORG_RECIPIENTS_SQL).map_err(db_error)?;
                stmt.query_map([&artifact_id], |row| row.get::<_, String>(0))
                    .map_err(db_error)?
                    .collect::<rusqlite::Result<Vec<_>>>()
                    .map_err(db_error)?
            };
            let payload = serde_json::to_string(&PushPayload {
                title,
                body,
                url: format!("{base}/{artifact_id}"),
                tag: format!("{artifact_id}:{key}"),
            })
            .map_err(|_| AppError::Internal)?;
            for subscription in &recipients {
                tx.execute(
                    "INSERT INTO push_deliveries (subscription_id, artifact_id, reminder_key, payload, state, attempts, next_attempt_at, expires_at, created_at, updated_at) VALUES (?, ?, ?, ?, 'pending', 0, ?, ?, ?, ?)",
                    params![subscription, artifact_id, key, payload, now_ms, now_ms + DELIVERY_TTL_MS, now_text, now_text],
                )
                .map_err(db_error)?;
            }
            report.fired += 1;
            report.deliveries += recipients.len() as u64;
        } else {
            report.stale += 1;
        }
        tx.execute(
            "UPDATE artifact_reminders SET state = 'fired', fired_at = ? WHERE artifact_id = ? AND scope = ? AND owner = ? AND key = ?",
            params![now_text, artifact_id, scope, owner, key],
        )
        .map_err(db_error)?;
    }
    tx.commit().map_err(db_error)?;
    Ok(report)
}
