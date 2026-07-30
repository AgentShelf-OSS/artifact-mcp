//! Owned by U11 (terra) — feedback threads, anchors, and ownership mutations.
//!
//! Node oracle: `lib/feedback.js` (whole file) plus the two validators the HTTP route applies
//! before it — `validateAnchorPage` [lib/app.js:13-36] and the ownership pre-checks at
//! [lib/app.js:604-613] / [lib/app.js:617-626].
//!
//! # What is reproduced exactly
//!
//! * **Error strings and their evaluation order.** `addFeedback` checks the body, then the reply
//!   parent, and only then normalizes the anchor — because `normalizeAnchor` is invoked inside the
//!   object literal handed to `insertStmt.run` [lib/feedback.js:107], which JavaScript evaluates
//!   after both guards have already thrown. A submission that is empty *and* carries a broken
//!   anchor reports the empty-body message in both runtimes.
//! * **JavaScript string semantics.** `String.prototype.trim` and `String.prototype.length` are
//!   not Rust's: JS trims `U+FEFF` but not `U+0085`, and counts UTF-16 code units, so one emoji
//!   costs two characters of `FEEDBACK_MAX_BODY`. See [`js_trim`] and [`utf16_len`].
//! * **SQL result ordering.** Every listing keeps Node's `ORDER BY` verbatim: unresolved before
//!   resolved, then the per-listing tiebreak. Later HTTP/MCP conformance compares ordered JSON
//!   arrays, so a reordering is a visible break.
//! * **Thread cascade.** `parent_id TEXT REFERENCES feedback(id) ON DELETE CASCADE` (migration 13)
//!   deletes replies with their parent — which only holds because `foreign_keys = ON` is one of
//!   the six pragmas U03 pins and verifies on every connection.
//!
//! # Scope checks live here, not only in the route
//!
//! Node's viewer mutations are guarded twice: the route rejects a feedback row belonging to a
//! different artifact or org [lib/app.js:610], then the store rejects a different viewer
//! [lib/feedback.js:138]. [`delete_as_viewer`] and [`resolve_as_viewer`] take the artifact scope
//! as an argument and apply both, in Node's order, so a route cannot lose half of the check and
//! turn a feedback id into a cross-tenant oracle.

use rusqlite::{Connection, OptionalExtension, params, params_from_iter};

use crate::config::IdSource;
use crate::error::AppError;
use crate::model::{
    ArtifactId, ClientId, EmailAddress, Feedback, FeedbackAnchor, FeedbackAuthor, FeedbackId,
    FeedbackMutation, FeedbackRef, OrgId, SubmitFeedback, Timestamp,
};

// ---------------------------------------------------------------------------
// Frozen message strings
// ---------------------------------------------------------------------------

/// [lib/feedback.js:86]. The apostrophe is U+2019, not U+0027.
pub const EMPTY_BODY_MESSAGE: &str = "Feedback can\u{2019}t be empty.";
/// [lib/feedback.js:92]
pub const PARENT_NOT_FOUND_MESSAGE: &str = "Reply parent not found.";
/// [lib/feedback.js:93]
pub const PARENT_OTHER_ARTIFACT_MESSAGE: &str = "Reply parent belongs to a different artifact.";
/// [lib/feedback.js:94]
pub const PARENT_NOT_TOP_LEVEL_MESSAGE: &str = "Replies can only be added to top-level feedback.";
/// [lib/feedback.js:14] — unrepresentable in Rust's typed anchor, exported for U13/U19 decoding.
pub const ANCHOR_NOT_OBJECT_MESSAGE: &str = "Anchor must be an object.";
/// [lib/feedback.js:16]
pub const ANCHOR_POINT_MESSAGE: &str = "Anchor x and y must be finite numbers between 0 and 1.";
/// [lib/feedback.js:20]
pub const ANCHOR_BOX_PAIR_MESSAGE: &str =
    "Anchor w and h must either both be supplied or both omitted.";
/// [lib/feedback.js:25]
pub const ANCHOR_BOX_RANGE_MESSAGE: &str = "Anchor w and h must be finite numbers between 0 and 1.";
/// [lib/feedback.js:27]
pub const ANCHOR_BOX_POSITIVE_MESSAGE: &str = "Box anchor w and h must be greater than 0.";
/// [lib/feedback.js:32]
pub const ANCHOR_BOX_BOUNDS_MESSAGE: &str = "Box anchor must fit within document bounds.";
/// [lib/app.js:15]
pub const ANCHOR_PAGE_UNANCHORED_MESSAGE: &str =
    "anchor_page is only valid for anchored bundle feedback.";
/// [lib/app.js:19]
pub const ANCHOR_PAGE_NOT_BUNDLE_MESSAGE: &str = "anchor_page is only valid for bundle feedback.";
/// [lib/app.js:23]
pub const ANCHOR_PAGE_REQUIRED_MESSAGE: &str =
    "anchor_page is required for anchored bundle feedback.";
/// [lib/app.js:27]
pub const ANCHOR_PAGE_TRAVERSAL_MESSAGE: &str =
    "anchor_page must be a bundle-relative path without traversal.";
/// [lib/app.js:30]
pub const ANCHOR_PAGE_NOT_A_FILE_MESSAGE: &str = "anchor_page must identify a bundle HTML file.";
/// [lib/app.js:33]
pub const ANCHOR_PAGE_MISSING_MESSAGE: &str =
    "anchor_page must identify an existing bundle HTML file.";
/// The 404 body both viewer mutations produce — [lib/app.js:611], [lib/app.js:625].
pub const NOT_FOUND_MESSAGE: &str = "Not found";
/// The 403 body both viewer mutations produce — [lib/app.js:611], [lib/app.js:625].
pub const FORBIDDEN_MESSAGE: &str = "Forbidden";

/// `String(anchor.path).slice(0, 512)` — [lib/feedback.js:35]
pub const ANCHOR_PATH_MAX_UTF16: usize = 512;

/// `Feedback is too long (max ${FEEDBACK_MAX_BODY} characters).` — [lib/feedback.js:87]
#[must_use]
pub fn too_long_message(max_body: u64) -> String {
    format!("Feedback is too long (max {max_body} characters).")
}

// ---------------------------------------------------------------------------
// SQL
// ---------------------------------------------------------------------------

/// Explicit column list in the order [`read_row`] expects. Node uses `SELECT *`; naming the
/// columns keeps the mapping independent of the `ALTER TABLE` order migrations 12-19 produced.
const COLUMNS: &str = "id, artifact_id, org, viewer_email, body, artifact_revision, parent_id, \
     anchor_path, anchor_x, anchor_y, anchor_w, anchor_h, anchor_approx, anchor_page, created_at, \
     resolved_at, resolved_by, author_source, external_author_id, external_author_display, \
     external_created_at, external_edited_at, external_deleted_at";

/// The same list qualified for the `feedback f JOIN artifacts a` listings.
const JOINED_COLUMNS: &str = "f.id, f.artifact_id, f.org, f.viewer_email, f.body, \
     f.artifact_revision, f.parent_id, f.anchor_path, f.anchor_x, f.anchor_y, f.anchor_w, \
     f.anchor_h, f.anchor_approx, f.anchor_page, f.created_at, f.resolved_at, f.resolved_by, \
     f.author_source, f.external_author_id, f.external_author_display, f.external_created_at, \
     f.external_edited_at, f.external_deleted_at";

/// `insertStmt` — [lib/feedback.js:45-48]
const INSERT_SQL: &str = "INSERT INTO feedback (id, artifact_id, org, viewer_email, body, \
     artifact_revision, parent_id, anchor_path, anchor_x, anchor_y, anchor_w, anchor_h, \
     anchor_approx, anchor_page) \
     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)";

/// `resolveStmt` — [lib/feedback.js:53-55]
const RESOLVE_SQL: &str = "UPDATE feedback SET resolved_at = datetime('now'), resolved_by = ?1 \
     WHERE id = ?2 AND resolved_at IS NULL";

/// `reopenStmt` — [lib/feedback.js:56-58]
const REOPEN_SQL: &str = "UPDATE feedback SET resolved_at = NULL, resolved_by = NULL \
     WHERE id = ?1 AND resolved_at IS NOT NULL";

/// `deleteStmt` — [lib/feedback.js:59]. Replies cascade via the migration-13 self FK.
const DELETE_SQL: &str = "DELETE FROM feedback WHERE id = ?1";

// ---------------------------------------------------------------------------
// JavaScript string semantics
// ---------------------------------------------------------------------------

/// `String.prototype.trim`'s character set, which is **not** Rust's `char::is_whitespace`.
///
/// ECMA-262 trims WhiteSpace ∪ LineTerminator: TAB, VT, FF, SP, NBSP, ZWNBSP (`U+FEFF`), every
/// `Zs`, LF, CR, LS, PS. Rust's White_Space property adds `U+0085` (NEL) and omits `U+FEFF`, so
/// both differences are corrected here.
#[must_use]
pub const fn is_js_whitespace(value: char) -> bool {
    matches!(value, '\u{feff}') || (value.is_whitespace() && !matches!(value, '\u{85}'))
}

/// `String.prototype.trim` — [lib/feedback.js:85]
#[must_use]
pub fn js_trim(value: &str) -> &str {
    value.trim_matches(is_js_whitespace)
}

/// `String.prototype.length`: UTF-16 code units, so one astral character counts twice.
#[must_use]
pub fn utf16_len(value: &str) -> usize {
    value.chars().map(char::len_utf16).sum()
}

/// `String.prototype.slice(0, limit)` measured in UTF-16 code units.
///
/// A cut that would land inside a surrogate pair drops the whole character instead of emitting the
/// lone surrogate JavaScript would produce — a lone surrogate is not representable in a Rust
/// `String`, and SQLite would not store it as written either.
#[must_use]
pub fn slice_utf16(value: &str, limit: usize) -> String {
    let mut used = 0_usize;
    let mut out = String::new();
    for character in value.chars() {
        let width = character.len_utf16();
        if used + width > limit {
            break;
        }
        used += width;
        out.push(character);
    }
    out
}

// ---------------------------------------------------------------------------
// Anchors
// ---------------------------------------------------------------------------

/// The persisted anchor columns produced by `normalizeAnchor` — [lib/feedback.js:12-42].
#[derive(Clone, Debug, Default, PartialEq)]
pub struct NormalizedAnchor {
    pub anchor_path: Option<String>,
    pub anchor_x: Option<f64>,
    pub anchor_y: Option<f64>,
    pub anchor_w: Option<f64>,
    pub anchor_h: Option<f64>,
    pub anchor_approx: bool,
}

/// `normalizeAnchor(anchor)` — [lib/feedback.js:12-42].
///
/// `path` is `anchor.path` in Node, which lives inside the anchor object; the frozen
/// `SubmitFeedback` carries it beside the anchor, so an unanchored submission discards it exactly
/// as `normalizeAnchor(null)` does.
///
/// An over-hanging box is **trimmed** to the document edge rather than rejected
/// ([lib/feedback.js:30-31]); only a box that starts on the far edge — and therefore trims to zero
/// area — is an error.
///
/// # Errors
/// [`AppError::Validation`] carrying [`ANCHOR_POINT_MESSAGE`], [`ANCHOR_BOX_PAIR_MESSAGE`],
/// [`ANCHOR_BOX_RANGE_MESSAGE`], [`ANCHOR_BOX_POSITIVE_MESSAGE`], or
/// [`ANCHOR_BOX_BOUNDS_MESSAGE`], in that order.
pub fn normalize_anchor(
    anchor: Option<&FeedbackAnchor>,
    path: Option<&str>,
) -> Result<NormalizedAnchor, AppError> {
    let Some(anchor) = anchor else {
        return Ok(NormalizedAnchor::default());
    };

    // `!Number.isFinite(x) || x < 0 || x > 1 || …` — NaN fails every comparison, so `is_finite`
    // plus the range test is the same predicate.
    if !unit_interval(anchor.x) || !unit_interval(anchor.y) {
        return Err(AppError::Validation(ANCHOR_POINT_MESSAGE.to_owned()));
    }

    let (mut anchor_w, mut anchor_h) = (None, None);
    match (anchor.w, anchor.h) {
        (None, None) => {}
        (Some(_), None) | (None, Some(_)) => {
            return Err(AppError::Validation(ANCHOR_BOX_PAIR_MESSAGE.to_owned()));
        }
        (Some(width), Some(height)) => {
            if !unit_interval(width) || !unit_interval(height) {
                return Err(AppError::Validation(ANCHOR_BOX_RANGE_MESSAGE.to_owned()));
            }
            if width <= 0.0 || height <= 0.0 {
                return Err(AppError::Validation(ANCHOR_BOX_POSITIVE_MESSAGE.to_owned()));
            }
            let trimmed_w = width.min(1.0 - anchor.x);
            let trimmed_h = height.min(1.0 - anchor.y);
            if trimmed_w <= 0.0 || trimmed_h <= 0.0 {
                return Err(AppError::Validation(ANCHOR_BOX_BOUNDS_MESSAGE.to_owned()));
            }
            anchor_w = Some(trimmed_w);
            anchor_h = Some(trimmed_h);
        }
    }

    Ok(NormalizedAnchor {
        anchor_path: path.map(|path| slice_utf16(path, ANCHOR_PATH_MAX_UTF16)),
        anchor_x: Some(anchor.x),
        anchor_y: Some(anchor.y),
        anchor_w,
        anchor_h,
        anchor_approx: anchor.approx,
    })
}

/// `Number.isFinite(v) && v >= 0 && v <= 1`.
fn unit_interval(value: f64) -> bool {
    value.is_finite() && (0.0..=1.0).contains(&value)
}

/// `validateAnchorPage(artifacts, meta, anchor, value)` — [lib/app.js:13-36].
///
/// `html_page_exists` is the caller's `readBundleFile(meta.id, normalized)` +
/// `isHtmlContentType(file.contentType)` composition [lib/app.js:31-34]; keeping it as a predicate
/// leaves U07's bundle reads and U19's wiring where they belong while the *rules* stay here.
///
/// `None` and `Some("")` both mean "absent", matching `value != null && value !== ""`.
///
/// # Errors
/// [`AppError::Validation`] carrying [`ANCHOR_PAGE_UNANCHORED_MESSAGE`],
/// [`ANCHOR_PAGE_NOT_BUNDLE_MESSAGE`], [`ANCHOR_PAGE_REQUIRED_MESSAGE`],
/// [`ANCHOR_PAGE_TRAVERSAL_MESSAGE`], [`ANCHOR_PAGE_NOT_A_FILE_MESSAGE`], or
/// [`ANCHOR_PAGE_MISSING_MESSAGE`], in Node's order.
pub fn validate_anchor_page(
    is_bundle: bool,
    anchor: Option<&FeedbackAnchor>,
    value: Option<&str>,
    html_page_exists: &dyn Fn(&str) -> bool,
) -> Result<Option<String>, AppError> {
    let supplied = value.filter(|value| !value.is_empty());

    if anchor.is_none() {
        return match supplied {
            Some(_) => Err(AppError::Validation(
                ANCHOR_PAGE_UNANCHORED_MESSAGE.to_owned(),
            )),
            None => Ok(None),
        };
    }
    if !is_bundle {
        return match supplied {
            Some(_) => Err(AppError::Validation(
                ANCHOR_PAGE_NOT_BUNDLE_MESSAGE.to_owned(),
            )),
            None => Ok(None),
        };
    }

    // `typeof value !== "string" || !value.trim()` — a whitespace-only page is "not supplied".
    let trimmed = supplied.map(js_trim).filter(|value| !value.is_empty());
    let Some(trimmed) = trimmed else {
        return Err(AppError::Validation(
            ANCHOR_PAGE_REQUIRED_MESSAGE.to_owned(),
        ));
    };

    let raw = trimmed.replace('\\', "/");
    if raw.starts_with('/') || is_drive_qualified(&raw) || raw.split('/').any(|part| part == "..") {
        return Err(AppError::Validation(
            ANCHOR_PAGE_TRAVERSAL_MESSAGE.to_owned(),
        ));
    }

    let normalized = posix_normalize(&raw);
    if normalized.is_empty() || normalized == "." {
        return Err(AppError::Validation(
            ANCHOR_PAGE_NOT_A_FILE_MESSAGE.to_owned(),
        ));
    }
    if !html_page_exists(&normalized) {
        return Err(AppError::Validation(ANCHOR_PAGE_MISSING_MESSAGE.to_owned()));
    }
    Ok(Some(normalized))
}

/// `/^[A-Za-z]:\//` — [lib/app.js:26]
fn is_drive_qualified(raw: &str) -> bool {
    let bytes = raw.as_bytes();
    bytes.len() >= 3 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' && bytes[2] == b'/'
}

/// `path.posix.normalize(raw)` for a relative path with no `..` segment (both already rejected).
///
/// Node collapses `.` and empty segments, returns `"."`/`"./"` when nothing is left, and
/// **preserves a trailing slash** — which is why `a/b/` normalizes to `a/b/` and then fails the
/// file lookup rather than silently resolving to `a/b`.
fn posix_normalize(raw: &str) -> String {
    let trailing = raw.ends_with('/');
    let segments: Vec<&str> = raw
        .split('/')
        .filter(|part| !part.is_empty() && *part != ".")
        .collect();
    if segments.is_empty() {
        return if trailing { "./" } else { "." }.to_owned();
    }
    let mut out = segments.join("/");
    if trailing {
        out.push('/');
    }
    out
}

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
    /// The output of [`validate_anchor_page`], which the route runs first [lib/app.js:570].
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
    let normalized = normalize_anchor(anchor, input.submission.anchor_path.as_deref())?;
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

// ---------------------------------------------------------------------------
// Reads
// ---------------------------------------------------------------------------

/// `getFeedback(id)` — [lib/feedback.js:116-118]
///
/// # Errors
/// [`AppError::Unavailable`] if the query fails.
pub fn get(conn: &Connection, id: &FeedbackId) -> Result<Option<Feedback>, AppError> {
    conn.query_row(
        &format!("SELECT {COLUMNS} FROM feedback WHERE id = ?1"),
        params![id.0],
        read_row,
    )
    .optional()
    .map_err(|error| failed("read feedback", &error))
}

/// The minimal lookup a route may perform *before* artifact authorization (contract §"Frozen
/// domain and access types"): id, artifact, org and nothing else — no body, viewer email, or
/// anchor.
///
/// # Errors
/// [`AppError::Unavailable`] if the query fails.
pub fn feedback_ref(conn: &Connection, id: &FeedbackId) -> Result<Option<FeedbackRef>, AppError> {
    conn.query_row(
        "SELECT id, artifact_id, org FROM feedback WHERE id = ?1",
        params![id.0],
        |row| {
            Ok(FeedbackRef {
                id: FeedbackId(row.get(0)?),
                artifact_id: ArtifactId(row.get(1)?),
                org: OrgId(row.get(2)?),
            })
        },
    )
    .optional()
    .map_err(|error| failed("read feedback reference", &error))
}

/// `listForArtifact(artifactId)` — [lib/feedback.js:50-52,112-114].
///
/// Frozen order: unresolved first, then oldest-first, then id — the thread reading order the
/// viewer shell renders.
///
/// # Errors
/// [`AppError::Unavailable`] if the query fails.
pub fn list_for_artifact(
    conn: &Connection,
    artifact_id: &ArtifactId,
) -> Result<Vec<Feedback>, AppError> {
    query(
        conn,
        &format!(
            "SELECT {COLUMNS} FROM feedback WHERE artifact_id = ?1 \
             ORDER BY (resolved_at IS NOT NULL), created_at ASC, id ASC"
        ),
        &[&artifact_id.0],
    )
}

/// `listAll(artifactId)` — [lib/feedback.js:153-155]. With an artifact it is
/// [`list_for_artifact`]; without one it is the admin firehose, newest-first.
///
/// # Errors
/// [`AppError::Unavailable`] if the query fails.
pub fn list_all(
    conn: &Connection,
    artifact_id: Option<&ArtifactId>,
) -> Result<Vec<Feedback>, AppError> {
    match artifact_id {
        Some(artifact_id) => list_for_artifact(conn, artifact_id),
        None => query(
            conn,
            &format!(
                "SELECT {COLUMNS} FROM feedback \
                 ORDER BY (resolved_at IS NOT NULL), created_at DESC, id DESC"
            ),
            &[],
        ),
    }
}

/// Organization-wide agent listing for reader and collaborator keys.
pub fn list_for_org(conn: &Connection, org: &OrgId) -> Result<Vec<Feedback>, AppError> {
    query(
        conn,
        &format!(
            "SELECT {COLUMNS} FROM feedback WHERE org = ?1 \
             ORDER BY (resolved_at IS NOT NULL), created_at DESC, id DESC"
        ),
        &[&org.0],
    )
}

/// `listForClient(clientId, artifactId, org)` — [lib/feedback.js:61-79,147-151].
///
/// The `org` filter is not optional cosmetics: `client_id` survives an org move
/// [lib/store.js:478], so a non-admin key that listed by `client_id` alone would keep reading the
/// *new* tenant's feedback bodies, anchors, and verified viewer emails [lib/feedback.js:69-71].
/// `None` is the admin path, which intentionally sees every org.
///
/// # Errors
/// [`AppError::Unavailable`] if the query fails.
pub fn list_for_client(
    conn: &Connection,
    client_id: &ClientId,
    artifact_id: Option<&ArtifactId>,
    org: Option<&OrgId>,
) -> Result<Vec<Feedback>, AppError> {
    let mut sql = format!(
        "SELECT {JOINED_COLUMNS} FROM feedback f \
         JOIN artifacts a ON a.id = f.artifact_id AND a.org = f.org \
         WHERE a.client_id = ?1"
    );
    let mut binds: Vec<&String> = vec![&client_id.0];
    // Node's four prepared statements differ only in these two predicates and their bind order.
    if let Some(artifact_id) = artifact_id {
        sql.push_str(" AND f.artifact_id = ?2");
        binds.push(&artifact_id.0);
    }
    if let Some(org) = org {
        sql.push_str(if artifact_id.is_some() {
            " AND a.org = ?3"
        } else {
            " AND a.org = ?2"
        });
        binds.push(&org.0);
    }
    sql.push_str(" ORDER BY (f.resolved_at IS NOT NULL), f.created_at DESC, f.id DESC");
    query(conn, &sql, &binds)
}

fn query(conn: &Connection, sql: &str, binds: &[&String]) -> Result<Vec<Feedback>, AppError> {
    let mut stmt = conn
        .prepare(sql)
        .map_err(|error| failed("prepare feedback listing", &error))?;
    let rows = stmt
        .query_map(params_from_iter(binds.iter()), read_row)
        .map_err(|error| failed("list feedback", &error))?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(|error| failed("read feedback rows", &error))
}

/// Maps one row of [`COLUMNS`] / [`JOINED_COLUMNS`].
fn read_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Feedback> {
    let viewer_email = row.get::<_, Option<String>>(3)?.map(EmailAddress);
    let author = match row.get::<_, String>(17)?.as_str() {
        "discord" => FeedbackAuthor::Discord {
            external_author_id: row.get(18)?,
            external_author_display: row.get(19)?,
        },
        _ => FeedbackAuthor::Artifact {
            viewer_email: viewer_email.clone().ok_or_else(|| {
                rusqlite::Error::InvalidColumnType(
                    3,
                    "viewer_email".to_owned(),
                    rusqlite::types::Type::Null,
                )
            })?,
        },
    };
    Ok(Feedback {
        id: FeedbackId(row.get(0)?),
        artifact_id: ArtifactId(row.get(1)?),
        org: OrgId(row.get(2)?),
        viewer_email,
        author,
        body: row.get(4)?,
        artifact_revision: row.get::<_, i64>(5)?.unsigned_abs(),
        parent_id: row.get::<_, Option<String>>(6)?.map(FeedbackId),
        anchor_path: row.get(7)?,
        anchor_x: row.get(8)?,
        anchor_y: row.get(9)?,
        anchor_w: row.get(10)?,
        anchor_h: row.get(11)?,
        anchor_approx: row.get::<_, i64>(12)? != 0,
        anchor_page: row.get(13)?,
        created_at: Timestamp(row.get(14)?),
        resolved_at: row.get::<_, Option<String>>(15)?.map(Timestamp),
        resolved_by: row.get(16)?,
        external_created_at: row.get::<_, Option<String>>(20)?.map(Timestamp),
        external_edited_at: row.get::<_, Option<String>>(21)?.map(Timestamp),
        external_deleted_at: row.get::<_, Option<String>>(22)?.map(Timestamp),
    })
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

/// SQL faults are operator-facing and logged; the message never carries row data.
fn failed(operation: &str, error: &rusqlite::Error) -> AppError {
    tracing::error!(operation, error = %error, "feedback persistence failed");
    AppError::Unavailable("database unavailable".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn javascript_trim_covers_zwnbsp_but_not_nel() {
        assert_eq!(js_trim("\u{feff} body \u{feff}"), "body");
        assert_eq!(js_trim("\u{85}body\u{85}"), "\u{85}body\u{85}");
        assert_eq!(js_trim("\u{a0}\t\nbody\r "), "body");
    }

    #[test]
    fn body_length_counts_utf16_code_units() {
        assert_eq!(utf16_len("abc"), 3);
        assert_eq!(utf16_len("\u{1f600}"), 2);
        assert_eq!(slice_utf16("\u{1f600}\u{1f600}", 3), "\u{1f600}");
    }

    #[test]
    fn box_anchor_is_trimmed_to_the_document_edge() {
        let anchor = FeedbackAnchor {
            x: 0.75,
            y: 0.5,
            w: Some(0.5),
            h: Some(0.25),
            approx: false,
        };
        let normalized = normalize_anchor(Some(&anchor), None).expect("in bounds");
        assert_eq!(normalized.anchor_w, Some(0.25));
        assert_eq!(normalized.anchor_h, Some(0.25));
    }

    #[test]
    fn posix_normalize_preserves_a_trailing_slash() {
        assert_eq!(posix_normalize("a/./b"), "a/b");
        assert_eq!(posix_normalize("a//b/"), "a/b/");
        assert_eq!(posix_normalize("."), ".");
        assert_eq!(posix_normalize("./"), "./");
    }
}
