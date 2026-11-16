//! Organization display-name validation.

use crate::error::AppError;

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

pub fn normalize_display_name(value: &str) -> Result<String, AppError> {
    if value.chars().any(char::is_control) || js_trim(value).chars().count() > 40 {
        return Err(AppError::Validation(
            "display_name must be a single line of at most 40 characters.".into(),
        ));
    }
    Ok(js_trim(value).to_owned())
}
