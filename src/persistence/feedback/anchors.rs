use crate::error::AppError;
use crate::model::{FeedbackAnchor, FeedbackAnchorV2};

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
/// PBI-084 v2 structured-anchor validation messages. These are intentionally distinct from the
/// legacy geometry errors: a malformed v2 envelope must fail rather than silently persist as v1.
pub const ANCHOR_VERSION_MESSAGE: &str = "Anchor version must be 2.";
pub const ANCHOR_KIND_MESSAGE: &str = "Anchor kind must be \"element\" or \"region\".";
pub const ANCHOR_PATH_V2_MESSAGE: &str = "Anchor path must be a string for version 2 anchors.";
pub const ANCHOR_NODE_ID_MESSAGE: &str = "Anchor nodeId must be a string or null.";
pub const ANCHOR_QUOTE_MESSAGE: &str = "Anchor quote must be a string or null.";
pub const ANCHOR_APPROX_V2_MESSAGE: &str = "Anchor approx must be a boolean for version 2 anchors.";
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
/// PBI-083 bridge cap for `data-artifact-node`.
pub const ANCHOR_NODE_ID_MAX_UTF16: usize = 128;
/// PBI-083 bridge cap for a normalized text quote.
pub const ANCHOR_QUOTE_MAX_UTF16: usize = 240;

/// `Feedback is too long (max ${FEEDBACK_MAX_BODY} characters).` — [lib/feedback.js:87]
#[must_use]
pub fn too_long_message(max_body: u64) -> String {
    format!("Feedback is too long (max {max_body} characters).")
}

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
    pub anchor_kind: Option<String>,
    pub anchor_node_id: Option<String>,
    pub anchor_quote: Option<String>,
}

type NormalizedAnchorV2 = (Option<String>, Option<String>, Option<String>);

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
    normalize_anchor_with_v2(anchor, path, None)
}

/// Extends [`normalize_anchor`] with the v2 structured envelope. Kept separate so existing
/// persistence callers remain source-compatible while v2-aware HTTP paths cannot discard it.
pub fn normalize_anchor_with_v2(
    anchor: Option<&FeedbackAnchor>,
    path: Option<&str>,
    v2: Option<&FeedbackAnchorV2>,
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

    let (anchor_kind, anchor_node_id, anchor_quote) = match v2 {
        None => (None, None, None),
        Some(v2) => normalize_v2(v2)?,
    };

    Ok(NormalizedAnchor {
        anchor_path: path.map(|path| slice_utf16(path, ANCHOR_PATH_MAX_UTF16)),
        anchor_x: Some(anchor.x),
        anchor_y: Some(anchor.y),
        anchor_w,
        anchor_h,
        anchor_approx: anchor.approx,
        anchor_kind,
        anchor_node_id,
        anchor_quote,
    })
}

fn normalize_v2(v2: &FeedbackAnchorV2) -> Result<NormalizedAnchorV2, AppError> {
    if v2.version != Some(2.0) {
        return Err(AppError::Validation(ANCHOR_VERSION_MESSAGE.to_owned()));
    }
    let kind = v2.kind.as_deref();
    if !matches!(kind, Some("element" | "region")) {
        return Err(AppError::Validation(ANCHOR_KIND_MESSAGE.to_owned()));
    }
    if !v2.path_is_string {
        return Err(AppError::Validation(ANCHOR_PATH_V2_MESSAGE.to_owned()));
    }
    if !v2.node_id_is_string_or_null {
        return Err(AppError::Validation(ANCHOR_NODE_ID_MESSAGE.to_owned()));
    }
    if !v2.quote_is_string_or_null {
        return Err(AppError::Validation(ANCHOR_QUOTE_MESSAGE.to_owned()));
    }
    if !v2.approx_is_boolean_or_absent {
        return Err(AppError::Validation(ANCHOR_APPROX_V2_MESSAGE.to_owned()));
    }
    Ok((
        kind.map(ToOwned::to_owned),
        v2.node_id
            .as_deref()
            .map(|value| slice_utf16(value, ANCHOR_NODE_ID_MAX_UTF16)),
        v2.quote
            .as_deref()
            .map(|value| slice_utf16(value, ANCHOR_QUOTE_MAX_UTF16)),
    ))
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
    fn invalid_point_precedes_invalid_box_pair() {
        let anchor = FeedbackAnchor {
            x: f64::NAN,
            y: 0.5,
            w: Some(0.25),
            h: None,
            approx: false,
        };
        assert_eq!(
            normalize_anchor(Some(&anchor), None),
            Err(AppError::Validation(ANCHOR_POINT_MESSAGE.to_owned()))
        );
    }

    #[test]
    fn posix_normalize_preserves_a_trailing_slash() {
        assert_eq!(posix_normalize("a/./b"), "a/b");
        assert_eq!(posix_normalize("a//b/"), "a/b/");
        assert_eq!(posix_normalize("."), ".");
        assert_eq!(posix_normalize("./"), "./");
    }
}
