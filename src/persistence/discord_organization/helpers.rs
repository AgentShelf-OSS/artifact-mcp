use rusqlite::{Connection, OptionalExtension, params};

use crate::error::AppError;

use super::types::{
    DiscordCredentialReadiness, OrganizationThreadingStatus, RecoveryDestination, RecoveryRecord,
    RecoveryState,
};

pub(super) const ENCRYPTION_REQUIRED: &str =
    "A server encryption key is required before storing Discord bot credentials.";
pub(super) const INVALID_CREDENTIAL: &str = "Discord bot credential validation failed.";
pub(super) const INVALID_RECOVERY_DESTINATION: &str =
    "Discord recovery destination does not match the organization connection.";

pub(super) fn credential_active(conn: &Connection, org: &str) -> Result<Option<bool>, AppError> {
    conn.query_row(
        "SELECT active FROM org_discord_bot_credentials WHERE org=?1",
        [org],
        |row| row.get::<_, i64>(0),
    )
    .optional()
    .map(|active| active.map(|value| value != 0))
    .map_err(internal)
}

pub(super) fn readiness_in_transaction(
    conn: &Connection,
    org: &str,
    fallback_available: bool,
) -> Result<DiscordCredentialReadiness, AppError> {
    match credential_active(conn, org)? {
        Some(true) => Ok(DiscordCredentialReadiness::Configured),
        Some(false) => Ok(DiscordCredentialReadiness::Deactivated),
        None if fallback_available && has_legacy_discussion_connection(conn, org)? => {
            Ok(DiscordCredentialReadiness::LegacyFallback)
        }
        None => Ok(DiscordCredentialReadiness::Unconfigured),
    }
}

pub(super) fn status_in_transaction(
    conn: &Connection,
    org: &str,
    credential_readiness: DiscordCredentialReadiness,
) -> Result<OrganizationThreadingStatus, AppError> {
    let outbound_enabled = conn
        .query_row(
            "SELECT outbound_enabled FROM org_discord_threading_policies WHERE org=?1",
            [org],
            |row| row.get::<_, i64>(0),
        )
        .optional()
        .map_err(internal)?
        .unwrap_or(0)
        != 0;
    let (pending, failed): (i64, i64) = conn
        .query_row(
            "SELECT \
               COUNT(*) FILTER (WHERE state IN ('pending','recovering','retryable','rate_limited')), \
               COUNT(*) FILTER (WHERE state IN ('not_found','ambiguous','permission_denied','invalid')) \
             FROM discord_notification_anchor_recoveries WHERE org=?1",
            [org],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .map_err(internal)?;
    let recovery_state = if pending > 0 {
        "recovering"
    } else if failed > 0 {
        "degraded"
    } else {
        "idle"
    };
    Ok(OrganizationThreadingStatus {
        outbound_enabled,
        credential_readiness,
        recovery_pending: pending.max(0) as u64,
        recovery_state,
    })
}

pub(super) fn recovery_record(
    conn: &Connection,
    artifact: &str,
    org: &str,
) -> Result<Option<RecoveryRecord>, AppError> {
    conn.query_row(
        "SELECT state, recovered_message_id, attempts, last_error FROM discord_notification_anchor_recoveries WHERE artifact_id=?1 AND org=?2",
        params![artifact, org],
        |row| Ok(RecoveryRecord {
            state: parse_recovery_state(row.get(0)?)?,
            recovered_message_id: row.get(1)?,
            attempts: row.get::<_, i64>(2)?.unsigned_abs(),
            last_error: row.get(3)?,
        }),
    ).optional().map_err(internal)
}

pub(super) fn parse_recovery_state(value: String) -> rusqlite::Result<RecoveryState> {
    match value.as_str() {
        "pending" => Ok(RecoveryState::Pending),
        "recovering" => Ok(RecoveryState::Recovering),
        "recovered" => Ok(RecoveryState::Recovered),
        "not_found" => Ok(RecoveryState::NotFound),
        "ambiguous" => Ok(RecoveryState::Ambiguous),
        "permission_denied" => Ok(RecoveryState::PermissionDenied),
        "rate_limited" => Ok(RecoveryState::RateLimited),
        "retryable" => Ok(RecoveryState::Retryable),
        "invalid" => Ok(RecoveryState::Invalid),
        _ => Err(rusqlite::Error::InvalidQuery),
    }
}

pub(super) fn destination_matches(
    conn: &Connection,
    org: &str,
    destination: &RecoveryDestination,
) -> Result<bool, AppError> {
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM org_discord_discussion_connections WHERE id=?1 AND org=?2 \
         AND strategy='notification_thread' AND notification_webhook_id=?3 \
         AND notification_provider_webhook_id=?4 AND guild_id=?5 AND channel_id=?6)",
        params![
            destination.connection_id,
            org,
            destination.notification_webhook_id,
            destination.provider_webhook_id,
            destination.guild_id,
            destination.channel_id
        ],
        |row| row.get(0),
    )
    .map_err(internal)
}

pub(super) fn destination_matches_preflight(destination: &RecoveryDestination) -> bool {
    !destination.connection_id.trim().is_empty()
        && !destination.notification_webhook_id.trim().is_empty()
        && valid_discord_id(destination.provider_webhook_id.trim())
        && valid_discord_id(destination.guild_id.trim())
        && valid_discord_id(destination.channel_id.trim())
}

pub(super) fn artifact_exists(
    conn: &Connection,
    artifact: &str,
    org: &str,
) -> Result<bool, AppError> {
    conn.query_row(
        "SELECT 1 FROM artifacts WHERE id=?1 AND org=?2",
        params![artifact, org],
        |_| Ok(()),
    )
    .optional()
    .map(|row: Option<()>| row.is_some())
    .map_err(internal)
}

pub(super) fn org_exists(conn: &Connection, org: &str) -> Result<bool, AppError> {
    conn.query_row("SELECT 1 FROM orgs WHERE name=?1", [org], |_| Ok(()))
        .optional()
        .map(|row: Option<()>| row.is_some())
        .map_err(internal)
}

/// The process-level token exists only to carry forward a deployment that already used a
/// PBI-079 discussion connection. It is never offered to an unrelated organization merely
/// because the deployment has a fallback configured.
pub(super) fn has_legacy_discussion_connection(
    conn: &Connection,
    org: &str,
) -> Result<bool, AppError> {
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM org_discord_discussion_connections WHERE org=?1)",
        [org],
        |row| row.get(0),
    )
    .map_err(internal)
}

pub(super) fn is_canonical_url(value: &str) -> bool {
    url::Url::parse(value).is_ok_and(|parsed| {
        matches!(parsed.scheme(), "http" | "https")
            && parsed.host_str().is_some()
            && parsed.fragment().is_none()
    })
}

pub(super) fn valid_discord_id(value: &str) -> bool {
    !value.is_empty() && value.len() <= 32 && value.bytes().all(|byte| byte.is_ascii_digit())
}
pub(super) fn trimmed(value: &str) -> String {
    value.trim().to_owned()
}
pub(super) fn internal(error: rusqlite::Error) -> AppError {
    tracing::error!(error = %error, "organization Discord persistence failed");
    AppError::Internal
}
