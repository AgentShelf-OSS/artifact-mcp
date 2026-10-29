use crate::error::AppError;

// ---------------------------------------------------------------------------
// Shared JavaScript string semantics
// ---------------------------------------------------------------------------

/// The exact set matched by JavaScript's `\s` and stripped by `String.prototype.trim`.
///
/// ECMA-262 defines both as `WhiteSpace ∪ LineTerminator`. This is **not** Rust's
/// `char::is_whitespace` (Unicode `White_Space`): it includes U+FEFF (`<ZWNBSP>`) and excludes
/// U+0085 (`<NEL>`). Getting this wrong changes which inputs normalize to the empty string.
#[must_use]
pub fn is_js_whitespace(value: char) -> bool {
    matches!(
        value,
        '\u{9}'          // <TAB>
            | '\u{b}'    // <VT>
            | '\u{c}'    // <FF>
            | '\u{20}'   // <SP>
            | '\u{a0}'   // <NBSP>
            | '\u{feff}' // <ZWNBSP>
            | '\u{a}'    // <LF>
            | '\u{d}'    // <CR>
            | '\u{2028}' // <LS>
            | '\u{2029}' // <PS>
    ) || matches!(
        value,
        // <USP>: the Unicode `Space_Separator` category, minus U+0020/U+00A0 above.
        '\u{1680}' | '\u{2000}'..='\u{200a}' | '\u{202f}' | '\u{205f}' | '\u{3000}'
    )
}

/// `String.prototype.trim()`.
#[must_use]
pub fn js_trim(value: &str) -> &str {
    value.trim_matches(is_js_whitespace)
}

/// Length of `value` in UTF-16 code units, which is what JavaScript's `.length` reports.
#[must_use]
pub fn utf16_len(value: &str) -> usize {
    value.chars().map(char::len_utf16).sum()
}

/// `value.slice(0, max_units)` measured in UTF-16 code units.
///
/// See the divergence note in the module docs: a character that straddles the cap is dropped
/// rather than truncated to a lone surrogate.
#[must_use]
pub fn truncate_utf16(value: &str, max_units: usize) -> String {
    let mut units = 0;
    let mut end = value.len();
    for (index, character) in value.char_indices() {
        let width = character.len_utf16();
        if units + width > max_units {
            end = index;
            break;
        }
        units += width;
    }
    value[..end].to_owned()
}

// ---------------------------------------------------------------------------
// Validation and normalization — [lib/orgs.js:29-51]
// ---------------------------------------------------------------------------

/// Longest accepted org id: `/^[a-z0-9][a-z0-9._-]{0,40}$/i` — [lib/orgs.js:29]
pub const ORG_NAME_MAX_LENGTH: usize = 41;

/// `label.trim().slice(0, 80)` — [lib/orgs.js:112]
pub const ORG_LABEL_MAX_LENGTH: usize = 80;

/// `normCategory` truncation — [lib/orgs.js:50]
pub const CATEGORY_MAX_LENGTH: usize = 60;

/// `email.length > 254` — [lib/orgs.js:40]
pub const EMAIL_MAX_LENGTH: usize = 254;

/// `local.length <= 64` — [lib/orgs.js:45]
pub const EMAIL_LOCAL_MAX_LENGTH: usize = 64;

/// `domain.length <= 253` — [lib/orgs.js:47]
pub const EMAIL_DOMAIN_MAX_LENGTH: usize = 253;

/// The org id reserved for the administrator pseudo-tenant — [lib/orgs.js:114]
pub const RESERVED_ORG_NAME: &str = "admin";

/// `ORG_RE = /^[a-z0-9][a-z0-9._-]{0,40}$/i` — [lib/orgs.js:29]
#[must_use]
pub fn is_valid_org_name(value: &str) -> bool {
    let mut characters = value.chars();
    let Some(first) = characters.next() else {
        return false;
    };
    first.is_ascii_alphanumeric()
        && characters.clone().count() < ORG_NAME_MAX_LENGTH
        && characters.all(is_org_name_character)
}

fn is_org_name_character(value: char) -> bool {
    value.is_ascii_alphanumeric() || matches!(value, '.' | '_' | '-')
}

/// `DOMAIN_RE = /^[a-z0-9]([a-z0-9-]*[a-z0-9])?(\.[a-z0-9]([a-z0-9-]*[a-z0-9])?)+$/i`
/// — [lib/orgs.js:30]
///
/// The trailing `+` makes at least one dot mandatory, so a bare host name is never a domain.
#[must_use]
pub fn is_valid_domain(value: &str) -> bool {
    let mut labels = value.split('.');
    let first = labels.next().unwrap_or_default();
    let mut count = 0_usize;
    for label in labels {
        if !is_valid_domain_label(label) {
            return false;
        }
        count += 1;
    }
    count >= 1 && is_valid_domain_label(first)
}

/// One `[a-z0-9]([a-z0-9-]*[a-z0-9])?` label: alphanumeric ends, hyphens allowed inside.
fn is_valid_domain_label(label: &str) -> bool {
    let bytes = label.as_bytes();
    match (bytes.first(), bytes.last()) {
        (Some(first), Some(last)) => {
            first.is_ascii_alphanumeric()
                && last.is_ascii_alphanumeric()
                && bytes
                    .iter()
                    .all(|byte| byte.is_ascii_alphanumeric() || *byte == b'-')
        }
        _ => false,
    }
}

/// ``EMAIL_LOCAL_RE = /^[a-z0-9!#$%&'*+/=?^_`{|}~.-]+$/i`` — [lib/orgs.js:31]
fn is_valid_email_local(local: &str) -> bool {
    !local.is_empty()
        && local.chars().all(|character| {
            character.is_ascii_alphanumeric()
                || matches!(
                    character,
                    '!' | '#'
                        | '$'
                        | '%'
                        | '&'
                        | '\''
                        | '*'
                        | '+'
                        | '/'
                        | '='
                        | '?'
                        | '^'
                        | '_'
                        | '`'
                        | '{'
                        | '|'
                        | '}'
                        | '~'
                        | '.'
                        | '-'
                )
        })
}

/// `validEmail(email)` — [lib/orgs.js:39-48]. Expects an already-normalized address.
#[must_use]
pub fn is_valid_email(email: &str) -> bool {
    if email.is_empty() || utf16_len(email) > EMAIL_MAX_LENGTH {
        return false;
    }
    if email.chars().any(is_js_whitespace) {
        return false;
    }
    let Some(at) = email.find('@') else {
        return false;
    };
    // `at <= 0 || at !== email.lastIndexOf("@")` — an empty local part or a second `@`.
    if at == 0 || email.rfind('@') != Some(at) {
        return false;
    }
    let (local, domain) = (&email[..at], &email[at + 1..]);
    utf16_len(local) <= EMAIL_LOCAL_MAX_LENGTH
        && is_valid_email_local(local)
        && !local.starts_with('.')
        && !local.ends_with('.')
        && !local.contains("..")
        && utf16_len(domain) <= EMAIL_DOMAIN_MAX_LENGTH
        && is_valid_domain(domain)
}

/// `HEX_RE = /^#(?:[0-9a-fA-F]{3}|[0-9a-fA-F]{6})$/` — [lib/orgs.js:10]
#[must_use]
pub fn is_valid_color(value: &str) -> bool {
    let Some(digits) = value.strip_prefix('#') else {
        return false;
    };
    matches!(digits.len(), 3 | 6) && digits.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// `String(name || "").trim()` — the org id normalization used by every lookup
/// ([lib/orgs.js:54] and friends). Deliberately does **not** lowercase.
#[must_use]
pub fn norm_org(value: &str) -> String {
    js_trim(value).to_owned()
}

/// `normDomain` — [lib/orgs.js:33-35]
#[must_use]
pub fn norm_domain(value: &str) -> String {
    js_trim(value).to_lowercase()
}

/// `normEmail` — [lib/orgs.js:36-38]
#[must_use]
pub fn norm_email(value: &str) -> String {
    js_trim(value).to_lowercase()
}

/// `normCategory` — [lib/orgs.js:49-51]: trim, collapse whitespace runs to one space, cap at 60.
#[must_use]
pub fn norm_category(value: &str) -> String {
    let trimmed = js_trim(value);
    let mut collapsed = String::with_capacity(trimmed.len());
    let mut in_whitespace = false;
    for character in trimmed.chars() {
        if is_js_whitespace(character) {
            if !in_whitespace {
                collapsed.push(' ');
                in_whitespace = true;
            }
        } else {
            collapsed.push(character);
            in_whitespace = false;
        }
    }
    truncate_utf16(&collapsed, CATEGORY_MAX_LENGTH)
}

// ---------------------------------------------------------------------------
// Error messages — one helper per Node `throw`
// ---------------------------------------------------------------------------

/// `Unknown organization "<org>".` — [lib/orgs.js:102], [lib/orgs.js:136], [lib/orgs.js:153],
/// [lib/orgs.js:172]
#[must_use]
pub fn unknown_org_message(org: &str) -> String {
    format!("Unknown organization \"{org}\".")
}

pub(super) fn unknown_org(org: &str) -> AppError {
    AppError::Validation(unknown_org_message(org))
}

/// `"<domain>" is not a valid email domain.` — [lib/orgs.js:118], [lib/orgs.js:137]
pub(super) fn invalid_domain(domain: &str) -> AppError {
    AppError::Validation(format!("\"{domain}\" is not a valid email domain."))
}

/// [lib/orgs.js:140]
pub(super) fn domain_taken(domain: &str, owner: &str, same_org: bool) -> AppError {
    AppError::Validation(if same_org {
        format!("\"{domain}\" is already on this org.")
    } else {
        format!("Domain \"{domain}\" is already mapped to \"{owner}\".")
    })
}

/// [lib/orgs.js:157-159]
pub(super) fn email_taken(email: &str, owner: &str, same_org: bool) -> AppError {
    AppError::Validation(if same_org {
        format!("\"{email}\" is already on this org.")
    } else {
        format!("Email \"{email}\" is already mapped to \"{owner}\".")
    })
}

/// Reports a SQLite failure without leaking any query detail to the caller.
pub(super) fn database_failure(operation: &str, error: &rusqlite::Error) -> AppError {
    tracing::error!(operation, error = %error, "organization persistence failed");
    AppError::Internal
}
