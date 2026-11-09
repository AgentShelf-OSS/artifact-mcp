use super::*;

// ---------------------------------------------------------------------------
// SQL helpers
// ---------------------------------------------------------------------------

pub(super) fn rows_for_org(conn: &Connection, org: &str) -> Result<Vec<WebhookRow>, AppError> {
    let mut stmt = conn
        .prepare(&format!(
            "SELECT {ROW_COLUMNS} FROM org_webhooks WHERE org = ?1 {ORDER_BY}"
        ))
        .map_err(|error| crate::error::internal("rows for org", error))?;
    let mapped = stmt
        .query_map((org,), WebhookRow::from_row)
        .map_err(|error| crate::error::internal("rows for org", error))?;
    mapped
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(|error| crate::error::internal("rows for org", error))
}

pub(super) fn row_by_id(conn: &Connection, id: &str) -> Result<Option<WebhookRow>, AppError> {
    conn.query_row(
        &format!("SELECT {ROW_COLUMNS} FROM org_webhooks WHERE id = ?1"),
        (id,),
        WebhookRow::from_row,
    )
    .optional()
    .map_err(|error| crate::error::internal("row by id", error))
}

/// Port of `orgExists(name)`.
///
/// Queried directly rather than through `persistence::orgs`: creation needs the check
/// inside its own connection, and the two modules are owned by concurrent units.
pub(super) fn org_exists(conn: &Connection, org: &str) -> Result<bool, AppError> {
    conn.query_row("SELECT 1 FROM orgs WHERE name = ?1", (org,), |_| Ok(()))
        .optional()
        .map(|found: Option<()>| found.is_some())
        .map_err(|error| crate::error::internal("org exists", error))
}

pub(super) fn discussion_anchor_exists(
    conn: &Connection,
    org: &str,
    webhook_id: &str,
) -> Result<bool, AppError> {
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM org_discord_discussion_connections \
         WHERE org = ?1 AND notification_webhook_id = ?2 \
           AND strategy = 'notification_thread')",
        (org, webhook_id),
        |row| row.get(0),
    )
    .map_err(|error| crate::error::internal("discussion anchor exists", error))
}

/// Trim leading and trailing whitespace from a webhook input string.
pub(super) fn trimmed(value: &str) -> String {
    value.trim().to_owned()
}
