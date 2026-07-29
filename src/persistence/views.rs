//! Owned by U10 (terra) — view analytics persistence.
//!
//! Port of `lib/views.js`: one `artifact_views` row per `(artifact_id, email)` with a hit counter
//! and first/last timestamps (v10 `artifact-view-analytics`, `lib/migrations.js:288-304`). The
//! composite foreign key `(artifact_id, org) REFERENCES artifacts(id, org) ON DELETE CASCADE`
//! removes the analytics with the artifact.
//!
//! # Semantics preserved
//!
//! * **Recording is best-effort and must never fail the request.** `lib/views.js:40-46` swallows
//!   every error from the upsert; `lib/app.js:494-501` then wraps the call in a *second*
//!   `try`/`catch` that only logs. Both layers exist here: [`record`] cannot fail, and
//!   [`shell_analytics`] mirrors the shell route's read-side `try`/`catch`
//!   (`lib/app.js:503-510`), including the fact that a `counts` value already assigned before a
//!   later failure is kept. [`gallery_analytics`] does the same for the gallery's per-org loop
//!   (`lib/app.js:219-228`). The *read* side is best-effort too — every analytics query on a
//!   route path is wrapped, so a broken `artifact_views` table degrades the page instead of
//!   failing the request.
//! * **The shell render is the single attribution point.** `lib/app.js:491-501` records a view
//!   only from `GET /:id` and only when `!viewer.isAdmin && viewer.email`. Raw, thumbnail, and
//!   bundle-subresource requests deliberately do not count, so a page with N iframes is still one
//!   view. [`should_record`] is that predicate; route units must not call [`record`] anywhere else.
//! * **Repeat views increment, they do not duplicate.**
//!   `ON CONFLICT(artifact_id, email) DO UPDATE SET count = count + 1, last_viewed_at =
//!   datetime('now')` (`lib/views.js:7-13`) keeps `unique_viewers` stable while `views` grows.
//! * **Ordering is observable.** `viewersFor` is `ORDER BY last_viewed_at DESC` and `topForOrg` is
//!   `ORDER BY views DESC, last_viewed_at DESC` (`lib/views.js:24-38`); both feed ordered JSON
//!   arrays (`artifact_stats` in `lib/mcp.js:502`, the gallery in `lib/portal.js`). Neither query
//!   gains a tiebreaker here — an extra sort key would diverge wherever Node's order is arbitrary.
//! * **An artifact with no rows still reports zeroes.** The aggregate in `lib/views.js:14-19` has
//!   no `GROUP BY`, so it always yields one row: `{ views: 0, unique_viewers: 0,
//!   last_viewed_at: null }`.

use std::collections::BTreeMap;

use rusqlite::Connection;

use crate::error::AppError;
use crate::model::{
    ArtifactId, EmailAddress, OrgId, Timestamp, TopViewedArtifact, ViewCounts, Viewer, ViewerView,
};
use crate::persistence::db::{self, DbPool};

/// `topForOrg`'s default limit (`lib/views.js:64`).
pub const DEFAULT_TOP_LIMIT: usize = 10;

/// `lib/views.js:7-13`.
const RECORD_SQL: &str = "\
INSERT INTO artifact_views (artifact_id, org, email)
VALUES (?, ?, ?)
ON CONFLICT(artifact_id, email) DO UPDATE SET
  count = count + 1,
  last_viewed_at = datetime('now')";

/// `lib/views.js:14-19`.
const COUNTS_SQL: &str = "\
SELECT COALESCE(SUM(count), 0) AS views,
       COUNT(*) AS unique_viewers,
       MAX(last_viewed_at) AS last_viewed_at
FROM artifact_views WHERE artifact_id = ?";

/// `lib/views.js:20-23`.
const COUNTS_FOR_ORG_SQL: &str = "\
SELECT artifact_id, SUM(count) AS views, COUNT(*) AS unique_viewers
FROM artifact_views WHERE org = ? GROUP BY artifact_id";

/// `lib/views.js:24-28`.
const VIEWERS_SQL: &str = "\
SELECT email, count, first_viewed_at, last_viewed_at
FROM artifact_views WHERE artifact_id = ?
ORDER BY last_viewed_at DESC";

/// `lib/views.js:29-38`.
const TOP_SQL: &str = "\
SELECT a.id AS artifact_id, a.title, SUM(v.count) AS views, COUNT(*) AS unique_viewers,
       MAX(v.last_viewed_at) AS last_viewed_at
FROM artifact_views v
INNER JOIN artifacts a ON a.id = v.artifact_id AND a.org = v.org
WHERE v.org = ?
GROUP BY a.id, a.title
ORDER BY views DESC, last_viewed_at DESC
LIMIT ?";

/// Read-side analytics for the artifact shell, with the route's best-effort contract baked in.
///
/// `None` means "the read failed and the shell renders without it", exactly like the `null`
/// initialisers in `lib/app.js:503-504`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ShellAnalytics {
    /// Aggregate counts, or `None` when the aggregate read failed.
    pub counts: Option<ViewCounts>,
    /// Per-viewer rows; `None` for non-admins *and* when the read failed, as in Node.
    pub viewers: Option<Vec<ViewerView>>,
}

/// Read-side analytics for the gallery, with the route's per-org best-effort contract baked in.
///
/// `lib/app.js:219-228` loops over the viewer's orgs and wraps *each* iteration in its own
/// `try`/`catch`, so one failing tenant neither aborts the others nor discards the counts already
/// merged for it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GalleryAnalytics {
    /// Merged per-artifact counts across every org the viewer can see.
    pub view_counts: BTreeMap<ArtifactId, ViewCounts>,
    /// Admin-only most-viewed lists, keyed by org. Empty for members.
    pub top_viewed: BTreeMap<OrgId, Vec<TopViewedArtifact>>,
}

/// Whether this viewer's shell render counts as a view (`lib/app.js:495`).
///
/// Admins are excluded so operator browsing does not inflate a tenant's analytics, and an
/// unidentified viewer has no key to attribute the row to. JavaScript truthiness of
/// `viewer.email` also excludes the empty string.
#[must_use]
pub fn should_record(viewer: &Viewer) -> bool {
    !viewer.is_admin
        && viewer
            .email
            .as_ref()
            .is_some_and(|email| !email.0.is_empty())
}

/// Records one view. Never fails: analytics must not affect the artifact response path.
///
/// This is `lib/views.js:40-46` — the `catch {}` is the contract, not an oversight. A missing
/// artifact (foreign key), a locked database, or a read-only file all end here silently.
pub fn record(conn: &Connection, artifact_id: &ArtifactId, org: &OrgId, email: &EmailAddress) {
    if let Err(error) = record_strict(conn, artifact_id, org, email) {
        // Logged, never propagated: `lib/app.js:498-500` does the same with logger.error.
        tracing::warn!(
            artifact_id = %artifact_id,
            error = %error,
            "view analytics record failed (ignored)"
        );
    }
}

/// The recording upsert without the best-effort wrapper, for diagnostics and tests.
///
/// # Errors
/// Returns the raw driver error, including the foreign-key failure a deleted artifact produces.
pub fn record_strict(
    conn: &Connection,
    artifact_id: &ArtifactId,
    org: &OrgId,
    email: &EmailAddress,
) -> Result<(), rusqlite::Error> {
    conn.execute(RECORD_SQL, (&artifact_id.0, &org.0, &email.0))?;
    Ok(())
}

/// Aggregate counts for one artifact; zeroes when it has never been viewed.
///
/// # Errors
/// Returns [`AppError::Internal`] if the query fails.
pub fn counts_for(conn: &Connection, artifact_id: &ArtifactId) -> Result<ViewCounts, AppError> {
    conn.query_row(COUNTS_SQL, (&artifact_id.0,), |row| {
        Ok(ViewCounts {
            views: counter(row.get(0)?),
            unique_viewers: counter(row.get(1)?),
            last_viewed_at: row.get::<_, Option<String>>(2)?.map(Timestamp),
        })
    })
    .map_err(|error| internal("read view counts", &error))
}

/// Per-artifact counts for one tenant (`lib/views.js:52-58`).
///
/// # Errors
/// Returns [`AppError::Internal`] if the query fails.
pub fn counts_for_org(
    conn: &Connection,
    org: &OrgId,
) -> Result<BTreeMap<ArtifactId, ViewCounts>, AppError> {
    let mut stmt = conn
        .prepare(COUNTS_FOR_ORG_SQL)
        .map_err(|error| internal("prepare org view counts", &error))?;
    let rows = stmt
        .query_map((&org.0,), |row| {
            Ok((
                ArtifactId(row.get(0)?),
                ViewCounts {
                    views: counter(row.get(1)?),
                    unique_viewers: counter(row.get(2)?),
                    // The org projection deliberately omits last_viewed_at (`lib/views.js:21`);
                    // the gallery badge only renders views/unique_viewers.
                    last_viewed_at: None,
                },
            ))
        })
        .map_err(|error| internal("query org view counts", &error))?;

    let mut map = BTreeMap::new();
    for row in rows {
        let (id, counts) = row.map_err(|error| internal("read org view counts", &error))?;
        map.insert(id, counts);
    }
    Ok(map)
}

/// Named viewers of one artifact, newest visit first (`lib/views.js:24-28`).
///
/// # Errors
/// Returns [`AppError::Internal`] if the query fails.
pub fn viewers_for(
    conn: &Connection,
    artifact_id: &ArtifactId,
) -> Result<Vec<ViewerView>, AppError> {
    let mut stmt = conn
        .prepare(VIEWERS_SQL)
        .map_err(|error| internal("prepare viewers", &error))?;
    let rows = stmt
        .query_map((&artifact_id.0,), |row| {
            Ok(ViewerView {
                email: EmailAddress(row.get(0)?),
                count: counter(row.get(1)?),
                first_viewed_at: Timestamp(row.get(2)?),
                last_viewed_at: Timestamp(row.get(3)?),
            })
        })
        .map_err(|error| internal("query viewers", &error))?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(|error| internal("read viewers", &error))
}

/// `Math.max(1, Number(limit) || 10)` from `lib/views.js:65`.
///
/// `0` is falsy in JavaScript, so it becomes the default rather than an empty result; `usize`
/// cannot be negative, so the `max(1, …)` clamp only matters for that zero case.
#[must_use]
pub const fn normalize_top_limit(limit: usize) -> usize {
    if limit == 0 { DEFAULT_TOP_LIMIT } else { limit }
}

/// Most-viewed artifacts in one tenant (`lib/views.js:29-38`, `64-66`).
///
/// # Errors
/// Returns [`AppError::Internal`] if the query fails.
pub fn top_for_org(
    conn: &Connection,
    org: &OrgId,
    limit: usize,
) -> Result<Vec<TopViewedArtifact>, AppError> {
    let effective = i64::try_from(normalize_top_limit(limit)).unwrap_or(i64::MAX);
    let mut stmt = conn
        .prepare(TOP_SQL)
        .map_err(|error| internal("prepare top viewed", &error))?;
    let rows = stmt
        .query_map((&org.0, effective), |row| {
            Ok(TopViewedArtifact {
                artifact_id: ArtifactId(row.get(0)?),
                title: row.get(1)?,
                views: counter(row.get(2)?),
                unique_viewers: counter(row.get(3)?),
                last_viewed_at: Timestamp(row.get(4)?),
            })
        })
        .map_err(|error| internal("query top viewed", &error))?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(|error| internal("read top viewed", &error))
}

/// The shell route's read-side analytics, best-effort in the same shape as
/// `lib/app.js:503-510`.
///
/// A failure of the aggregate read leaves both fields `None`; a failure of the viewer read keeps
/// an aggregate that was already loaded, because Node assigns `counts` before it can throw on
/// `viewersFor`.
#[must_use]
pub fn shell_analytics(
    conn: &Connection,
    artifact_id: &ArtifactId,
    is_admin: bool,
) -> ShellAnalytics {
    let mut analytics = ShellAnalytics::default();
    match counts_for(conn, artifact_id) {
        Ok(counts) => analytics.counts = Some(counts),
        Err(error) => {
            tracing::warn!(
                artifact_id = %artifact_id,
                error = %error,
                "view analytics shell read failed (ignored)"
            );
            return analytics;
        }
    }
    if is_admin {
        match viewers_for(conn, artifact_id) {
            Ok(viewers) => analytics.viewers = Some(viewers),
            Err(error) => tracing::warn!(
                artifact_id = %artifact_id,
                error = %error,
                "view analytics shell read failed (ignored)"
            ),
        }
    }
    analytics
}

/// The gallery route's analytics, best-effort per org (`lib/app.js:219-228`).
///
/// A tenant whose read fails is logged and skipped; the remaining tenants still render, and a
/// tenant whose counts merged before its `top_for_org` failed keeps those counts.
#[must_use]
pub fn gallery_analytics(conn: &Connection, orgs: &[OrgId], is_admin: bool) -> GalleryAnalytics {
    let mut analytics = GalleryAnalytics::default();
    for tenant in orgs {
        let counts = match counts_for_org(conn, tenant) {
            Ok(counts) => counts,
            Err(error) => {
                tracing::warn!(org = %tenant, error = %error, "view analytics gallery read failed (ignored)");
                continue;
            }
        };
        analytics.view_counts.extend(counts);
        if is_admin {
            match top_for_org(conn, tenant, DEFAULT_TOP_LIMIT) {
                Ok(top) => {
                    analytics.top_viewed.insert(tenant.clone(), top);
                }
                Err(error) => tracing::warn!(
                    org = %tenant,
                    error = %error,
                    "view analytics gallery read failed (ignored)"
                ),
            }
        }
    }
    analytics
}

/// Pooled [`record`]: still best-effort, so a pool or task failure is swallowed too.
///
/// The frozen `EngagementService::record_view` returns `Result`, but this adapter only ever
/// reports `Ok(())` — Node's caller has no failure path to reproduce.
///
/// # Errors
/// Never; the signature exists to satisfy the frozen port shape.
pub async fn record_pooled(
    pool: &DbPool,
    artifact_id: ArtifactId,
    org: OrgId,
    email: EmailAddress,
) -> Result<(), AppError> {
    let outcome = db::interact(pool, move |conn| {
        record(conn, &artifact_id, &org, &email);
        Ok(())
    })
    .await;
    if let Err(error) = outcome {
        tracing::warn!(error = %error, "view analytics record could not run (ignored)");
    }
    Ok(())
}

/// Pooled [`counts_for`].
///
/// # Errors
/// See [`counts_for`]; also [`AppError::Unavailable`] when no connection is available.
pub async fn counts_for_pooled(
    pool: &DbPool,
    artifact_id: ArtifactId,
) -> Result<ViewCounts, AppError> {
    db::interact(pool, move |conn| counts_for(conn, &artifact_id)).await
}

/// Pooled [`counts_for_org`].
///
/// # Errors
/// See [`counts_for_org`]; also [`AppError::Unavailable`] when no connection is available.
pub async fn counts_for_org_pooled(
    pool: &DbPool,
    org: OrgId,
) -> Result<BTreeMap<ArtifactId, ViewCounts>, AppError> {
    db::interact(pool, move |conn| counts_for_org(conn, &org)).await
}

/// Pooled [`viewers_for`].
///
/// # Errors
/// See [`viewers_for`]; also [`AppError::Unavailable`] when no connection is available.
pub async fn viewers_for_pooled(
    pool: &DbPool,
    artifact_id: ArtifactId,
) -> Result<Vec<ViewerView>, AppError> {
    db::interact(pool, move |conn| viewers_for(conn, &artifact_id)).await
}

/// Pooled [`top_for_org`].
///
/// # Errors
/// See [`top_for_org`]; also [`AppError::Unavailable`] when no connection is available.
pub async fn top_for_org_pooled(
    pool: &DbPool,
    org: OrgId,
    limit: usize,
) -> Result<Vec<TopViewedArtifact>, AppError> {
    db::interact(pool, move |conn| top_for_org(conn, &org, limit)).await
}

/// Pooled [`shell_analytics`]: best-effort end to end, including pool failures.
pub async fn shell_analytics_pooled(
    pool: &DbPool,
    artifact_id: ArtifactId,
    is_admin: bool,
) -> ShellAnalytics {
    db::interact(pool, move |conn| {
        Ok(shell_analytics(conn, &artifact_id, is_admin))
    })
    .await
    .unwrap_or_else(|error| {
        tracing::warn!(error = %error, "view analytics shell read could not run (ignored)");
        ShellAnalytics::default()
    })
}

/// SQLite counters are `i64`; `rusqlite` only reads `u64` behind an optional feature this crate
/// does not enable. View counts are monotonic non-negative sums, so a negative value would mean a
/// hand-corrupted row — reported as zero rather than a panic.
const fn counter(value: i64) -> u64 {
    if value < 0 { 0 } else { value.unsigned_abs() }
}

/// SQL faults are logged and reported without leaking driver detail.
fn internal(operation: &str, error: &rusqlite::Error) -> AppError {
    tracing::error!(operation, error = %error, "view analytics query failed");
    AppError::Internal
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attributes_views_only_to_identified_non_admin_viewers() {
        let member = Viewer {
            email: Some(EmailAddress::from("member@acme.test")),
            org: Some(OrgId::from("acme")),
            is_admin: false,
        };
        assert!(should_record(&member));

        let admin = Viewer {
            is_admin: true,
            ..member.clone()
        };
        assert!(!should_record(&admin));

        let anonymous = Viewer {
            email: None,
            ..member.clone()
        };
        assert!(!should_record(&anonymous));

        let empty_email = Viewer {
            email: Some(EmailAddress::default()),
            ..member
        };
        assert!(!should_record(&empty_email));
    }

    #[test]
    fn treats_a_zero_limit_as_nodes_default() {
        assert_eq!(normalize_top_limit(0), 10);
        assert_eq!(normalize_top_limit(1), 1);
        assert_eq!(normalize_top_limit(25), 25);
        assert_eq!(DEFAULT_TOP_LIMIT, 10);
    }
}
