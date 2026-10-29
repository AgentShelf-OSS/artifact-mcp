//! The JavaScript semantics that the Node release scripts relied on.
//!
//! The release evidence files were written with `JSON.stringify(value, null, 2)` and checked
//! with `JSON.parse`, strict equality, `Number(string)` and `String(value)`. This module
//! reproduces those operations so the Rust commands write the same bytes and reach the same
//! decisions:
//!
//! - [`Js`] keeps object keys in insertion order. `JSON.parse` keeps the first position and the
//!   last value of a duplicated key.
//! - [`stringify()`] and [`stringify_pretty`] follow the ECMAScript `JSON.stringify` algorithm,
//!   including number formatting, string escapes and the integer-key-first property order.
//! - [`string_to_number`] is `Number(string)`; [`to_js_string`] is `String(value)`.

use std::cmp::Ordering;
use std::fmt::{self, Write as _};

use serde::de::{Deserialize, Deserializer, MapAccess, SeqAccess, Visitor};

/// A parsed or constructed JSON value with JavaScript property order.
#[derive(Clone, Debug, PartialEq)]
pub enum Js {
    Null,
    Bool(bool),
    Number(f64),
    String(String),
    Array(Vec<Js>),
    Object(Vec<(String, Js)>),
}

impl Js {
    /// A string value.
    pub fn str(value: impl Into<String>) -> Self {
        Self::String(value.into())
    }

    /// An object with properties in the given order.
    pub fn object<const N: usize>(entries: [(&str, Js); N]) -> Self {
        Self::Object(
            entries
                .into_iter()
                .map(|(key, value)| (key.to_owned(), value))
                .collect(),
        )
    }

    /// Append (or replace in place) one property, like `object.key = value`.
    pub fn set(&mut self, key: &str, value: Js) {
        if let Self::Object(entries) = self {
            match entries.iter_mut().find(|(existing, _)| existing == key) {
                Some(slot) => slot.1 = value,
                None => entries.push((key.to_owned(), value)),
            }
        }
    }

    /// `value.key` for an own property of an object; `None` is `undefined`.
    pub fn get(&self, key: &str) -> Option<&Js> {
        match self {
            Self::Object(entries) => entries
                .iter()
                .find(|(existing, _)| existing == key)
                .map(|(_, value)| value),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::String(value) => Some(value),
            _ => None,
        }
    }

    pub fn as_array(&self) -> Option<&[Js]> {
        match self {
            Self::Array(items) => Some(items),
            _ => None,
        }
    }
}

/// `value?.key`: optional chaining over a possibly undefined value.
pub fn get<'a>(value: Option<&'a Js>, key: &str) -> Option<&'a Js> {
    value.and_then(|value| value.get(key))
}

/// `typeof value === "string"`.
pub fn string(value: Option<&Js>) -> Option<&str> {
    value.and_then(Js::as_str)
}

/// JavaScript truthiness of a possibly undefined value.
pub fn truthy(value: Option<&Js>) -> bool {
    match value {
        None | Some(Js::Null) => false,
        Some(Js::Bool(value)) => *value,
        Some(Js::Number(value)) => *value != 0.0 && !value.is_nan(),
        Some(Js::String(value)) => !value.is_empty(),
        Some(Js::Array(_) | Js::Object(_)) => true,
    }
}

/// `Number.MAX_SAFE_INTEGER`.
const MAX_SAFE_INTEGER: f64 = 9_007_199_254_740_991.0;

/// `Number.isSafeInteger(value)` for a JavaScript number.
pub fn is_safe_integer_number(value: f64) -> bool {
    value.is_finite() && value.trunc() == value && value.abs() <= MAX_SAFE_INTEGER
}

/// `Number.isSafeInteger(value)` for a possibly undefined value; returns the number.
pub fn safe_integer(value: Option<&Js>) -> Option<f64> {
    match value {
        Some(Js::Number(number)) if is_safe_integer_number(*number) => Some(*number),
        _ => None,
    }
}

/// `String(value)` (also a template-literal substitution); `None` is `"undefined"`.
pub fn to_js_string(value: Option<&Js>) -> String {
    match value {
        None => "undefined".to_owned(),
        Some(Js::Null) => "null".to_owned(),
        Some(Js::Bool(value)) => value.to_string(),
        Some(Js::Number(value)) => number_to_string(*value),
        Some(Js::String(value)) => value.clone(),
        // Array.prototype.join writes null and undefined elements as empty strings.
        Some(Js::Array(items)) => items
            .iter()
            .map(|item| match item {
                Js::Null => String::new(),
                other => to_js_string(Some(other)),
            })
            .collect::<Vec<_>>()
            .join(","),
        Some(Js::Object(_)) => "[object Object]".to_owned(),
    }
}

/// ECMAScript `Number::toString(10)`.
pub fn number_to_string(value: f64) -> String {
    if value.is_nan() {
        return "NaN".to_owned();
    }
    if value == 0.0 {
        return "0".to_owned();
    }
    if value.is_infinite() {
        return if value > 0.0 { "Infinity" } else { "-Infinity" }.to_owned();
    }
    if value < 0.0 {
        return format!("-{}", number_to_string(-value));
    }
    // `{:e}` writes the shortest round-trip digits: "d[.ddd]e[-]x".
    let formatted = format!("{value:e}");
    let (mantissa, exponent) = formatted
        .split_once('e')
        .expect("LowerExp output has an exponent");
    let digits = mantissa.replace('.', "");
    let k = digits.len() as i64;
    let n = exponent
        .parse::<i64>()
        .expect("LowerExp output has an integer exponent")
        + 1;
    if k <= n && n <= 21 {
        format!("{digits}{}", "0".repeat((n - k) as usize))
    } else if 0 < n && n <= 21 {
        let (whole, fraction) = digits.split_at(n as usize);
        format!("{whole}.{fraction}")
    } else if -6 < n && n <= 0 {
        format!("0.{}{digits}", "0".repeat((-n) as usize))
    } else {
        let sign = if n - 1 < 0 { '-' } else { '+' };
        let exponent = (n - 1).abs();
        let (first, rest) = digits.split_at(1);
        if rest.is_empty() {
            format!("{first}e{sign}{exponent}")
        } else {
            format!("{first}.{rest}e{sign}{exponent}")
        }
    }
}

/// The ECMAScript WhiteSpace and LineTerminator code points that `String.prototype.trim` and
/// `Number(string)` remove.
pub fn is_js_whitespace(c: char) -> bool {
    matches!(
        c,
        '\u{0009}'
            | '\u{000A}'
            | '\u{000B}'
            | '\u{000C}'
            | '\u{000D}'
            | '\u{0020}'
            | '\u{00A0}'
            | '\u{1680}'
            | '\u{2000}'
            ..='\u{200A}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202F}'
                | '\u{205F}'
                | '\u{3000}'
                | '\u{FEFF}'
    )
}

/// `String.prototype.trim`.
pub fn trim(value: &str) -> &str {
    value.trim_matches(is_js_whitespace)
}

/// `Number(string)`: the StringNumericLiteral grammar, or `NaN`.
pub fn string_to_number(raw: &str) -> f64 {
    let text = trim(raw);
    if text.is_empty() {
        return 0.0;
    }
    let bytes = text.as_bytes();
    if bytes.len() > 2 && bytes[0] == b'0' {
        let radix = match bytes[1] {
            b'x' | b'X' => Some(16),
            b'o' | b'O' => Some(8),
            b'b' | b'B' => Some(2),
            _ => None,
        };
        if let Some(radix) = radix {
            let mut value = 0.0_f64;
            for c in text[2..].chars() {
                match c.to_digit(radix) {
                    Some(digit) => value = value * f64::from(radix) + f64::from(digit),
                    None => return f64::NAN,
                }
            }
            return value;
        }
    }
    if !is_str_decimal_literal(text) {
        return f64::NAN;
    }
    text.parse::<f64>().unwrap_or(f64::NAN)
}

/// StrDecimalLiteral: `[+-]? (Infinity | digits [. digits?] exp? | . digits exp?)`.
fn is_str_decimal_literal(text: &str) -> bool {
    let unsigned = text.strip_prefix(['+', '-']).unwrap_or(text);
    if unsigned == "Infinity" {
        return true;
    }
    let all_digits = |part: &str| part.bytes().all(|b| b.is_ascii_digit());
    let (mantissa, exponent) = match unsigned.find(['e', 'E']) {
        Some(index) => (&unsigned[..index], Some(&unsigned[index + 1..])),
        None => (unsigned, None),
    };
    if let Some(exponent) = exponent {
        let digits = exponent.strip_prefix(['+', '-']).unwrap_or(exponent);
        if digits.is_empty() || !all_digits(digits) {
            return false;
        }
    }
    let (whole, fraction) = match mantissa.split_once('.') {
        Some((whole, fraction)) => (whole, Some(fraction)),
        None => (mantissa, None),
    };
    if !all_digits(whole) || !fraction.is_none_or(all_digits) {
        return false;
    }
    !whole.is_empty() || fraction.is_some_and(|fraction| !fraction.is_empty())
}

/// `JSON.parse(text)`.
pub fn parse(text: &str) -> Result<Js, String> {
    serde_json::from_str::<Js>(text).map_err(|error| error.to_string())
}

/// UTF-8 decoding as `fs.readFile(path, "utf8")` performs it: invalid sequences become U+FFFD.
pub fn decode_utf8(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

impl<'de> Deserialize<'de> for Js {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(JsVisitor)
    }
}

struct JsVisitor;

impl<'de> Visitor<'de> for JsVisitor {
    type Value = Js;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a JSON value")
    }

    fn visit_unit<E>(self) -> Result<Js, E> {
        Ok(Js::Null)
    }

    fn visit_none<E>(self) -> Result<Js, E> {
        Ok(Js::Null)
    }

    fn visit_bool<E>(self, value: bool) -> Result<Js, E> {
        Ok(Js::Bool(value))
    }

    fn visit_i64<E>(self, value: i64) -> Result<Js, E> {
        Ok(Js::Number(value as f64))
    }

    fn visit_u64<E>(self, value: u64) -> Result<Js, E> {
        Ok(Js::Number(value as f64))
    }

    fn visit_f64<E>(self, value: f64) -> Result<Js, E> {
        Ok(Js::Number(value))
    }

    fn visit_str<E>(self, value: &str) -> Result<Js, E> {
        Ok(Js::String(value.to_owned()))
    }

    fn visit_string<E>(self, value: String) -> Result<Js, E> {
        Ok(Js::String(value))
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Js, A::Error> {
        let mut items = Vec::new();
        while let Some(item) = seq.next_element::<Js>()? {
            items.push(item);
        }
        Ok(Js::Array(items))
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Js, A::Error> {
        let mut object = Js::Object(Vec::new());
        while let Some(key) = map.next_key::<String>()? {
            let value = map.next_value::<Js>()?;
            object.set(&key, value);
        }
        Ok(object)
    }
}

/// `JSON.stringify(value)`; `None` (undefined) has no JSON text.
pub fn stringify(value: &Js) -> String {
    let mut out = String::new();
    write_value(&mut out, value, None, 0);
    out
}

/// `JSON.stringify(value, null, 2)`.
pub fn stringify_pretty(value: &Js) -> String {
    let mut out = String::new();
    write_value(&mut out, value, Some("  "), 0);
    out
}

fn write_value(out: &mut String, value: &Js, gap: Option<&str>, depth: usize) {
    match value {
        Js::Null => out.push_str("null"),
        Js::Bool(value) => out.push_str(if *value { "true" } else { "false" }),
        Js::Number(value) if value.is_finite() => out.push_str(&number_to_string(*value)),
        Js::Number(_) => out.push_str("null"),
        Js::String(value) => quote(out, value),
        Js::Array(items) => {
            if items.is_empty() {
                out.push_str("[]");
                return;
            }
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                newline(out, gap, depth + 1);
                write_value(out, item, gap, depth + 1);
            }
            newline(out, gap, depth);
            out.push(']');
        }
        Js::Object(entries) => {
            if entries.is_empty() {
                out.push_str("{}");
                return;
            }
            out.push('{');
            for (index, (key, item)) in property_order(entries).into_iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                newline(out, gap, depth + 1);
                quote(out, key);
                out.push(':');
                if gap.is_some() {
                    out.push(' ');
                }
                write_value(out, item, gap, depth + 1);
            }
            newline(out, gap, depth);
            out.push('}');
        }
    }
}

fn newline(out: &mut String, gap: Option<&str>, depth: usize) {
    if let Some(gap) = gap {
        out.push('\n');
        for _ in 0..depth {
            out.push_str(gap);
        }
    }
}

/// OrdinaryOwnPropertyKeys: array-index keys in ascending numeric order, then the other keys
/// in insertion order.
fn property_order(entries: &[(String, Js)]) -> Vec<(&String, &Js)> {
    let mut indexed = entries
        .iter()
        .filter_map(|(key, value)| array_index(key).map(|index| (index, key, value)))
        .collect::<Vec<_>>();
    indexed.sort_by_key(|(index, _, _)| *index);
    indexed
        .into_iter()
        .map(|(_, key, value)| (key, value))
        .chain(
            entries
                .iter()
                .filter(|(key, _)| array_index(key).is_none())
                .map(|(key, value)| (key, value)),
        )
        .collect()
}

/// A canonical array index: "0" or a decimal integer without a leading zero below 2^32 - 1.
fn array_index(key: &str) -> Option<u64> {
    if key.is_empty()
        || key.len() > 10
        || !key.bytes().all(|b| b.is_ascii_digit())
        || (key.len() > 1 && key.starts_with('0'))
    {
        return None;
    }
    key.parse::<u64>()
        .ok()
        .filter(|index| *index < 4_294_967_295)
}

/// QuoteJSONString.
fn quote(out: &mut String, value: &str) {
    out.push('"');
    for c in value.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if u32::from(c) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", u32::from(c));
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// `Array.prototype.sort()` with no comparator on strings: UTF-16 code unit order.
pub fn utf16_cmp(left: &str, right: &str) -> Ordering {
    left.encode_utf16().cmp(right.encode_utf16())
}

/// `left.localeCompare(right)` under Node's ICU root collation, for the characters a fixture
/// body path can contain.
///
/// ASCII follows the CLDR root order (whitespace, punctuation and symbols, digits, then letters
/// with lowercase before uppercase at the tertiary level); C0 controls are ignorable. Other
/// characters sort after ASCII by code point, which is an approximation. A difference from ICU
/// can only make the frozen-fixture body list fail to match, never match a list that ICU orders
/// differently, because the comparison checks the committed order.
pub fn locale_cmp(left: &str, right: &str) -> Ordering {
    let keys = |text: &str| {
        text.chars()
            .filter_map(collation_key)
            .collect::<Vec<(u32, u8)>>()
    };
    let (left, right) = (keys(left), keys(right));
    let primary = |keys: &[(u32, u8)]| keys.iter().map(|(primary, _)| *primary).collect::<Vec<_>>();
    primary(&left).cmp(&primary(&right)).then_with(|| {
        let tertiary = |keys: &[(u32, u8)]| {
            keys.iter()
                .map(|(_, tertiary)| *tertiary)
                .collect::<Vec<_>>()
        };
        tertiary(&left).cmp(&tertiary(&right))
    })
}

/// CLDR root order of the printable ASCII punctuation and symbols.
const ASCII_SYMBOL_ORDER: &str = "_-,;:!?.'\"()[]{}@*/\\&#%`^+<=>|~$";

fn collation_key(c: char) -> Option<(u32, u8)> {
    const WHITESPACE: u32 = 100;
    const SYMBOLS: u32 = 200;
    const DIGITS: u32 = 300;
    const LETTERS: u32 = 400;
    const OTHER: u32 = 1_000;
    match c {
        '\t' | '\n' | '\u{b}' | '\u{c}' | '\r' => Some((WHITESPACE + u32::from(c), 0)),
        ' ' => Some((WHITESPACE + 0x20, 0)),
        '\0'..='\u{1f}' | '\u{7f}' => None,
        '0'..='9' => Some((DIGITS + u32::from(c) - u32::from('0'), 0)),
        'a'..='z' => Some((LETTERS + u32::from(c) - u32::from('a'), 0)),
        'A'..='Z' => Some((LETTERS + u32::from(c) - u32::from('A'), 1)),
        c if c.is_ascii() => ASCII_SYMBOL_ORDER
            .find(c)
            .map(|position| (SYMBOLS + position as u32, 0)),
        c => Some((OTHER + u32::from(c), 0)),
    }
}

/// `new Date(milliseconds).toISOString()` for years 0..=9999.
pub fn iso_from_millis(millis: i64) -> String {
    let days = millis.div_euclid(86_400_000);
    let rest = millis.rem_euclid(86_400_000);
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        rest / 3_600_000,
        rest / 60_000 % 60,
        rest / 1_000 % 60,
        rest % 1_000
    )
}

/// `new Date().toISOString()`.
pub fn iso_now() -> String {
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or(0);
    iso_from_millis(millis)
}

/// Days since 1970-01-01 to a proleptic Gregorian (year, month, day).
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

/// Days in a proleptic Gregorian month.
pub fn days_in_month(year: i64, month: i64) -> i64 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if (year % 4 == 0 && year % 100 != 0) || year % 400 == 0 => 29,
        2 => 28,
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_format_like_ecmascript() {
        for (value, expected) in [
            (0.0, "0"),
            (-0.0, "0"),
            (1.0, "1"),
            (-23.0, "-23"),
            (1.5, "1.5"),
            (0.1, "0.1"),
            (123_456_789.0, "123456789"),
            (1e21, "1e+21"),
            (1.5e21, "1.5e+21"),
            (1e20, "100000000000000000000"),
            (1e-6, "0.000001"),
            (1e-7, "1e-7"),
            (1.25e-7, "1.25e-7"),
            (9_007_199_254_740_993.0, "9007199254740992"),
            (f64::NAN, "NaN"),
            (f64::INFINITY, "Infinity"),
        ] {
            assert_eq!(number_to_string(value), expected, "{value:e}");
        }
    }

    #[test]
    fn number_conversion_follows_the_string_numeric_literal_grammar() {
        for (text, expected) in [
            ("23", 23.0),
            (" 23\n", 23.0),
            ("\u{feff}7\u{3000}", 7.0),
            ("", 0.0),
            ("   ", 0.0),
            ("0x17", 23.0),
            ("0X1f", 31.0),
            ("0o17", 15.0),
            ("0b101", 5.0),
            ("1e1", 10.0),
            ("+5", 5.0),
            ("5.", 5.0),
            (".5e1", 5.0),
            ("23.0", 23.0),
        ] {
            assert_eq!(string_to_number(text), expected, "{text:?}");
        }
        assert!(string_to_number("-0").is_sign_negative());
        assert_eq!(string_to_number("Infinity"), f64::INFINITY);
        assert_eq!(string_to_number("-Infinity"), f64::NEG_INFINITY);
        for text in [
            "0x", "-0x10", "1_000", "12abc", "inf", "NaN", "nan", "e5", "1e", ".", "+", "0b2",
            "\u{85}5",
        ] {
            assert!(string_to_number(text).is_nan(), "{text:?}");
        }
    }

    #[test]
    fn stringify_matches_json_stringify() {
        let value =
            parse(r#"{"b":1,"a":[1,"x",{"c":null}],"2":true,"1":false,"b":2,"e":{},"f":[]}"#)
                .unwrap();
        assert_eq!(
            stringify(&value),
            r#"{"1":false,"2":true,"b":2,"a":[1,"x",{"c":null}],"e":{},"f":[]}"#
        );
        assert_eq!(
            stringify_pretty(&value),
            "{\n  \"1\": false,\n  \"2\": true,\n  \"b\": 2,\n  \"a\": [\n    1,\n    \"x\",\n    \
             {\n      \"c\": null\n    }\n  ],\n  \"e\": {},\n  \"f\": []\n}"
        );
        assert_eq!(
            stringify(&Js::str(
                "q\"b\\\u{8}\u{c}\n\r\t\u{1}\u{1f}\u{7f}\u{2028}é😀"
            )),
            "\"q\\\"b\\\\\\b\\f\\n\\r\\t\\u0001\\u001f\u{7f}\u{2028}é😀\""
        );
        assert_eq!(stringify(&parse("1.0").unwrap()), "1");
        assert_eq!(stringify(&parse("1E2").unwrap()), "100");
    }

    #[test]
    fn string_conversion_and_truthiness_follow_javascript() {
        let value = parse(r#"["v1.2.3"]"#).unwrap();
        assert_eq!(to_js_string(Some(&value)), "v1.2.3");
        let value = parse(r#"[null,1,[2,[3]],{"a":1},true]"#).unwrap();
        assert_eq!(to_js_string(Some(&value)), ",1,2,3,[object Object],true");
        assert_eq!(to_js_string(None), "undefined");
        for (text, expected) in [
            ("0", false),
            ("\"\"", false),
            ("null", false),
            ("false", false),
            ("1", true),
            ("\"0\"", true),
            ("[]", true),
            ("{}", true),
        ] {
            assert_eq!(truthy(Some(&parse(text).unwrap())), expected, "{text}");
        }
        assert!(!truthy(None));
    }

    #[test]
    fn sort_orders_match_javascript() {
        let mut names = vec!["\u{ff5e}", "😀", "b", "B", "a"];
        names.sort_by(|left, right| utf16_cmp(left, right));
        assert_eq!(names, ["B", "a", "b", "😀", "\u{ff5e}"]);
        let mut paths = vec![
            "single16.html",
            ".history/single16/1.html",
            "bundle16/index.html",
            "bundle16/assets/site.css",
            "B.html",
            "a.html",
            "a-b.html",
            "a_b.html",
            "10.html",
            "9.html",
        ];
        paths.sort_by(|left, right| locale_cmp(left, right));
        assert_eq!(
            paths,
            [
                ".history/single16/1.html",
                "10.html",
                "9.html",
                "a_b.html",
                "a-b.html",
                "a.html",
                "B.html",
                "bundle16/assets/site.css",
                "bundle16/index.html",
                "single16.html",
            ]
        );
    }

    #[test]
    fn iso_timestamps_match_to_iso_string() {
        assert_eq!(iso_from_millis(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(
            iso_from_millis(1_792_548_572_527),
            "2026-10-21T02:09:32.527Z"
        );
        assert_eq!(iso_from_millis(951_782_400_000), "2000-02-29T00:00:00.000Z");
        assert_eq!(
            iso_from_millis(-62_167_219_200_000),
            "0000-01-01T00:00:00.000Z"
        );
    }
}
