//! Owned by U11 (terra) — public-share expiry, resolution, listing, and revocation.
//!
//! Node oracle: `lib/shares.js` (whole file), plus the two compositions that consume it —
//! `sharedArtifactOr404` [lib/app.js:106-113] and `revoke_share` [lib/mcp.js:488-498].
//!
//! # The one rule this module exists to enforce
//!
//! `/s/:token` is Access-bypassed: the token *is* the authorization boundary, so a token probe
//! must never become an existence oracle. There are exactly four ways a token can fail to grant
//! access, and all four must be **externally indistinguishable**:
//!
//! | State | How it arises |
//! |---|---|
//! | invalid | no `artifact_shares` row with that token |
//! | expired | `expires_at` is in the past |
//! | revoked | `revoked_at` is set |
//! | stale | the row survives but its artifact is gone or now belongs to another org |
//!
//! Node collapses invalid/expired/revoked in SQL [lib/shares.js:11-14] and stale in the route
//! [lib/app.js:111]. Splitting the decision across two layers is what makes a leak possible: a
//! route that forgets the second half turns a moved or deleted artifact into a distinguishable
//! response. This port collapses all four **inside the persistence result shape**: [`resolve`] is
//! the only function here that accepts a bare token, and it returns `Option<ShareGrant>` — there
//! is no reason code for a caller to accidentally propagate. Conformance asserts the resulting
//! 404s are byte-identical (`share.public-delivery` step 5, `sameAsStep: 3`) and carry
//! `cache-control: no-store` + `x-robots-tag: noindex`.
//!
//! [`revoke`] and [`list_for_artifact`] are artifact-scoped: their callers have already proven
//! access to that artifact (`AuthorizedArtifact`), so their outputs are not a public oracle.
//! Never route an unauthenticated token through them.
//!
//! # Timezone assumption
//!
//! `new Date("2026-05-01T10:00")` — an ISO date-time with **no** offset — is parsed in the
//! *server's local zone* by JavaScript. `time` is compiled without `local-offset` (Cargo.toml is
//! U01-owned), so [`expiry_for`] reads such an input as UTC. That is identical to Node whenever
//! the process runs in UTC, which is what the container image and the conformance harness do.
//! Every other accepted shape (date-only, `Z`, explicit `±HH:MM`) is offset-independent and
//! matches unconditionally. Recorded as a contract-delta request.

use rusqlite::{Connection, OptionalExtension, params};
use time::{Date, Month};

use crate::config::{Clock, IdSource};
use crate::error::AppError;
use crate::model::{
    ArtifactId, CreateShare, OrgId, PublicShare, ShareGrant, ShareToken, Timestamp,
};

/// `expires` is neither keyword nor an ISO-shaped string — [lib/shares.js:29]
pub const EXPIRES_FORMAT_MESSAGE: &str = "expires must be '24h', 'never', or a future ISO date";
/// `expires` parsed to an invalid instant, or to one that is not in the future — [lib/shares.js:33]
pub const EXPIRES_FUTURE_MESSAGE: &str = "expires must be a valid future ISO date";
/// `expires` was a date `Date` silently rolled over (`2027-02-31`) — [lib/shares.js:38]
pub const EXPIRES_CALENDAR_MESSAGE: &str = "expires is not a real calendar date";

/// The `"24h"` keyword's offset — [lib/shares.js:26]
pub const TWENTY_FOUR_HOURS_MILLIS: i64 = 24 * 60 * 60 * 1000;

/// Julian day number of 1970-01-01, used to convert a [`Date`] to a Unix day count.
const UNIX_EPOCH_JULIAN_DAY: i32 = 2_440_588;

const MILLIS_PER_DAY: i64 = 86_400_000;

/// `insertStmt` — [lib/shares.js:7-10]
const INSERT_SQL: &str = "INSERT INTO artifact_shares (token, artifact_id, org, created_by, expires_at) \
     VALUES (?1, ?2, ?3, ?4, ?5)";

/// Node's `resolveStmt` [lib/shares.js:11-14] **inner-joined** with the artifact-existence and
/// org-match test both Node call sites apply immediately afterwards ([lib/app.js:110-111],
/// [lib/mcp.js:491-492]).
///
/// `getArtifactMeta` is `SELECT * FROM artifacts WHERE id = ?` [lib/store.js:111], so
/// `(!meta || meta.org !== share.org)` is exactly "no `artifacts` row with this `(id, org)`".
const RESOLVE_SQL: &str = "SELECT s.artifact_id, s.org FROM artifact_shares s \
     JOIN artifacts a ON a.id = s.artifact_id AND a.org = s.org \
     WHERE s.token = ?1 AND s.revoked_at IS NULL \
       AND (s.expires_at IS NULL OR julianday(s.expires_at) > julianday('now'))";

/// `listStmt` — [lib/shares.js:15-19]. The `ORDER BY` is frozen: HTTP/MCP conformance compares
/// ordered JSON arrays.
const LIST_SQL: &str = "SELECT token, expires_at, created_at, created_by FROM artifact_shares \
     WHERE artifact_id = ?1 AND revoked_at IS NULL \
       AND (expires_at IS NULL OR julianday(expires_at) > julianday('now')) \
     ORDER BY created_at DESC, token DESC";

/// `revokeStmt` — [lib/shares.js:20-23]
const REVOKE_SQL: &str = "UPDATE artifact_shares SET revoked_at = datetime('now') \
     WHERE artifact_id = ?1 AND token = ?2 AND revoked_at IS NULL";

/// `dropSharesOnMoveStmt` — [lib/store.js:144], run inside the org move [lib/store.js:476].
const DROP_ON_MOVE_SQL: &str = "DELETE FROM artifact_shares WHERE artifact_id = ?1";

/// `expiryFor(expires)` — [lib/shares.js:25-41].
///
/// Returns the value stored in `artifact_shares.expires_at`: `None` for `"never"`, otherwise the
/// `Date.prototype.toISOString()` rendering (`YYYY-MM-DDTHH:MM:SS.mmmZ`) of the requested instant.
///
/// Evaluation order is load-bearing and reproduced exactly: keywords, then shape, then
/// validity/futureness, then the impossible-calendar guard.
///
/// # Errors
/// [`AppError::Validation`] carrying [`EXPIRES_FORMAT_MESSAGE`], [`EXPIRES_FUTURE_MESSAGE`], or
/// [`EXPIRES_CALENDAR_MESSAGE`] — the exact strings the Node routes surface as a 400 body
/// [lib/app.js:651] and as an MCP tool error [lib/mcp.js:478].
pub fn expiry_for(clock: &dyn Clock, expires: &str) -> Result<Option<String>, AppError> {
    if expires == "24h" {
        let millis = clock
            .now_unix_millis()
            .checked_add(TWENTY_FOUR_HOURS_MILLIS)
            .ok_or(AppError::Internal)?;
        return Ok(Some(render_iso(millis)?));
    }
    if expires == "never" {
        return Ok(None);
    }

    // `typeof expires !== "string" || !/…/.test(expires)` — [lib/shares.js:28]. A missing
    // `expires` reaches Rust as an empty string, which fails the same shape test with the same
    // message, so the frozen `CreateShare { expires: String }` loses nothing.
    let parsed = parse_iso_shape(expires)
        .ok_or_else(|| AppError::Validation(EXPIRES_FORMAT_MESSAGE.to_owned()))?;

    // `Number.isNaN(date.getTime()) || date.getTime() <= Date.now()` — [lib/shares.js:32]. Both
    // failures share one message, so an unparseable value is not distinguishable from a past one.
    let millis = parsed
        .to_unix_millis()
        .filter(|millis| *millis > clock.now_unix_millis())
        .ok_or_else(|| AppError::Validation(EXPIRES_FUTURE_MESSAGE.to_owned()))?;

    let rendered = render_iso(millis)?;

    // `2027-02-31` parses (Date rolls it into March) and would silently outlive the request —
    // [lib/shares.js:37-39]. Only the date-only shape is checked, exactly as Node does.
    if parsed.date_only && rendered.get(..10) != Some(expires) {
        return Err(AppError::Validation(EXPIRES_CALENDAR_MESSAGE.to_owned()));
    }
    Ok(Some(rendered))
}

/// `create({ artifactId, org, createdBy, expires })` — [lib/shares.js:43-48].
///
/// `created_at` is left to the column default (`datetime('now')`), as in Node; the returned record
/// carries only `token` + `expires_at`, which is what both call sites serialize.
///
/// # Errors
/// The [`expiry_for`] validation errors, [`AppError::Internal`] if the id source fails, or
/// [`AppError::Unavailable`] if the insert fails.
pub fn create(
    conn: &Connection,
    ids: &dyn IdSource,
    clock: &dyn Clock,
    artifact_id: &ArtifactId,
    org: &OrgId,
    request: &CreateShare,
) -> Result<PublicShare, AppError> {
    // Node computes the expiry *before* minting the token, so a rejected `expires` burns no id.
    let expires_at = expiry_for(clock, &request.expires)?;
    let token = ids.share_token()?;
    conn.execute(
        INSERT_SQL,
        params![
            token.0,
            artifact_id.0,
            org.0,
            request.created_by,
            expires_at
        ],
    )
    .map_err(|error| failed("insert public share", &error))?;
    Ok(PublicShare {
        token,
        expires_at: expires_at.map(Timestamp),
        created_at: None,
        created_by: None,
    })
}

/// `resolve(token)` composed with its callers' artifact check — see the module docs.
///
/// `None` is returned for **invalid, expired, revoked, and stale** tokens alike. That collapse is
/// the security property: a caller cannot tell the cases apart, so it cannot leak them.
///
/// # Errors
/// [`AppError::Unavailable`] if the query fails.
pub fn resolve(conn: &Connection, token: &ShareToken) -> Result<Option<ShareGrant>, AppError> {
    conn.query_row(RESOLVE_SQL, params![token.0], |row| {
        Ok(ShareGrant {
            artifact_id: ArtifactId(row.get(0)?),
            org: OrgId(row.get(1)?),
        })
    })
    .optional()
    .map_err(|error| failed("resolve share token", &error))
}

/// `listForArtifact(artifactId)` — [lib/shares.js:54-56]. Revoked and expired rows stay hidden.
///
/// # Errors
/// [`AppError::Unavailable`] if the query fails.
pub fn list_for_artifact(
    conn: &Connection,
    artifact_id: &ArtifactId,
) -> Result<Vec<PublicShare>, AppError> {
    let mut stmt = conn
        .prepare(LIST_SQL)
        .map_err(|error| failed("prepare share listing", &error))?;
    let rows = stmt
        .query_map(params![artifact_id.0], |row| {
            Ok(PublicShare {
                token: ShareToken(row.get(0)?),
                expires_at: row.get::<_, Option<String>>(1)?.map(Timestamp),
                created_at: row.get::<_, Option<String>>(2)?.map(Timestamp),
                created_by: row.get(3)?,
            })
        })
        .map_err(|error| failed("list public shares", &error))?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(|error| failed("read public shares", &error))
}

/// `revoke(artifactId, token)` — [lib/shares.js:58-60]. `true` when a live row was revoked.
///
/// Artifact-scoped by design: an already-revoked token reports `false` while a merely expired one
/// reports `true`, which is only safe because every caller has proven access to `artifact_id`
/// first ([lib/app.js:663-666], [lib/mcp.js:490-497]).
///
/// # Errors
/// [`AppError::Unavailable`] if the update fails.
pub fn revoke(
    conn: &Connection,
    artifact_id: &ArtifactId,
    token: &ShareToken,
) -> Result<bool, AppError> {
    let changed = conn
        .execute(REVOKE_SQL, params![artifact_id.0, token.0])
        .map_err(|error| failed("revoke public share", &error))?;
    Ok(changed > 0)
}

/// `dropSharesOnMoveStmt.run(id)` — [lib/store.js:144,476].
///
/// An org move **destroys** every share link instead of carrying it into the new tenant: a link
/// minted under the old org must not keep serving content that now belongs to somebody else. The
/// composite FK `(artifact_id, org) REFERENCES artifacts(id, org)` would otherwise be violated by
/// the move itself. U08 calls this inside the same transaction as the `artifacts` update.
///
/// # Errors
/// [`AppError::Unavailable`] if the delete fails.
pub fn revoke_all_for_artifact(
    conn: &Connection,
    artifact_id: &ArtifactId,
) -> Result<usize, AppError> {
    conn.execute(DROP_ON_MOVE_SQL, params![artifact_id.0])
        .map_err(|error| failed("drop public shares on move", &error))
}

// ---------------------------------------------------------------------------
// ECMAScript date subset
// ---------------------------------------------------------------------------

/// The pieces of an `expires` string that passed the [lib/shares.js:28] shape test.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct IsoShape {
    year: i32,
    month: u8,
    day: u8,
    hour: u8,
    minute: u8,
    second: u8,
    millisecond: u16,
    /// The written offset, still unvalidated: `+24:00` is *shape*-legal (the regular expression
    /// only counts digits) and must therefore fail as an invalid instant, not as a bad shape.
    offset: Option<IsoOffset>,
    /// True for the bare `YYYY-MM-DD` form — the only shape Node calendar-checks.
    date_only: bool,
}

/// A written `Z` or `±HH[:]MM` suffix.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct IsoOffset {
    negative: bool,
    hours: u8,
    minutes: u8,
}

impl IsoShape {
    /// V8's field validation plus the epoch arithmetic `Date` performs, or `None` for the values
    /// `new Date(…)` reports as `Invalid Date`.
    fn to_unix_millis(self) -> Option<i64> {
        // Empirically pinned against V8 (see `u11_node_parity`): month 1-12, day 1-31 (rolling
        // over into the following month), hour <= 24 with 24 permitted only as exact midnight,
        // minute/second <= 59, offset |hh| <= 23 and |mm| <= 59.
        if !(1..=12).contains(&self.month) || !(1..=31).contains(&self.day) {
            return None;
        }
        if self.hour > 24
            || (self.hour == 24 && (self.minute > 0 || self.second > 0 || self.millisecond > 0))
            || self.minute > 59
            || self.second > 59
        {
            return None;
        }
        let offset_minutes = match self.offset {
            None => 0,
            Some(offset) => {
                if offset.hours > 23 || offset.minutes > 59 {
                    return None;
                }
                let total = i64::from(offset.hours) * 60 + i64::from(offset.minutes);
                if offset.negative { -total } else { total }
            }
        };

        let month = Month::try_from(self.month).ok()?;
        let first = Date::from_calendar_date(self.year, month, 1).ok()?;
        // `Date` rolls an over-long day into the following month rather than rejecting it; the
        // calendar guard in `expiry_for` turns that back into an error for date-only input.
        let days = i64::from(first.to_julian_day() - UNIX_EPOCH_JULIAN_DAY)
            .checked_add(i64::from(self.day) - 1)?;

        let time_millis = i64::from(self.hour) * 3_600_000
            + i64::from(self.minute) * 60_000
            + i64::from(self.second) * 1_000
            + i64::from(self.millisecond);
        days.checked_mul(MILLIS_PER_DAY)?
            .checked_add(time_millis)?
            .checked_sub(offset_minutes.checked_mul(60_000)?)
    }
}

/// The literal regular expression at [lib/shares.js:28], hand-rolled:
/// `^\d{4}-\d{2}-\d{2}(?:T\d{2}:\d{2}(?::\d{2}(?:\.\d{1,3})?)?(?:Z|[+-]\d{2}:?\d{2})?)?$`
///
/// Shape only — out-of-range field *values* are rejected later by [`IsoShape::to_unix_millis`],
/// which is what keeps Node's two distinct error messages in the right order.
fn parse_iso_shape(value: &str) -> Option<IsoShape> {
    let bytes = value.as_bytes();
    let mut cursor = 0_usize;

    let year = take_digits(bytes, &mut cursor, 4)?;
    expect(bytes, &mut cursor, b'-')?;
    let month = take_digits(bytes, &mut cursor, 2)?;
    expect(bytes, &mut cursor, b'-')?;
    let day = take_digits(bytes, &mut cursor, 2)?;

    let mut shape = IsoShape {
        year: i32::try_from(year).ok()?,
        month: u8::try_from(month).ok()?,
        day: u8::try_from(day).ok()?,
        hour: 0,
        minute: 0,
        second: 0,
        millisecond: 0,
        offset: None,
        date_only: true,
    };

    if cursor == bytes.len() {
        return Some(shape);
    }

    expect(bytes, &mut cursor, b'T')?;
    shape.date_only = false;
    shape.hour = u8::try_from(take_digits(bytes, &mut cursor, 2)?).ok()?;
    expect(bytes, &mut cursor, b':')?;
    shape.minute = u8::try_from(take_digits(bytes, &mut cursor, 2)?).ok()?;

    if bytes.get(cursor) == Some(&b':') {
        cursor += 1;
        shape.second = u8::try_from(take_digits(bytes, &mut cursor, 2)?).ok()?;
        if bytes.get(cursor) == Some(&b'.') {
            cursor += 1;
            shape.millisecond = take_fraction(bytes, &mut cursor)?;
        }
    }

    match bytes.get(cursor) {
        None => Some(shape),
        Some(b'Z') => {
            cursor += 1;
            shape.offset = Some(IsoOffset {
                negative: false,
                hours: 0,
                minutes: 0,
            });
            (cursor == bytes.len()).then_some(shape)
        }
        Some(sign @ (b'+' | b'-')) => {
            let negative = *sign == b'-';
            cursor += 1;
            let hours = take_digits(bytes, &mut cursor, 2)?;
            if bytes.get(cursor) == Some(&b':') {
                cursor += 1;
            }
            let minutes = take_digits(bytes, &mut cursor, 2)?;
            if cursor != bytes.len() {
                return None;
            }
            shape.offset = Some(IsoOffset {
                negative,
                hours: u8::try_from(hours).ok()?,
                minutes: u8::try_from(minutes).ok()?,
            });
            Some(shape)
        }
        Some(_) => None,
    }
}

/// `\.\d{1,3}` right-padded to milliseconds: `.5` is 500 ms, `.05` is 50 ms, `.123` is 123 ms.
fn take_fraction(bytes: &[u8], cursor: &mut usize) -> Option<u16> {
    let start = *cursor;
    while *cursor < bytes.len() && bytes[*cursor].is_ascii_digit() && *cursor - start < 3 {
        *cursor += 1;
    }
    let digits = *cursor - start;
    if digits == 0 {
        return None;
    }
    let mut millis = 0_u16;
    for index in 0..3_usize {
        let digit = if index < digits {
            u16::from(bytes[start + index] - b'0')
        } else {
            0
        };
        millis = millis * 10 + digit;
    }
    Some(millis)
}

fn take_digits(bytes: &[u8], cursor: &mut usize, count: usize) -> Option<u32> {
    let end = cursor.checked_add(count)?;
    let slice = bytes.get(*cursor..end)?;
    if !slice.iter().all(u8::is_ascii_digit) {
        return None;
    }
    *cursor = end;
    slice.iter().try_fold(0_u32, |acc, byte| {
        acc.checked_mul(10)?.checked_add(u32::from(byte - b'0'))
    })
}

fn expect(bytes: &[u8], cursor: &mut usize, wanted: u8) -> Option<()> {
    if bytes.get(*cursor) == Some(&wanted) {
        *cursor += 1;
        Some(())
    } else {
        None
    }
}

/// `Date.prototype.toISOString()` for the four-digit-year range this module can produce:
/// `YYYY-MM-DDTHH:MM:SS.mmmZ`.
///
/// # Errors
/// [`AppError::Internal`] if the instant is outside the representable range.
pub fn render_iso(millis: i64) -> Result<String, AppError> {
    let days = millis.div_euclid(MILLIS_PER_DAY);
    let within_day = millis.rem_euclid(MILLIS_PER_DAY);
    let julian = i32::try_from(days)
        .ok()
        .and_then(|days| days.checked_add(UNIX_EPOCH_JULIAN_DAY))
        .ok_or(AppError::Internal)?;
    let date = Date::from_julian_day(julian).map_err(|_| AppError::Internal)?;
    Ok(format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        date.year(),
        u8::from(date.month()),
        date.day(),
        within_day / 3_600_000,
        (within_day / 60_000) % 60,
        (within_day / 1_000) % 60,
        within_day % 1_000
    ))
}

/// SQL faults are operator-facing and logged; the message never carries row data.
fn failed(operation: &str, error: &rusqlite::Error) -> AppError {
    tracing::error!(operation, error = %error, "share persistence failed");
    AppError::Unavailable("database unavailable".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::FixedClock;

    #[test]
    fn never_has_no_expiry_and_24h_is_exactly_one_day_out() {
        let clock = FixedClock::default();
        assert_eq!(expiry_for(&clock, "never"), Ok(None));
        assert_eq!(
            expiry_for(&clock, "24h"),
            Ok(Some("2026-01-02T00:00:00.000Z".to_owned()))
        );
    }

    #[test]
    fn rejects_rolled_over_calendar_dates() {
        let clock = FixedClock::default();
        assert_eq!(
            expiry_for(&clock, "2027-02-31"),
            Err(AppError::Validation(EXPIRES_CALENDAR_MESSAGE.to_owned()))
        );
    }

    #[test]
    fn renders_the_javascript_iso_shape() {
        assert_eq!(render_iso(0).as_deref(), Ok("1970-01-01T00:00:00.000Z"));
        assert_eq!(
            render_iso(1_767_225_600_123).as_deref(),
            Ok("2026-01-01T00:00:00.123Z")
        );
    }
}
