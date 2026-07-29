//! Owned by U10 (terra) — viewer notification projections and read watermarks.
//!
//! Port of `lib/notifications.js`. Notifications are not a stored feed: they are a projection of
//! tenant-scoped viewer feedback joined against one durable per-viewer watermark row in
//! `notification_reads` (v20 `notification-read-watermarks`, `lib/migrations.js:402-413`).
//!
//! # Semantics preserved
//!
//! * **Two queries, not one with a predicate.** `lib/notifications.js:9-50` prepares separate
//!   admin and member statements. The member statement adds `a.org = @org`; the admin statement
//!   has no org filter at all, so an admin sees every tenant. [`recent_for_viewer`] and
//!   [`unread_count`] select between them on [`Viewer::is_admin`], exactly as
//!   `lib/notifications.js:66-74` does.
//! * **Self-authored feedback is never a notification.** `f.viewer_email <> @email` in all four
//!   statements.
//! * **`unread` is computed against the watermark, not stored.**
//!   `f.created_at > COALESCE(r.seen_at, @epoch)` with [`EPOCH`] as the never-seen default
//!   (`lib/notifications.js:6`). The join `LEFT JOIN notification_reads r ON r.viewer_email =
//!   @email` is deliberately uncorrelated with `f` — one watermark applies to every row.
//! * **Ordering is observable.** `ORDER BY f.created_at DESC, f.id DESC` feeds an ordered JSON
//!   array into the gallery (`lib/app.js:229-232`), so the tiebreaker on `f.id` is load-bearing
//!   whenever two comments share a second.
//! * **The watermark only moves forward.** `SET seen_at = MAX(notification_reads.seen_at,
//!   excluded.seen_at)` (`lib/notifications.js:51-55`) — a clock that jumps backwards, or a
//!   late-arriving request, cannot resurrect notifications the viewer already dismissed.
//! * **Missing identity degrades to the empty string**, not to an error:
//!   `String(viewer.email || "")` / `String(viewer.org || "")` (`lib/notifications.js:57-64`).
//!   An empty email matches no feedback author, so every row stays "not mine" and unread.
//!
//! # Not best-effort
//!
//! Unlike the view analytics next door, the notification projection is **not** wrapped in a
//! `try`/`catch` by its caller: `lib/app.js:229-232` builds `notificationState` outside the
//! guarded per-org loop above it, so a failure here fails the gallery request. That asymmetry is
//! deliberate in the reference (an empty badge would silently hide feedback), and it is why these
//! functions return `Result` instead of degrading like [`crate::persistence::views`].
//!
//! One mechanical difference: Node passes `@limit` to the counting statements too
//! (`lib/notifications.js:72`), where the SQL does not use it. better-sqlite3 ignores the extra
//! named value; rusqlite rejects an unknown parameter name, so the count path binds only the
//! parameters its statement declares. No observable difference.

use rusqlite::Connection;

use crate::error::AppError;
use crate::model::{
    ArtifactId, EmailAddress, FeedbackId, OrgId, Timestamp, Viewer, ViewerNotification,
};
use crate::persistence::db::{self, DbPool};

/// The "never seen anything" watermark (`lib/notifications.js:6`).
pub const EPOCH: &str = "1970-01-01 00:00:00";

/// `recentForViewer`'s default page size (`lib/notifications.js:66`).
pub const DEFAULT_LIMIT: usize = 30;

/// Upper clamp applied to any requested limit (`lib/notifications.js:62`).
pub const MAX_LIMIT: usize = 100;

/// `lib/notifications.js:9-21`.
const RECENT_ADMIN_SQL: &str = "\
SELECT f.id, f.artifact_id, a.title AS artifact_title, a.org,
       f.body, f.viewer_email, f.created_at, f.parent_id,
       (f.resolved_at IS NOT NULL) AS resolved,
       (f.anchor_x IS NOT NULL AND f.anchor_y IS NOT NULL) AS has_anchor,
       (f.created_at > COALESCE(r.seen_at, @epoch)) AS unread
FROM feedback f
JOIN artifacts a ON a.id = f.artifact_id AND a.org = f.org
LEFT JOIN notification_reads r ON r.viewer_email = @email
WHERE f.viewer_email <> @email
ORDER BY f.created_at DESC, f.id DESC
LIMIT @limit";

/// `lib/notifications.js:22-34`.
const RECENT_MEMBER_SQL: &str = "\
SELECT f.id, f.artifact_id, a.title AS artifact_title, a.org,
       f.body, f.viewer_email, f.created_at, f.parent_id,
       (f.resolved_at IS NOT NULL) AS resolved,
       (f.anchor_x IS NOT NULL AND f.anchor_y IS NOT NULL) AS has_anchor,
       (f.created_at > COALESCE(r.seen_at, @epoch)) AS unread
FROM feedback f
JOIN artifacts a ON a.id = f.artifact_id AND a.org = f.org
LEFT JOIN notification_reads r ON r.viewer_email = @email
WHERE a.org = @org AND f.viewer_email <> @email
ORDER BY f.created_at DESC, f.id DESC
LIMIT @limit";

/// `lib/notifications.js:35-42`.
const COUNT_ADMIN_SQL: &str = "\
SELECT COUNT(*) AS count
FROM feedback f
JOIN artifacts a ON a.id = f.artifact_id AND a.org = f.org
LEFT JOIN notification_reads r ON r.viewer_email = @email
WHERE f.viewer_email <> @email
  AND f.created_at > COALESCE(r.seen_at, @epoch)";

/// `lib/notifications.js:43-50`.
const COUNT_MEMBER_SQL: &str = "\
SELECT COUNT(*) AS count
FROM feedback f
JOIN artifacts a ON a.id = f.artifact_id AND a.org = f.org
LEFT JOIN notification_reads r ON r.viewer_email = @email
WHERE a.org = @org AND f.viewer_email <> @email
  AND f.created_at > COALESCE(r.seen_at, @epoch)";

/// `lib/notifications.js:51-55`.
const MARK_SEEN_SQL: &str = "\
INSERT INTO notification_reads (viewer_email, seen_at) VALUES (?, datetime('now'))
ON CONFLICT(viewer_email) DO UPDATE
SET seen_at = MAX(notification_reads.seen_at, excluded.seen_at)";

/// `Math.max(1, Math.min(100, Number(limit) || 30))` from `lib/notifications.js:62`.
///
/// `0` is falsy in JavaScript and becomes the default; anything above [`MAX_LIMIT`] is clamped.
#[must_use]
pub const fn normalize_limit(limit: usize) -> usize {
    if limit == 0 {
        DEFAULT_LIMIT
    } else if limit > MAX_LIMIT {
        MAX_LIMIT
    } else {
        limit
    }
}

/// `String(viewer.email || "")` (`lib/notifications.js:59`).
fn viewer_email(viewer: &Viewer) -> String {
    viewer
        .email
        .as_ref()
        .map_or_else(String::new, |email| email.0.clone())
}

/// `String(viewer.org || "")` (`lib/notifications.js:60`).
fn viewer_org(viewer: &Viewer) -> String {
    viewer
        .org
        .as_ref()
        .map_or_else(String::new, |org| org.0.clone())
}

/// The viewer's most recent notifications, newest first.
///
/// # Errors
/// Returns [`AppError::Internal`] if the projection query fails.
pub fn recent_for_viewer(
    conn: &Connection,
    viewer: &Viewer,
    limit: usize,
) -> Result<Vec<ViewerNotification>, AppError> {
    let email = viewer_email(viewer);
    let org = viewer_org(viewer);
    let bound = i64::try_from(normalize_limit(limit)).unwrap_or(i64::MAX);

    let sql = if viewer.is_admin {
        RECENT_ADMIN_SQL
    } else {
        RECENT_MEMBER_SQL
    };
    let mut stmt = conn
        .prepare(sql)
        .map_err(|error| internal("prepare notifications", &error))?;

    // The admin statement has no @org placeholder; binding one would be an unknown-parameter error.
    let mut params: Vec<(&str, &dyn rusqlite::ToSql)> =
        vec![("@email", &email), ("@epoch", &EPOCH), ("@limit", &bound)];
    if !viewer.is_admin {
        params.push(("@org", &org));
    }

    let rows = stmt
        .query_map(&params[..], |row| {
            Ok(ViewerNotification {
                id: FeedbackId(row.get(0)?),
                artifact_id: ArtifactId(row.get(1)?),
                artifact_title: row.get(2)?,
                org: OrgId(row.get(3)?),
                body: row.get(4)?,
                viewer_email: EmailAddress(row.get(5)?),
                created_at: Timestamp(row.get(6)?),
                parent_id: row.get::<_, Option<String>>(7)?.map(FeedbackId),
                resolved: row.get::<_, i64>(8)? != 0,
                has_anchor: row.get::<_, i64>(9)? != 0,
                unread: row.get::<_, i64>(10)? != 0,
            })
        })
        .map_err(|error| internal("query notifications", &error))?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(|error| internal("read notifications", &error))
}

/// How many notifications sit above the viewer's watermark (`lib/notifications.js:71-74`).
///
/// # Errors
/// Returns [`AppError::Internal`] if the count query fails.
pub fn unread_count(conn: &Connection, viewer: &Viewer) -> Result<u64, AppError> {
    let email = viewer_email(viewer);
    let org = viewer_org(viewer);

    let sql = if viewer.is_admin {
        COUNT_ADMIN_SQL
    } else {
        COUNT_MEMBER_SQL
    };
    let mut params: Vec<(&str, &dyn rusqlite::ToSql)> =
        vec![("@email", &email), ("@epoch", &EPOCH)];
    if !viewer.is_admin {
        params.push(("@org", &org));
    }

    conn.query_row(sql, &params[..], |row| row.get::<_, i64>(0))
        .map(|count| u64::try_from(count).unwrap_or(0))
        .map_err(|error| internal("count notifications", &error))
}

/// Advances the viewer's read watermark to now, never backwards.
///
/// # Errors
/// Returns [`AppError::Internal`] if the upsert fails.
pub fn mark_seen(conn: &Connection, email: &EmailAddress) -> Result<(), AppError> {
    conn.execute(MARK_SEEN_SQL, (&email.0,))
        .map_err(|error| internal("mark notifications seen", &error))?;
    Ok(())
}

/// The stored watermark for one viewer, or `None` when they have never marked anything seen.
///
/// # Errors
/// Returns [`AppError::Internal`] if the query fails.
pub fn watermark(conn: &Connection, email: &EmailAddress) -> Result<Option<Timestamp>, AppError> {
    use rusqlite::OptionalExtension as _;

    conn.query_row(
        "SELECT seen_at FROM notification_reads WHERE viewer_email = ?",
        (&email.0,),
        |row| row.get::<_, String>(0),
    )
    .optional()
    .map(|value| value.map(Timestamp))
    .map_err(|error| internal("read notification watermark", &error))
}

/// Pooled [`recent_for_viewer`].
///
/// # Errors
/// See [`recent_for_viewer`]; also [`AppError::Unavailable`] when no connection is available.
pub async fn recent_for_viewer_pooled(
    pool: &DbPool,
    viewer: Viewer,
    limit: usize,
) -> Result<Vec<ViewerNotification>, AppError> {
    db::interact(pool, move |conn| recent_for_viewer(conn, &viewer, limit)).await
}

/// Pooled [`unread_count`].
///
/// # Errors
/// See [`unread_count`]; also [`AppError::Unavailable`] when no connection is available.
pub async fn unread_count_pooled(pool: &DbPool, viewer: Viewer) -> Result<u64, AppError> {
    db::interact(pool, move |conn| unread_count(conn, &viewer)).await
}

/// Pooled [`mark_seen`].
///
/// # Errors
/// See [`mark_seen`]; also [`AppError::Unavailable`] when no connection is available.
pub async fn mark_seen_pooled(pool: &DbPool, email: EmailAddress) -> Result<(), AppError> {
    db::interact(pool, move |conn| mark_seen(conn, &email)).await
}

/// SQL faults are logged and reported without leaking driver detail.
fn internal(operation: &str, error: &rusqlite::Error) -> AppError {
    tracing::error!(operation, error = %error, "notification query failed");
    AppError::Internal
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamps_limits_the_way_node_does() {
        assert_eq!(normalize_limit(0), 30);
        assert_eq!(normalize_limit(1), 1);
        assert_eq!(normalize_limit(30), 30);
        assert_eq!(normalize_limit(100), 100);
        assert_eq!(normalize_limit(101), 100);
        assert_eq!(normalize_limit(usize::MAX), 100);
    }

    #[test]
    fn treats_a_missing_identity_as_the_empty_string() {
        let anonymous = Viewer::default();
        assert_eq!(viewer_email(&anonymous), "");
        assert_eq!(viewer_org(&anonymous), "");

        let member = Viewer {
            email: Some(EmailAddress::from("member@acme.test")),
            org: Some(OrgId::from("acme")),
            is_admin: false,
        };
        assert_eq!(viewer_email(&member), "member@acme.test");
        assert_eq!(viewer_org(&member), "acme");
    }
}
