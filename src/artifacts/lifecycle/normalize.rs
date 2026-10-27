//! JavaScript string semantics and metadata normalization.

use super::*;

// ---------------------------------------------------------------------------
// JavaScript string semantics
// ---------------------------------------------------------------------------

/// ECMAScript `\s` / `WhiteSpace` + `LineTerminator`, matching U07's private predicate in
/// `validation.rs`. `String.prototype.trim` and the regex class cover the same set.
const fn is_js_whitespace(value: char) -> bool {
    matches!(
        value,
        '\u{9}'..='\u{d}'
            | '\u{20}'
            | '\u{a0}'
            | '\u{1680}'
            | '\u{2000}'..='\u{200a}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{202f}'
            | '\u{205f}'
            | '\u{3000}'
            | '\u{feff}'
    )
}

/// `String.prototype.slice(0, max)` — UTF-16 code units, not bytes and not `char`s.
///
/// A limit that would split a surrogate pair drops the whole astral character; JavaScript would
/// keep a lone surrogate, which Rust's `String` cannot represent and SQLite would not store
/// faithfully either.
#[must_use]
fn js_slice(value: &str, max_units: usize) -> &str {
    let mut units = 0;
    for (index, character) in value.char_indices() {
        let width = character.len_utf16();
        if units + width > max_units {
            return &value[..index];
        }
        units += width;
    }
    value
}

/// `String(title || "Untitled artifact").slice(0, 200)` — [lib/store.js:196]
const TITLE_MAX_UNITS: usize = 200;
/// `String(description || "").slice(0, 500)` — [lib/store.js:197]
const DESCRIPTION_MAX_UNITS: usize = 500;
/// `String(uploaderLabel || "").slice(0, 60)` — [lib/store.js:195]
const LABEL_MAX_UNITS: usize = 60;
/// `.slice(0, 60)` inside `normalizeCategory` — [lib/store.js:58]
const CATEGORY_MAX_UNITS: usize = 60;
/// `"Untitled artifact"` — [lib/store.js:196]
const DEFAULT_TITLE: &str = "Untitled artifact";
/// `org || "default"` — [lib/store.js:193]
pub(super) const DEFAULT_ORG: &str = "default";

/// `normalizeCategory(value)` = `String(value || "").trim().replace(/\s+/g, " ").slice(0, 60)`
/// — [lib/store.js:57-59]
#[must_use]
pub fn normalize_category(value: &str) -> String {
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
    js_slice(&collapsed, CATEGORY_MAX_UNITS).to_owned()
}

/// `String(title || "Untitled artifact").slice(0, 200)` — [lib/store.js:196]
pub(super) fn normalize_title(value: Option<&str>) -> String {
    let value = value
        .filter(|value| !value.is_empty())
        .unwrap_or(DEFAULT_TITLE);
    js_slice(value, TITLE_MAX_UNITS).to_owned()
}

/// `String(description || "").slice(0, 500)` — [lib/store.js:197]
pub(super) fn normalize_description(value: Option<&str>) -> String {
    js_slice(value.unwrap_or_default(), DESCRIPTION_MAX_UNITS).to_owned()
}

/// `String(uploaderLabel || "").slice(0, 60)` — [lib/store.js:195]
pub(super) fn normalize_label(value: &str) -> String {
    js_slice(value, LABEL_MAX_UNITS).to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slices_by_utf16_code_units_like_javascript() {
        assert_eq!(js_slice("abcdef", 3), "abc");
        assert_eq!(js_slice("abc", 10), "abc");
        // "é" is one UTF-16 unit but two UTF-8 bytes.
        assert_eq!(js_slice("éé", 1), "é");
        // An astral character is two UTF-16 units; a limit of one drops it entirely rather than
        // producing a lone surrogate.
        assert_eq!(js_slice("\u{1f600}x", 1), "");
        assert_eq!(js_slice("\u{1f600}x", 2), "\u{1f600}");
    }

    #[test]
    fn normalizes_categories_like_node() {
        assert_eq!(normalize_category("  design   docs \n"), "design docs");
        assert_eq!(normalize_category(""), "");
        assert_eq!(normalize_category("\u{feff}a\u{a0}b"), "a b");
        assert_eq!(normalize_category(&"x".repeat(100)).len(), 60);
    }

    #[test]
    fn applies_nodes_metadata_defaults() {
        assert_eq!(normalize_title(None), "Untitled artifact");
        assert_eq!(normalize_title(Some("")), "Untitled artifact");
        assert_eq!(normalize_title(Some("Real")), "Real");
        assert_eq!(normalize_description(None), "");
        assert_eq!(normalize_label(&"l".repeat(80)).len(), 60);
    }
}
