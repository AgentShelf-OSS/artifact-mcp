use crate::error::AppError;
use rusqlite::Connection;

use super::{QueueStatus, internal};

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
