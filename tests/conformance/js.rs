// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Neil Blackman
//
// JSON values with JavaScript semantics.
//
// The goldens were written by `JSON.stringify(value, null, 2)`, and every comparison is defined as
// equality of `JSON.stringify` output. Reproducing them byte for byte needs three JavaScript rules
// that `serde_json::Value` does not follow:
//
// - Object keys keep insertion order, except that array-index keys ("0", "1", ...) always
//   enumerate first, in ascending numeric order.
// - Every number is an IEEE-754 double, printed with `Number.prototype.toString` (so `1.0` prints
//   as `1`, and integers above 2^53 lose precision exactly as `JSON.parse` loses it).
// - `JSON.stringify` escapes only `"`, `\` and control characters below U+0020.

use std::cmp::Ordering;
use std::fmt;

use serde::Deserialize;
use serde::de::{self, MapAccess, SeqAccess, Visitor};

/// A JSON value as `JSON.parse` would produce it.
#[derive(Clone, Debug)]
pub enum Js {
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
    Arr(Vec<Js>),
    Obj(Obj),
}

/// An object whose entries are kept in JavaScript property-enumeration order.
#[derive(Clone, Debug, Default)]
pub struct Obj(Vec<(String, Js)>);

/// The array-index value of `key`, if JavaScript treats it as an integer-indexed property.
fn array_index(key: &str) -> Option<u32> {
    let bytes = key.as_bytes();
    let canonical = key == "0"
        || (!bytes.is_empty()
            && bytes[0] != b'0'
            && bytes.len() <= 10
            && bytes.iter().all(u8::is_ascii_digit));
    if !canonical {
        return None;
    }
    key.parse::<u64>()
        .ok()
        .filter(|value| *value < u64::from(u32::MAX))
        .and_then(|value| u32::try_from(value).ok())
}

impl Obj {
    pub fn new() -> Self {
        Self::default()
    }

    /// `object[key] = value`: an existing key keeps its position; a new array-index key sorts
    /// among the leading index keys; any other new key goes last.
    pub fn insert(&mut self, key: impl Into<String>, value: Js) {
        let key = key.into();
        if let Some(slot) = self.0.iter_mut().find(|(existing, _)| *existing == key) {
            slot.1 = value;
            return;
        }
        match array_index(&key) {
            Some(index) => {
                let position = self
                    .0
                    .iter()
                    .position(|(existing, _)| array_index(existing).is_none_or(|e| e > index))
                    .unwrap_or(self.0.len());
                self.0.insert(position, (key, value));
            }
            None => self.0.push((key, value)),
        }
    }

    pub fn with(mut self, key: impl Into<String>, value: Js) -> Self {
        self.insert(key, value);
        self
    }

    pub fn get(&self, key: &str) -> Option<&Js> {
        self.0
            .iter()
            .find(|(existing, _)| existing == key)
            .map(|(_, value)| value)
    }

    pub fn contains_key(&self, key: &str) -> bool {
        self.get(key).is_some()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&String, &Js)> {
        self.0.iter().map(|(key, value)| (key, value))
    }

    pub fn keys(&self) -> impl Iterator<Item = &String> {
        self.0.iter().map(|(key, _)| key)
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl Js {
    pub fn str(value: impl Into<String>) -> Self {
        Self::Str(value.into())
    }

    /// Member access on an object; `None` stands for `undefined`.
    pub fn get(&self, key: &str) -> Option<&Js> {
        match self {
            Js::Obj(object) => object.get(key),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Js::Str(value) => Some(value),
            _ => None,
        }
    }

    pub fn as_array(&self) -> Option<&Vec<Js>> {
        match self {
            Js::Arr(items) => Some(items),
            _ => None,
        }
    }

    pub fn as_object(&self) -> Option<&Obj> {
        match self {
            Js::Obj(object) => Some(object),
            _ => None,
        }
    }

    /// JavaScript truthiness.
    pub fn truthy(&self) -> bool {
        match self {
            Js::Null => false,
            Js::Bool(value) => *value,
            Js::Num(value) => *value != 0.0 && !value.is_nan(),
            Js::Str(value) => !value.is_empty(),
            Js::Arr(_) | Js::Obj(_) => true,
        }
    }
}

/// Truthiness of a possibly-`undefined` value.
pub fn truthy(value: Option<&Js>) -> bool {
    value.is_some_and(Js::truthy)
}

/// General property access (`value[key]`) for objects, arrays and strings. `None` is `undefined`.
pub fn property(value: &Js, key: &str) -> Option<Js> {
    match value {
        Js::Obj(object) => object.get(key).cloned(),
        Js::Arr(items) => {
            if key == "length" {
                return Some(Js::Num(items.len() as f64));
            }
            array_index(key).and_then(|index| items.get(index as usize).cloned())
        }
        Js::Str(text) => {
            let units: Vec<u16> = text.encode_utf16().collect();
            if key == "length" {
                return Some(Js::Num(units.len() as f64));
            }
            array_index(key)
                .and_then(|index| units.get(index as usize).copied())
                .map(|unit| Js::Str(String::from_utf16_lossy(&[unit])))
        }
        _ => None,
    }
}

/// `String(value)`.
pub fn to_js_string(value: &Js) -> String {
    match value {
        Js::Null => "null".to_owned(),
        Js::Bool(value) => value.to_string(),
        Js::Num(value) => number_to_string(*value),
        Js::Str(value) => value.clone(),
        Js::Arr(items) => items
            .iter()
            .map(|item| match item {
                Js::Null => String::new(),
                other => to_js_string(other),
            })
            .collect::<Vec<_>>()
            .join(","),
        Js::Obj(_) => "[object Object]".to_owned(),
    }
}

/// Length in UTF-16 code units, which is what JavaScript's `.length` counts.
pub fn utf16_len(text: &str) -> usize {
    text.encode_utf16().count()
}

/// The default `Array.prototype.sort` order for strings: UTF-16 code-unit order.
pub fn cmp_utf16(left: &str, right: &str) -> Ordering {
    left.encode_utf16().cmp(right.encode_utf16())
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
    // Rust's `{:e}` prints the shortest digit string that round-trips, as ECMA-262 requires.
    let scientific = format!("{value:e}");
    let (mantissa, exponent) = scientific
        .split_once('e')
        .expect("scientific notation has an exponent");
    let exponent: i32 = exponent.parse().expect("decimal exponent");
    let digits: String = mantissa.chars().filter(|c| *c != '.').collect();
    let k = i32::try_from(digits.len()).expect("digit count fits i32");
    let n = exponent + 1;
    if k <= n && n <= 21 {
        format!("{digits}{}", "0".repeat((n - k) as usize))
    } else if 0 < n && n <= 21 {
        let split = n as usize;
        format!("{}.{}", &digits[..split], &digits[split..])
    } else if -6 < n && n <= 0 {
        format!("0.{}{digits}", "0".repeat((-n) as usize))
    } else {
        let e = n - 1;
        let sign = if e < 0 { '-' } else { '+' };
        if k == 1 {
            format!("{digits}e{sign}{}", e.abs())
        } else {
            format!("{}.{}e{sign}{}", &digits[..1], &digits[1..], e.abs())
        }
    }
}

/// `JSON.parse(text)`.
pub fn parse(text: &str) -> Result<Js, String> {
    serde_json::from_str::<Js>(text).map_err(|error| error.to_string())
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

/// Recursively sort object keys (the conformance `canonicalize`). Array order is kept.
pub fn canonicalize(value: &Js) -> Js {
    match value {
        Js::Arr(items) => Js::Arr(items.iter().map(canonicalize).collect()),
        Js::Obj(object) => {
            let mut keys: Vec<&String> = object.keys().collect();
            keys.sort_by(|a, b| cmp_utf16(a, b));
            let mut out = Obj::new();
            for key in keys {
                out.insert(
                    key.clone(),
                    canonicalize(object.get(key).expect("key from same object")),
                );
            }
            Js::Obj(out)
        }
        other => other.clone(),
    }
}

/// `JSON.stringify(canonicalize(value))`.
pub fn stable_stringify(value: &Js) -> String {
    stringify(&canonicalize(value))
}

fn write_value(out: &mut String, value: &Js, indent: Option<&str>, depth: usize) {
    match value {
        Js::Null => out.push_str("null"),
        Js::Bool(value) => out.push_str(if *value { "true" } else { "false" }),
        Js::Num(value) if value.is_finite() => out.push_str(&number_to_string(*value)),
        Js::Num(_) => out.push_str("null"),
        Js::Str(value) => write_string(out, value),
        Js::Arr(items) => {
            if items.is_empty() {
                out.push_str("[]");
                return;
            }
            out.push('[');
            for (position, item) in items.iter().enumerate() {
                if position > 0 {
                    out.push(',');
                }
                newline(out, indent, depth + 1);
                write_value(out, item, indent, depth + 1);
            }
            newline(out, indent, depth);
            out.push(']');
        }
        Js::Obj(object) => {
            if object.is_empty() {
                out.push_str("{}");
                return;
            }
            out.push('{');
            for (position, (key, item)) in object.iter().enumerate() {
                if position > 0 {
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

fn newline(out: &mut String, indent: Option<&str>, depth: usize) {
    if let Some(indent) = indent {
        out.push('\n');
        for _ in 0..depth {
            out.push_str(indent);
        }
    }
}

fn write_string(out: &mut String, value: &str) {
    out.push('"');
    for character in value.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{08}' => out.push_str("\\b"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\u{0c}' => out.push_str("\\f"),
            '\r' => out.push_str("\\r"),
            control if u32::from(control) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", u32::from(control)));
            }
            other => out.push(other),
        }
    }
    out.push('"');
}

impl<'de> Deserialize<'de> for Js {
    fn deserialize<D: de::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(JsVisitor)
    }
}

struct JsVisitor;

impl<'de> Visitor<'de> for JsVisitor {
    type Value = Js;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("any JSON value")
    }

    fn visit_bool<E>(self, value: bool) -> Result<Js, E> {
        Ok(Js::Bool(value))
    }

    // JSON numbers are doubles in JavaScript; large integers round exactly as JSON.parse rounds.
    #[allow(clippy::cast_precision_loss)]
    fn visit_i64<E>(self, value: i64) -> Result<Js, E> {
        Ok(Js::Num(value as f64))
    }

    #[allow(clippy::cast_precision_loss)]
    fn visit_u64<E>(self, value: u64) -> Result<Js, E> {
        Ok(Js::Num(value as f64))
    }

    fn visit_f64<E>(self, value: f64) -> Result<Js, E> {
        Ok(Js::Num(value))
    }

    fn visit_str<E>(self, value: &str) -> Result<Js, E> {
        Ok(Js::Str(value.to_owned()))
    }

    fn visit_string<E>(self, value: String) -> Result<Js, E> {
        Ok(Js::Str(value))
    }

    fn visit_unit<E>(self) -> Result<Js, E> {
        Ok(Js::Null)
    }

    fn visit_none<E>(self) -> Result<Js, E> {
        Ok(Js::Null)
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut sequence: A) -> Result<Js, A::Error> {
        let mut items = Vec::new();
        while let Some(item) = sequence.next_element()? {
            items.push(item);
        }
        Ok(Js::Arr(items))
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Js, A::Error> {
        // Duplicate keys behave as in JSON.parse: the last value wins at the first position.
        let mut object = Obj::new();
        while let Some((key, value)) = map.next_entry::<String, Js>()? {
            object.insert(key, value);
        }
        Ok(Js::Obj(object))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_print_like_javascript() {
        let cases = [
            (1.0, "1"),
            (-0.0, "0"),
            (1.5, "1.5"),
            (100.0, "100"),
            (0.1, "0.1"),
            (0.000_001, "0.000001"),
            (0.000_000_1, "1e-7"),
            (1e21, "1e+21"),
            (1.25e21, "1.25e+21"),
            (123_456_789_012_345_680_000.0, "123456789012345680000"),
            (-2.5e-8, "-2.5e-8"),
            (9_007_199_254_740_993.0, "9007199254740992"),
        ];
        for (value, expected) in cases {
            assert_eq!(number_to_string(value), expected, "{value:e}");
        }
    }

    #[test]
    fn array_index_keys_enumerate_first_like_javascript_objects() {
        let parsed = parse(r#"{"b":1,"10":2,"a":3,"2":4,"01":5,"4294967295":6}"#).unwrap();
        assert_eq!(
            stringify(&parsed),
            r#"{"2":4,"10":2,"b":1,"a":3,"01":5,"4294967295":6}"#
        );
        let duplicate = parse(r#"{"a":1,"b":2,"a":3}"#).unwrap();
        assert_eq!(stringify(&duplicate), r#"{"a":3,"b":2}"#);
    }

    #[test]
    fn stringify_matches_json_stringify_formatting() {
        let parsed =
            parse(r#"{"z":[],"y":{},"x":[1,{"k":"\u0001\"\\\n\u007f "}],"w":1.0}"#).unwrap();
        assert_eq!(
            stringify(&parsed),
            "{\"z\":[],\"y\":{},\"x\":[1,{\"k\":\"\\u0001\\\"\\\\\\n\u{7f}\u{2028}\"}],\"w\":1}"
        );
        assert_eq!(
            stringify_pretty(&parsed),
            "{\n  \"z\": [],\n  \"y\": {},\n  \"x\": [\n    1,\n    {\n      \"k\": \"\\u0001\\\"\\\\\\n\u{7f}\u{2028}\"\n    }\n  ],\n  \"w\": 1\n}"
        );
        assert_eq!(
            stable_stringify(&parsed),
            "{\"w\":1,\"x\":[1,{\"k\":\"\\u0001\\\"\\\\\\n\u{7f}\u{2028}\"}],\"y\":{},\"z\":[]}"
        );
    }

    #[test]
    fn property_access_follows_javascript_for_arrays_and_strings() {
        let value = parse(r#"{"list":["a","b"],"text":"hé"}"#).unwrap();
        let list = property(&value, "list").unwrap();
        assert_eq!(to_js_string(&property(&list, "1").unwrap()), "b");
        assert_eq!(to_js_string(&property(&list, "length").unwrap()), "2");
        assert!(property(&list, "01").is_none());
        let text = property(&value, "text").unwrap();
        assert_eq!(to_js_string(&property(&text, "length").unwrap()), "2");
        assert_eq!(to_js_string(&Js::Arr(vec![Js::Num(1.0), Js::Null])), "1,");
    }
}
