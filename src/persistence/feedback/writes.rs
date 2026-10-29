use rusqlite::{Connection, params};

use crate::config::IdSource;
use crate::error::AppError;
use crate::model::{
    ArtifactId, EmailAddress, Feedback, FeedbackId, FeedbackMutation, FeedbackRef, OrgId,
    SubmitFeedback,
};

use super::anchors::{js_trim, normalize_anchor_with_v2, utf16_len};
use super::reads::{failed, get};
use super::sql::{DELETE_SQL, INSERT_SQL, REOPEN_SQL, RESOLVE_SQL};
use super::{
    EMPTY_BODY_MESSAGE, FORBIDDEN_MESSAGE, NOT_FOUND_MESSAGE, PARENT_NOT_FOUND_MESSAGE,
    PARENT_NOT_TOP_LEVEL_MESSAGE, PARENT_OTHER_ARTIFACT_MESSAGE, too_long_message,
};

// ---------------------------------------------------------------------------
// Writes
// ---------------------------------------------------------------------------

/// Everything `addFeedback` derives from the *authorized artifact* rather than from viewer input
/// — org and revision are read off the artifact row in Node too [lib/app.js:571-574].
#[derive(Clone, Copy, Debug)]
pub struct NewFeedback<'a> {
    pub artifact_id: &'a ArtifactId,
    pub org: &'a OrgId,
    /// `meta.revision`; `Number(x) || 1` maps a zero/absent revision to 1 [lib/feedback.js:104].
    pub artifact_revision: u64,
    pub submission: &'a SubmitFeedback,
    /// The output of [`validate_anchor_page`](super::validate_anchor_page), which the route runs first [lib/app.js:570].
    pub anchor_page: Option<&'a str>,
    /// `FEEDBACK_MAX_BODY` from `AppConfig::limits` [lib/config.js:21].
    pub max_body: u64,
}

/// `addFeedback({ … })` — [lib/feedback.js:84-110].
///
/// # Errors
/// [`AppError::Validation`] for [`EMPTY_BODY_MESSAGE`], [`too_long_message`], the three reply-parent
/// messages, and the anchor messages — evaluated in exactly that order. A `CHECK` violation from
/// the `feedback` table (reachable only when `FEEDBACK_MAX_BODY` is configured above the schema's
/// own 4000-character bound) is surfaced as [`AppError::Validation`] with SQLite's message, the way
/// Node's route surfaces a thrown `SqliteError` as a 400 [lib/app.js:600]. Any other SQL fault is
/// [`AppError::Unavailable`].
pub fn add(
    conn: &Connection,
    ids: &dyn IdSource,
    input: &NewFeedback<'_>,
) -> Result<Feedback, AppError> {
    let trimmed = js_trim(&input.submission.body);
    if trimmed.is_empty() {
        return Err(AppError::Validation(EMPTY_BODY_MESSAGE.to_owned()));
    }
    if utf16_len(trimmed) > usize::try_from(input.max_body).unwrap_or(usize::MAX) {
        return Err(AppError::Validation(too_long_message(input.max_body)));
    }

    // `parentId == null || parentId === "" ? null : String(parentId)` — [lib/feedback.js:89]
    let parent_id = input
        .submission
        .parent_id
        .as_ref()
        .filter(|parent| !parent.0.is_empty());
    if let Some(parent_id) = parent_id {
        let parent = get(conn, parent_id)?
            .ok_or_else(|| AppError::Validation(PARENT_NOT_FOUND_MESSAGE.to_owned()))?;
        if &parent.artifact_id != input.artifact_id || &parent.org != input.org {
            return Err(AppError::Validation(
                PARENT_OTHER_ARTIFACT_MESSAGE.to_owned(),
            ));
        }
        if parent.parent_id.is_some() {
            return Err(AppError::Validation(
                PARENT_NOT_TOP_LEVEL_MESSAGE.to_owned(),
            ));
        }
    }

    // A reply never carries an anchor or a page: `parent_id ? normalizeAnchor(null) : …`
    // [lib/feedback.js:106-107]. Both are silently dropped, not rejected.
    let anchor = if parent_id.is_some() {
        None
    } else {
        input.submission.anchor.as_ref()
    };
    let normalized = normalize_anchor_with_v2(
        anchor,
        input.submission.anchor_path.as_deref(),
        if parent_id.is_some() {
            None
        } else {
            input.submission.anchor_v2.as_ref()
        },
    )?;
    let anchor_page = if parent_id.is_some() || anchor.is_none() {
        None
    } else {
        input.anchor_page
    };

    let id = ids.feedback_id()?;
    // `Number(artifactRevision) || 1` — a zero or unusable revision becomes 1, and the column's
    // own `CHECK (artifact_revision >= 1)` would reject anything else anyway.
    let revision = i64::try_from(input.artifact_revision)
        .ok()
        .filter(|revision| *revision > 0)
        .unwrap_or(1);
    conn.execute(
        INSERT_SQL,
        params![
            id.0,
            input.artifact_id.0,
            input.org.0,
            input.submission.viewer_email.0,
            trimmed,
            revision,
            parent_id.map(|parent| parent.0.as_str()),
            normalized.anchor_path,
            normalized.anchor_x,
            normalized.anchor_y,
            normalized.anchor_w,
            normalized.anchor_h,
            i64::from(normalized.anchor_approx),
            anchor_page,
            normalized.anchor_kind,
            normalized.anchor_node_id,
            normalized.anchor_quote,
        ],
    )
    .map_err(|error| insert_failure(&error))?;

    // `return getStmt.get(id)` — the row, not the input, is what callers serialize.
    get(conn, &id)?.ok_or(AppError::Internal)
}

/// `resolveFeedback(id, resolvedBy)` — [lib/feedback.js:121-123]. The agent/publisher path; the
/// caller has already proven artifact ownership [lib/mcp.js:546-549].
///
/// `false` means "no state transition" (already resolved), which is what suppresses a duplicate
/// `resolved` webhook [lib/mcp.js:551].
///
/// # Errors
/// [`AppError::Unavailable`] if the update fails.
pub fn resolve_as_publisher(
    conn: &Connection,
    id: &FeedbackId,
    resolved_by: &str,
) -> Result<bool, AppError> {
    let changed = conn
        .execute(RESOLVE_SQL, params![resolved_by, id.0])
        .map_err(|error| failed("resolve feedback", &error))?;
    Ok(changed > 0)
}

/// `reopenFeedback(id)` — [lib/feedback.js:143-145]. `false` when it was not resolved.
///
/// # Errors
/// [`AppError::Unavailable`] if the update fails.
pub fn reopen(conn: &Connection, id: &FeedbackId) -> Result<bool, AppError> {
    let changed = conn
        .execute(REOPEN_SQL, params![id.0])
        .map_err(|error| failed("reopen feedback", &error))?;
    Ok(changed > 0)
}

/// `resolveByViewer(id, { viewerEmail, isAdmin })` [lib/feedback.js:125-133] **composed with** the
/// route's artifact-scope guard [lib/app.js:624-626].
///
/// `resolved_by` is `admin:<email>` for an administrator [lib/feedback.js:131].
/// [`FeedbackMutation::changed`] is `false` for an already-resolved row, which is what stops a
/// retried resolve from re-emitting the `resolved` notification [lib/app.js:629].
///
/// # Errors
/// [`AppError::NotFound`] ([`NOT_FOUND_MESSAGE`]) when the row is missing or belongs to another
/// artifact/org, [`AppError::Forbidden`] ([`FORBIDDEN_MESSAGE`]) for another viewer's row, or
/// [`AppError::Unavailable`] on a SQL fault.
pub fn resolve_as_viewer(
    conn: &Connection,
    scope: &FeedbackRef,
    viewer_email: &EmailAddress,
    is_admin: bool,
) -> Result<FeedbackMutation, AppError> {
    let row = scoped_row(conn, scope, viewer_email, is_admin)?;
    let resolved_by = if is_admin {
        format!("admin:{viewer_email}")
    } else {
        viewer_email.0.clone()
    };
    let changed = resolve_as_publisher(conn, &row.id, &resolved_by)?;
    Ok(FeedbackMutation {
        id: row.id,
        changed,
    })
}

/// `deleteFeedback(id, { viewerEmail, isAdmin })` [lib/feedback.js:135-141] composed with the
/// route's artifact-scope guard [lib/app.js:604-606]. Replies cascade with their parent.
///
/// # Errors
/// As [`resolve_as_viewer`].
pub fn delete_as_viewer(
    conn: &Connection,
    scope: &FeedbackRef,
    viewer_email: &EmailAddress,
    is_admin: bool,
) -> Result<FeedbackMutation, AppError> {
    let row = scoped_row(conn, scope, viewer_email, is_admin)?;
    let deleted = conn
        .execute(DELETE_SQL, params![row.id.0])
        .map_err(|error| failed("delete feedback", &error))?;
    Ok(FeedbackMutation {
        id: row.id,
        changed: deleted > 0,
    })
}

/// The shared "row exists, in this artifact/org, and this viewer may touch it" gate.
fn scoped_row(
    conn: &Connection,
    scope: &FeedbackRef,
    viewer_email: &EmailAddress,
    is_admin: bool,
) -> Result<Feedback, AppError> {
    let row = get(conn, &scope.id)?
        .filter(|row| row.artifact_id == scope.artifact_id && row.org == scope.org)
        .ok_or_else(|| AppError::NotFound(NOT_FOUND_MESSAGE.to_owned()))?;
    if row.viewer_email.as_ref() != Some(viewer_email) && !is_admin {
        return Err(AppError::Forbidden(FORBIDDEN_MESSAGE.to_owned()));
    }
    Ok(row)
}

/// A rejected insert is a validation failure the viewer caused; anything else is infrastructure.
fn insert_failure(error: &rusqlite::Error) -> AppError {
    if let rusqlite::Error::SqliteFailure(inner, Some(message)) = error
        && inner.code == rusqlite::ErrorCode::ConstraintViolation
    {
        return AppError::Validation(message.clone());
    }
    failed("insert feedback", error)
}
