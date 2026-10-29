use super::*;

// ---------------------------------------------------------------------------
// Reminders
// ---------------------------------------------------------------------------

/// `owner` column: `''` for org scope, the lowercased email for viewer scope.
#[must_use]
pub fn reminder_owner(scope: ReminderScope, viewer_email: Option<&str>) -> String {
    match scope {
        ReminderScope::Org => String::new(),
        ReminderScope::Viewer => viewer_email.map(normalize_email).unwrap_or_default(),
    }
}

/// One armed reminder as listed to a viewer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ReminderRecord {
    pub key: String,
    pub scope: ReminderScope,
    pub fire_at: i64,
    pub title: String,
    pub body: String,
    pub revision: i64,
}

/// Armed rows in one `(artifact, scope, owner)` bucket, ordered by key.
///
/// # Errors
/// [`AppError::Internal`] when SQLite fails.
pub fn list_reminders(
    conn: &Connection,
    artifact_id: &str,
    scope: ReminderScope,
    owner: &str,
) -> Result<Vec<ReminderRecord>, AppError> {
    let mut stmt = conn
        .prepare("SELECT key, fire_at, title, body, revision FROM artifact_reminders WHERE artifact_id = ? AND scope = ? AND owner = ? AND state = 'armed' ORDER BY key")
        .map_err(db_error)?;
    stmt.query_map(params![artifact_id, scope.as_str(), owner], |row| {
        Ok(ReminderRecord {
            key: row.get(0)?,
            scope,
            fire_at: row.get(1)?,
            title: row.get(2)?,
            body: row.get(3)?,
            revision: row.get(4)?,
        })
    })
    .map_err(db_error)?
    .collect::<rusqlite::Result<Vec<_>>>()
    .map_err(db_error)
}

/// A validated reminder write.
#[derive(Clone, Debug)]
pub struct ReminderWrite<'a> {
    pub artifact_id: &'a str,
    /// The artifact's organization.
    pub org: &'a str,
    pub scope: ReminderScope,
    pub owner: &'a str,
    pub key: &'a str,
    pub fire_at: i64,
    pub text: &'a ReminderText,
    /// `viewer` or `publisher:<client_id>`; never an email.
    pub created_by: &'a str,
}

/// Arm or re-arm one reminder and return its new revision.
///
/// # Errors
/// [`PushError::ReminderLimit`] when a new armed row would exceed [`ARMED_REMINDER_LIMIT`];
/// [`PushError::BadScope`] for a viewer-scope write without an owner; otherwise
/// [`PushError::Unavailable`].
pub fn set_reminder(
    conn: &mut Connection,
    write: &ReminderWrite<'_>,
    now_text: &str,
) -> Result<i64, PushError> {
    if !valid_key(write.key) {
        return Err(PushError::BadKey);
    }
    if write.scope == ReminderScope::Viewer && write.owner.is_empty() {
        return Err(PushError::BadScope);
    }
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(db_error)?;
    let current: Option<(String, i64)> = tx
        .query_row(
            "SELECT state, revision FROM artifact_reminders WHERE artifact_id = ? AND scope = ? AND owner = ? AND key = ?",
            params![write.artifact_id, write.scope.as_str(), write.owner, write.key],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .map_err(db_error)?;
    // An armed key never counts as a new row; a new key, or a fired key being re-armed, does.
    if current.as_ref().is_none_or(|(state, _)| state != "armed") {
        let armed: i64 = tx
            .query_row(
                "SELECT COUNT(*) FROM artifact_reminders WHERE artifact_id = ? AND state = 'armed'",
                [write.artifact_id],
                |row| row.get(0),
            )
            .map_err(db_error)?;
        if armed >= ARMED_REMINDER_LIMIT {
            return Err(PushError::ReminderLimit);
        }
    }
    let revision = if let Some((_, revision)) = current {
        tx.execute(
            "UPDATE artifact_reminders SET org = ?, fire_at = ?, title = ?, body = ?, state = 'armed', revision = revision + 1, created_by = ?, updated_at = ?, fired_at = NULL WHERE artifact_id = ? AND scope = ? AND owner = ? AND key = ?",
            params![write.org, write.fire_at, write.text.title, write.text.body, write.created_by, now_text, write.artifact_id, write.scope.as_str(), write.owner, write.key],
        )
        .map_err(db_error)?;
        revision + 1
    } else {
        tx.execute(
            "INSERT INTO artifact_reminders (artifact_id, scope, owner, key, org, fire_at, title, body, state, revision, created_by, updated_at, fired_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, 'armed', 1, ?, ?, NULL)",
            params![write.artifact_id, write.scope.as_str(), write.owner, write.key, write.org, write.fire_at, write.text.title, write.text.body, write.created_by, now_text],
        )
        .map_err(db_error)?;
        1
    };
    tx.commit().map_err(db_error)?;
    Ok(revision)
}

/// Delete one reminder key. Absent keys are not an error.
///
/// # Errors
/// [`AppError::Internal`] when SQLite fails.
pub fn clear_reminder(
    conn: &Connection,
    artifact_id: &str,
    scope: ReminderScope,
    owner: &str,
    key: &str,
) -> Result<bool, AppError> {
    conn.execute(
        "DELETE FROM artifact_reminders WHERE artifact_id = ? AND scope = ? AND owner = ? AND key = ?",
        params![artifact_id, scope.as_str(), owner, key],
    )
    .map(|changed| changed > 0)
    .map_err(db_error)
}
