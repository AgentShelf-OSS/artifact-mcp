//! JSON values with JavaScript semantics.
//!
//! The operator commands replace Node scripts whose request bodies and printed JSON other tools
//! consume. To keep those bytes identical, this module models a parsed JSON value the way V8 does:
//! object keys keep insertion order, except that array-index keys ("0", "1", ...) come first in
//! ascending order; a duplicate key keeps its first position and takes the last value; every
//! number is an IEEE double printed like `Number.prototype.toString`. [`stringify`] and
//! [`stringify_pretty`] match `JSON.stringify(value)` and `JSON.stringify(value, null, 2)`.
//!
//! A missing property (JavaScript `undefined`) is `None` wherever an `Option<&Js>` appears.

use std::fmt::{self, Write as _};

use serde::de::{Deserialize, Deserializer, MapAccess, SeqAccess, Visitor};

/// One JSON value.
#[derive(Clone, Debug, PartialEq)]
pub enum Js {
    Null,
    Bool(bool),
    Number(f64),
    String(String),
    Array(Vec<Js>),
    Object(Object),
}

/// A JSON object in JavaScript property order.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Object(Vec<(String, Js)>);

/// The array index a property key denotes, if any (a canonical integer below 2^32 - 1).
fn array_index(key: &str) -> Option<u32> {
    if key == "0" {
        return Some(0);
    }
    let bytes = key.as_bytes();
    if bytes.is_empty() || bytes.len() > 10 || bytes[0] == b'0' {
        return None;
    }
    if !bytes.iter().all(u8::is_ascii_digit) {
        return None;
    }
    let value: u64 = key.parse().ok()?;
    u32::try_from(value).ok().filter(|index| *index != u32::MAX)
}

impl Object {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get(&self, key: &str) -> Option<&Js> {
        self.0
            .iter()
            .find(|(existing, _)| existing == key)
            .map(|(_, value)| value)
    }

    /// Define `key` like a JavaScript property assignment: an existing key keeps its position.
    pub fn insert(&mut self, key: impl Into<String>, value: Js) {
        let key = key.into();
        if let Some(slot) = self.0.iter_mut().find(|(existing, _)| *existing == key) {
            slot.1 = value;
            return;
        }
        match array_index(&key) {
            Some(index) => {
                let at = self
                    .0
                    .iter()
                    .position(|(existing, _)| array_index(existing).is_none_or(|i| i > index))
                    .unwrap_or(self.0.len());
                self.0.insert(at, (key, value));
            }
            None => self.0.push((key, value)),
        }
    }

    pub fn keys(&self) -> impl Iterator<Item = &str> {
        self.0.iter().map(|(key, _)| key.as_str())
    }

    pub fn values(&self) -> impl Iterator<Item = &Js> {
        self.0.iter().map(|(_, value)| value)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &Js)> {
        self.0.iter().map(|(key, value)| (key.as_str(), value))
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// Build an object from `(key, value)` pairs in order.
pub fn object<const N: usize>(pairs: [(&str, Js); N]) -> Js {
    let mut object = Object::new();
    for (key, value) in pairs {
        object.insert(key, value);
    }
    Js::Object(object)
}

impl From<&str> for Js {
    fn from(value: &str) -> Self {
        Js::String(value.to_owned())
    }
}

impl From<String> for Js {
    fn from(value: String) -> Self {
        Js::String(value)
    }
}

impl From<bool> for Js {
    fn from(value: bool) -> Self {
        Js::Bool(value)
    }
}

impl From<f64> for Js {
    fn from(value: f64) -> Self {
        Js::Number(value)
    }
}

impl From<u64> for Js {
    #[allow(clippy::cast_precision_loss)]
    fn from(value: u64) -> Self {
        Js::Number(value as f64)
    }
}

impl From<Object> for Js {
    fn from(value: Object) -> Self {
        Js::Object(value)
    }
}

impl From<Vec<Js>> for Js {
    fn from(value: Vec<Js>) -> Self {
        Js::Array(value)
    }
}

impl Js {
    /// An own data property of an object; `None` for every other value.
    pub fn property(&self, key: &str) -> Option<&Js> {
        match self {
            Js::Object(object) => object.get(key),
            _ => None,
        }
    }
}

struct JsVisitor;

impl<'de> Visitor<'de> for JsVisitor {
    type Value = Js;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("a JSON value")
    }

    fn visit_bool<E>(self, value: bool) -> Result<Js, E> {
        Ok(Js::Bool(value))
    }

    #[allow(clippy::cast_precision_loss)]
    fn visit_i64<E>(self, value: i64) -> Result<Js, E> {
        Ok(Js::Number(value as f64))
    }

    #[allow(clippy::cast_precision_loss)]
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

    fn visit_unit<E>(self) -> Result<Js, E> {
        Ok(Js::Null)
    }

    fn visit_none<E>(self) -> Result<Js, E> {
        Ok(Js::Null)
    }

    fn visit_some<D: Deserializer<'de>>(self, deserializer: D) -> Result<Js, D::Error> {
        Js::deserialize(deserializer)
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Js, A::Error> {
        let mut items = Vec::new();
        while let Some(item) = seq.next_element()? {
            items.push(item);
        }
        Ok(Js::Array(items))
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Js, A::Error> {
        let mut object = Object::new();
        while let Some((key, value)) = map.next_entry::<String, Js>()? {
            object.insert(key, value);
        }
        Ok(Js::Object(object))
    }
}

impl<'de> Deserialize<'de> for Js {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Js, D::Error> {
        deserializer.deserialize_any(JsVisitor)
    }
}

/// `JSON.parse(text)`.
pub fn parse(text: &str) -> Result<Js, serde_json::Error> {
    serde_json::from_str(text)
}

/// The message `JSON.parse` would throw for `text`, as closely as serde reports it.
pub fn parse_error_message(text: &str, error: &serde_json::Error) -> String {
    if error.is_eof() || text.trim_matches([' ', '\t', '\n', '\r']).is_empty() {
        "Unexpected end of JSON input".to_owned()
    } else {
        format!("Unexpected token in JSON ({error})")
    }
}

/// `JSON.stringify(value)`.
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

fn newline(out: &mut String, indent: Option<&str>, depth: usize) {
    if let Some(unit) = indent {
        out.push('\n');
        for _ in 0..depth {
            out.push_str(unit);
        }
    }
}

fn write_value(out: &mut String, value: &Js, indent: Option<&str>, depth: usize) {
    match value {
        Js::Null => out.push_str("null"),
        Js::Bool(true) => out.push_str("true"),
        Js::Bool(false) => out.push_str("false"),
        Js::Number(number) => out.push_str(&json_number(*number)),
        Js::String(text) => write_string(out, text),
        Js::Array(items) if items.is_empty() => out.push_str("[]"),
        Js::Array(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                newline(out, indent, depth + 1);
                write_value(out, item, indent, depth + 1);
            }
            newline(out, indent, depth);
            out.push(']');
        }
        Js::Object(object) if object.is_empty() => out.push_str("{}"),
        Js::Object(object) => {
            out.push('{');
            for (index, (key, item)) in object.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                newline(out, indent, depth + 1);
                write_string(out, key);
                out.push(':');
                if indent.is_some() {
                    out.push(' ');
                }
                write_value(out, item, indent, depth + 1);
            }
            newline(out, indent, depth);
            out.push('}');
        }
    }
}

fn write_string(out: &mut String, text: &str) {
    out.push('"');
    for character in text.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            control if u32::from(control) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", u32::from(control));
            }
            other => out.push(other),
        }
    }
    out.push('"');
}

/// A number as `JSON.stringify` writes it (`null` when it is not finite).
pub fn json_number(value: f64) -> String {
    if value.is_finite() {
        number_to_string(value)
    } else {
        "null".to_owned()
    }
}

/// `Number.prototype.toString()` for radix 10 (ECMA-262 Number::toString).
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
    // Rust prints the shortest round-trip digits; `{:e}` exposes them with the exponent.
    let scientific = format!("{value:e}");
    let (mantissa, exponent) = scientific
        .split_once('e')
        .expect("`{:e}` always writes an exponent");
    let exponent: i64 = exponent.parse().expect("the exponent is an integer");
    let digits: String = mantissa.chars().filter(char::is_ascii_digit).collect();
    let k = i64::try_from(digits.len()).expect("a double has at most 17 significant digits");
    let n = exponent + 1;
    let zeros = |count: i64| "0".repeat(usize::try_from(count).unwrap_or(0));
    if k <= n && n <= 21 {
        format!("{digits}{}", zeros(n - k))
    } else if 0 < n && n <= 21 {
        let split = usize::try_from(n).expect("0 < n <= 21");
        format!("{}.{}", &digits[..split], &digits[split..])
    } else if -6 < n && n <= 0 {
        format!("0.{}{digits}", zeros(-n))
    } else {
        let sign = if n - 1 < 0 { '-' } else { '+' };
        let magnitude = (n - 1).abs();
        if k == 1 {
            format!("{digits}e{sign}{magnitude}")
        } else {
            format!("{}.{}e{sign}{magnitude}", &digits[..1], &digits[1..])
        }
    }
}

/// JavaScript truthiness; `None` is `undefined`.
pub fn truthy(value: Option<&Js>) -> bool {
    match value {
        None | Some(Js::Null) | Some(Js::Bool(false)) => false,
        Some(Js::Number(number)) => *number != 0.0 && !number.is_nan(),
        Some(Js::String(text)) => !text.is_empty(),
        Some(_) => true,
    }
}

/// `value || fallback`.
pub fn or(value: Option<&Js>, fallback: Js) -> Js {
    if truthy(value) {
        value.cloned().unwrap_or(fallback)
    } else {
        fallback
    }
}

/// `String(value)` / template-literal interpolation.
pub fn to_js_string(value: Option<&Js>) -> String {
    match value {
        None => "undefined".to_owned(),
        Some(Js::Null) => "null".to_owned(),
        Some(Js::Bool(flag)) => flag.to_string(),
        Some(Js::Number(number)) => number_to_string(*number),
        Some(Js::String(text)) => text.clone(),
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

/// `value === "text"`.
pub fn is_str(value: Option<&Js>, text: &str) -> bool {
    matches!(value, Some(Js::String(actual)) if actual == text)
}

/// `left === right` for values read from one parsed document (objects compare by identity).
pub fn strict_equals(left: Option<&Js>, right: Option<&Js>) -> bool {
    match (left, right) {
        (None, None) | (Some(Js::Null), Some(Js::Null)) => true,
        (Some(Js::Bool(a)), Some(Js::Bool(b))) => a == b,
        (Some(Js::Number(a)), Some(Js::Number(b))) => a == b,
        (Some(Js::String(a)), Some(Js::String(b))) => a == b,
        (Some(a @ (Js::Array(_) | Js::Object(_))), Some(b)) => std::ptr::eq(a, b),
        _ => false,
    }
}

/// SameValueZero, the key equality of a JavaScript `Map`.
pub fn same_value_zero(left: Option<&Js>, right: Option<&Js>) -> bool {
    match (left, right) {
        (Some(Js::Number(a)), Some(Js::Number(b))) if a.is_nan() && b.is_nan() => true,
        _ => strict_equals(left, right),
    }
}

/// `value.key`: a TypeError message when `value` is `undefined` or `null`.
pub fn get<'a>(value: Option<&'a Js>, key: &str) -> Result<Option<&'a Js>, String> {
    match value {
        None => Err(format!(
            "Cannot read properties of undefined (reading '{key}')"
        )),
        Some(Js::Null) => Err(format!("Cannot read properties of null (reading '{key}')")),
        Some(other) => Ok(other.property(key)),
    }
}

/// `value?.key`.
pub fn get_opt<'a>(value: Option<&'a Js>, key: &str) -> Option<&'a Js> {
    value.and_then(|value| value.property(key))
}

/// `Number(value)`.
pub fn to_number(value: Option<&Js>) -> f64 {
    match value {
        None => f64::NAN,
        Some(Js::Null) => 0.0,
        Some(Js::Bool(flag)) => f64::from(u8::from(*flag)),
        Some(Js::Number(number)) => *number,
        Some(Js::String(text)) => string_to_number(text),
        Some(array @ Js::Array(_)) => string_to_number(&to_js_string(Some(array))),
        Some(Js::Object(_)) => f64::NAN,
    }
}

/// `Number(text)` for a string (StringToNumber).
pub fn string_to_number(text: &str) -> f64 {
    let trimmed = js_trim(text);
    if trimmed.is_empty() {
        return 0.0;
    }
    for (prefix, radix) in [
        ("0x", 16),
        ("0X", 16),
        ("0o", 8),
        ("0O", 8),
        ("0b", 2),
        ("0B", 2),
    ] {
        if let Some(digits) = trimmed.strip_prefix(prefix) {
            if digits.is_empty() || !digits.chars().all(|c| c.is_digit(radix)) {
                return f64::NAN;
            }
            return digits.chars().fold(0.0, |total, c| {
                total * f64::from(radix) + f64::from(c.to_digit(radix).unwrap_or(0))
            });
        }
    }
    let unsigned = trimmed.strip_prefix(['+', '-']).unwrap_or(trimmed);
    if unsigned == "Infinity" {
        return if trimmed.starts_with('-') {
            f64::NEG_INFINITY
        } else {
            f64::INFINITY
        };
    }
    if !is_decimal_literal(unsigned) {
        return f64::NAN;
    }
    trimmed.parse().unwrap_or(f64::NAN)
}

fn is_decimal_literal(text: &str) -> bool {
    let bytes = text.as_bytes();
    let mut index = 0;
    let digits = |index: &mut usize| {
        let start = *index;
        while bytes.get(*index).is_some_and(u8::is_ascii_digit) {
            *index += 1;
        }
        *index - start
    };
    let mut significant = digits(&mut index);
    if bytes.get(index) == Some(&b'.') {
        index += 1;
        significant += digits(&mut index);
    }
    if significant == 0 {
        return false;
    }
    if matches!(bytes.get(index), Some(b'e' | b'E')) {
        index += 1;
        if matches!(bytes.get(index), Some(b'+' | b'-')) {
            index += 1;
        }
        if digits(&mut index) == 0 {
            return false;
        }
    }
    index == bytes.len()
}

/// `String.prototype.trim` (ECMAScript WhiteSpace and LineTerminator, including U+FEFF).
pub fn js_trim(text: &str) -> &str {
    text.trim_matches(|c: char| {
        matches!(
            c,
            '\t' | '\n' | '\u{b}' | '\u{c}' | '\r' | ' ' | '\u{a0}' | '\u{1680}' | '\u{2000}'
                ..='\u{200a}'
                    | '\u{2028}'
                    | '\u{2029}'
                    | '\u{202f}'
                    | '\u{205f}'
                    | '\u{3000}'
                    | '\u{feff}'
        )
    })
}

/// `encodeURIComponent(text)`.
pub fn encode_uri_component(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&byte) {
            out.push(char::from(byte));
        } else {
            let _ = write!(out, "%{byte:02X}");
        }
    }
    out
}

fn canonical(value: &Js) -> Js {
    match value {
        Js::Array(items) => Js::Array(items.iter().map(canonical).collect()),
        Js::Object(object) => {
            let mut keys: Vec<&str> = object.keys().collect();
            keys.sort_by_key(|key| key.encode_utf16().collect::<Vec<_>>());
            let mut sorted = Object::new();
            for key in keys {
                if let Some(item) = object.get(key) {
                    sorted.insert(key, canonical(item));
                }
            }
            Js::Object(sorted)
        }
        other => other.clone(),
    }
}

/// `JSON.stringify(canonical(left)) === JSON.stringify(canonical(right))`.
pub fn same_value(left: &Js, right: &Js) -> bool {
    stringify(&canonical(left)) == stringify(&canonical(right))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn objects_follow_v8_property_order() {
        let parsed = parse(r#"{"b":1,"10":2,"a":3,"2":4,"b":5,"01":6}"#).unwrap();
        assert_eq!(stringify(&parsed), r#"{"2":4,"10":2,"b":5,"a":3,"01":6}"#);
    }

    #[test]
    fn numbers_print_like_javascript() {
        for (value, expected) in [
            (1.0, "1"),
            (-0.0, "0"),
            (0.1, "0.1"),
            (100.0, "100"),
            (1e21, "1e+21"),
            (1.5e21, "1.5e+21"),
            (123_456_789_012_345_680_000.0, "123456789012345680000"),
            (1e-6, "0.000001"),
            (1e-7, "1e-7"),
            (1.25e-7, "1.25e-7"),
            (12.345_678_9, "12.3456789"),
            (5e-324, "5e-324"),
        ] {
            assert_eq!(number_to_string(value), expected, "{value:e}");
        }
        assert_eq!(json_number(f64::NAN), "null");
        assert_eq!(
            stringify(&parse("[1.0, 1e2, 18446744073709551616]").unwrap()),
            "[1,100,18446744073709552000]"
        );
    }

    #[test]
    fn strings_escape_like_json_stringify() {
        let value = Js::from("q\"b\\\u{8}\u{c}\n\r\t\u{1}\u{7f}é</script>");
        assert_eq!(
            stringify(&value),
            "\"q\\\"b\\\\\\b\\f\\n\\r\\t\\u0001\u{7f}é</script>\""
        );
    }

    #[test]
    fn pretty_printing_matches_two_space_indent() {
        let value = parse(r#"{"a":[],"b":{},"c":[1,{"d":null}]}"#).unwrap();
        assert_eq!(
            stringify_pretty(&value),
            "{\n  \"a\": [],\n  \"b\": {},\n  \"c\": [\n    1,\n    {\n      \"d\": null\n    }\n  ]\n}"
        );
    }

    #[test]
    fn number_conversion_matches_javascript() {
        for (text, expected) in [
            ("", 0.0),
            (" 7 ", 7.0),
            ("0x10", 16.0),
            ("1e2", 100.0),
            ("+5", 5.0),
            ("5.", 5.0),
            (".5", 0.5),
        ] {
            assert_eq!(string_to_number(text), expected, "{text:?}");
        }
        for text in ["inf", "nan", "1_0", "0x", "abc", "1e"] {
            assert!(string_to_number(text).is_nan(), "{text:?}");
        }
        assert_eq!(string_to_number("-Infinity"), f64::NEG_INFINITY);
    }

    #[test]
    fn uri_components_and_trim() {
        assert_eq!(
            encode_uri_component("a b/c?d=é!'()*~"),
            "a%20b%2Fc%3Fd%3D%C3%A9!'()*~"
        );
        assert_eq!(js_trim("\u{feff} x \u{a0}"), "x");
    }

    #[test]
    fn canonical_comparison_ignores_key_order() {
        let left = parse(r#"{"a":1,"b":{"c":2,"d":3}}"#).unwrap();
        let right = parse(r#"{"b":{"d":3,"c":2},"a":1.0}"#).unwrap();
        assert!(same_value(&left, &right));
        assert!(!same_value(&left, &parse(r#"{"a":1}"#).unwrap()));
    }
}
