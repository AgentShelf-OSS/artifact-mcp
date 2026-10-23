// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Neil Blackman
//! JavaScript `Number(value)` and `Number.isSafeInteger` for JSON values and environment strings.
//!
//! The Node renderer parsed `PORT`, `RENDER_TIMEOUT_MS`, `MAX_BODY_BYTES`, `width` and `height` with
//! `Number(value)` followed by `Number.isSafeInteger(n) && n > 0`. Those rules accept more than a
//! plain integer: `"800"`, `" 800 "`, `"0x320"`, `8e2`, `[800]` and `true` (which is 1) all count.
//! This module reproduces them so a request that worked before still works, and one that fell back
//! to the default still falls back.

use serde_json::Value;

/// `Number.MAX_SAFE_INTEGER`.
const MAX_SAFE_INTEGER: f64 = 9_007_199_254_740_991.0;

/// `ToPrimitive` threw a `TypeError`. That happens when a JSON object has an own `toString`
/// property: it shadows `Object.prototype.toString` with a value that is not callable. Node threw
/// inside the request handler and answered `500`, so callers map this to a render failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TypeError;

/// `positiveInteger(value, fallback)` from the Node renderer, for a value that is already a number.
pub fn positive_integer(number: f64, fallback: u64) -> u64 {
    if is_safe_integer(number) && number > 0.0 {
        // A safe integer is exactly representable and below 2^53, so the cast is lossless.
        number as u64
    } else {
        fallback
    }
}

/// `positiveInteger(process.env.NAME, fallback)`: an unset variable is `undefined`, which is `NaN`.
pub fn env_positive_integer(value: Option<&str>, fallback: u64) -> u64 {
    match value {
        Some(text) => positive_integer(string_to_number(text), fallback),
        None => fallback,
    }
}

/// `Math.min(positiveInteger(value, fallback), maximum)` for a JSON request field. `None` is a
/// missing field (`undefined`, so the fallback).
pub fn bounded_dimension(
    value: Option<&Value>,
    fallback: u32,
    maximum: u32,
) -> Result<u32, TypeError> {
    let number = match value {
        Some(value) => to_number(value)?,
        None => f64::NAN,
    };
    let parsed = positive_integer(number, u64::from(fallback));
    Ok(parsed.min(u64::from(maximum)) as u32)
}

/// `Number.isSafeInteger(number)`.
pub fn is_safe_integer(number: f64) -> bool {
    number.is_finite() && number.trunc() == number && number.abs() <= MAX_SAFE_INTEGER
}

/// `Number(value)` for a value produced by `JSON.parse`.
pub fn to_number(value: &Value) -> Result<f64, TypeError> {
    match value {
        Value::Null => Ok(0.0),
        Value::Bool(flag) => Ok(if *flag { 1.0 } else { 0.0 }),
        Value::Number(number) => Ok(number.as_f64().unwrap_or(f64::NAN)),
        Value::String(text) => Ok(string_to_number(text)),
        // `ToPrimitive(array)` is `array.join(",")`. Two or more elements always produce a comma,
        // which no numeric literal contains, so only the empty and one-element cases can be numbers.
        Value::Array(items) => match items.as_slice() {
            [] => Ok(0.0),
            [only] => single_element_to_number(only),
            _ => {
                // `join` still converts every element, so an element that throws still throws.
                for item in items {
                    element_to_string_check(item)?;
                }
                Ok(f64::NAN)
            }
        },
        Value::Object(map) => object_to_number(map),
    }
}

/// `Number(String(element))` for the only element of an array.
fn single_element_to_number(element: &Value) -> Result<f64, TypeError> {
    match element {
        // `join` writes null and undefined as the empty string, and `Number("")` is 0.
        Value::Null => Ok(0.0),
        Value::Bool(_) => Ok(f64::NAN),
        // `Number(String(n))` round-trips every finite number exactly.
        Value::Number(number) => Ok(number.as_f64().unwrap_or(f64::NAN)),
        Value::String(text) => Ok(string_to_number(text)),
        // `String(nested)` is `nested.join(",")`, which is what `to_number` already models.
        Value::Array(_) => to_number(element),
        Value::Object(map) => object_to_string_check(map).map(|()| f64::NAN),
    }
}

fn element_to_string_check(element: &Value) -> Result<(), TypeError> {
    match element {
        Value::Array(items) => items.iter().try_for_each(element_to_string_check),
        Value::Object(map) => object_to_string_check(map),
        _ => Ok(()),
    }
}

/// `ToPrimitive(object, number)`: `valueOf` (inherited, returns the object itself, or an own
/// non-callable value that is skipped), then `toString`. An own `toString` key is never callable in
/// JSON, so it leaves no method that returns a primitive and the conversion throws.
fn object_to_number(map: &serde_json::Map<String, Value>) -> Result<f64, TypeError> {
    object_to_string_check(map).map(|()| f64::NAN)
}

/// `ToPrimitive(object, string)` tries `toString` then `valueOf`; the inherited `valueOf` returns
/// the object, so an own `toString` key throws here as well. Otherwise the result is
/// `"[object Object]"`, which is `NaN`.
fn object_to_string_check(map: &serde_json::Map<String, Value>) -> Result<(), TypeError> {
    if map.contains_key("toString") {
        Err(TypeError)
    } else {
        Ok(())
    }
}

/// ECMAScript `StringToNumber`.
pub fn string_to_number(text: &str) -> f64 {
    let trimmed = text.trim_matches(is_js_whitespace);
    if trimmed.is_empty() {
        return 0.0;
    }
    let bytes = trimmed.as_bytes();
    if bytes.len() > 2 && bytes[0] == b'0' {
        let radix = match bytes[1] {
            b'x' | b'X' => 16,
            b'o' | b'O' => 8,
            b'b' | b'B' => 2,
            _ => 0,
        };
        if radix != 0 {
            return non_decimal(&trimmed[2..], radix);
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
    if !is_str_unsigned_decimal_literal(unsigned) {
        return f64::NAN;
    }
    // The grammar check above excludes everything Rust accepts that JavaScript does not
    // ("inf", "nan", "infinity" in any case), and Rust's parser is correctly rounded, as is V8's.
    trimmed.parse::<f64>().unwrap_or(f64::NAN)
}

/// `StrUnsignedDecimalLiteral` without `Infinity`:
/// `digits [. digits?] [exp]` or `. digits [exp]`, where `exp` is `(e|E) [+|-] digits`.
fn is_str_unsigned_decimal_literal(text: &str) -> bool {
    let bytes = text.as_bytes();
    let mut index = 0;
    let integer_digits = count_digits(&bytes[index..]);
    index += integer_digits;
    let mut fraction_digits = 0;
    if bytes.get(index) == Some(&b'.') {
        index += 1;
        fraction_digits = count_digits(&bytes[index..]);
        index += fraction_digits;
    }
    if integer_digits == 0 && fraction_digits == 0 {
        return false;
    }
    if matches!(bytes.get(index), Some(b'e' | b'E')) {
        index += 1;
        if matches!(bytes.get(index), Some(b'+' | b'-')) {
            index += 1;
        }
        let exponent_digits = count_digits(&bytes[index..]);
        if exponent_digits == 0 {
            return false;
        }
        index += exponent_digits;
    }
    index == bytes.len()
}

fn count_digits(bytes: &[u8]) -> usize {
    bytes
        .iter()
        .take_while(|byte| byte.is_ascii_digit())
        .count()
}

/// `NonDecimalIntegerLiteral` digits after the `0x`, `0o` or `0b` prefix. No sign and no numeric
/// separators are allowed. Values above 2^53 lose precision, which is harmless: every caller only
/// accepts safe integers, and accumulation is monotonic, so an unsafe value never becomes safe.
fn non_decimal(digits: &str, radix: u32) -> f64 {
    let mut value = 0.0_f64;
    for character in digits.chars() {
        match character.to_digit(radix) {
            Some(digit) => value = value * f64::from(radix) + f64::from(digit),
            None => return f64::NAN,
        }
    }
    value
}

/// ECMAScript `WhiteSpace` and `LineTerminator` code points, which `StringToNumber` trims.
fn is_js_whitespace(character: char) -> bool {
    matches!(
        character,
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn number(value: Value) -> f64 {
        to_number(&value).expect("conversion must not throw")
    }

    #[test]
    fn string_to_number_matches_ecmascript() {
        let cases: &[(&str, f64)] = &[
            ("", 0.0),
            ("   ", 0.0),
            ("800", 800.0),
            (" \t\n800\u{00A0}\u{FEFF}", 800.0),
            ("+800", 800.0),
            ("-800", -800.0),
            ("8e2", 800.0),
            ("8E+2", 800.0),
            ("80000e-2", 800.0),
            ("800.", 800.0),
            (".5", 0.5),
            ("0x320", 800.0),
            ("0X320", 800.0),
            ("0o1440", 800.0),
            ("0b1100100000", 800.0),
            ("Infinity", f64::INFINITY),
            ("-Infinity", f64::NEG_INFINITY),
            ("00800", 800.0),
        ];
        for (text, expected) in cases {
            assert_eq!(string_to_number(text), *expected, "Number({text:?})");
        }
        for text in [
            "0x",
            "0x1g",
            "-0x10",
            "+0x10",
            "1_000",
            "1e",
            "1e+",
            ".",
            "e5",
            "inf",
            "infinity",
            "NaN",
            "nan",
            "INFINITY",
            "1,2",
            "12px",
            "true",
            "[object Object]",
            "0x 1",
            "1 2",
        ] {
            assert!(
                string_to_number(text).is_nan(),
                "Number({text:?}) must be NaN"
            );
        }
    }

    #[test]
    fn json_values_convert_like_number() {
        assert_eq!(number(json!(null)), 0.0);
        assert_eq!(number(json!(true)), 1.0);
        assert_eq!(number(json!(false)), 0.0);
        assert_eq!(number(json!(1200)), 1200.0);
        assert_eq!(number(json!(1200.0)), 1200.0);
        assert_eq!(number(json!(1.5)), 1.5);
        assert_eq!(number(json!("640")), 640.0);
        assert_eq!(number(json!([])), 0.0);
        assert_eq!(number(json!([640])), 640.0);
        assert_eq!(number(json!(["640"])), 640.0);
        assert_eq!(number(json!([[640]])), 640.0);
        assert_eq!(number(json!([null])), 0.0);
        assert!(number(json!([1, 2])).is_nan());
        assert!(number(json!([true])).is_nan());
        assert!(number(json!({})).is_nan());
        assert!(number(json!({"valueOf": 5})).is_nan());
        assert!(number(json!([{}])).is_nan());
        assert_eq!(to_number(&json!({"toString": 1})), Err(TypeError));
        assert_eq!(to_number(&json!([{"toString": 1}])), Err(TypeError));
        assert_eq!(to_number(&json!([1, {"toString": 1}])), Err(TypeError));
    }

    #[test]
    fn positive_integer_requires_a_positive_safe_integer() {
        assert_eq!(positive_integer(640.0, 1200), 640);
        assert_eq!(positive_integer(0.0, 1200), 1200);
        assert_eq!(positive_integer(-1.0, 1200), 1200);
        assert_eq!(positive_integer(1.5, 1200), 1200);
        assert_eq!(positive_integer(f64::NAN, 1200), 1200);
        assert_eq!(positive_integer(f64::INFINITY, 1200), 1200);
        assert_eq!(
            positive_integer(MAX_SAFE_INTEGER, 1),
            MAX_SAFE_INTEGER as u64
        );
        assert_eq!(positive_integer(MAX_SAFE_INTEGER + 1.0, 1), 1);
    }

    #[test]
    fn bounded_dimension_clamps_and_falls_back() {
        assert_eq!(bounded_dimension(None, 1200, 1920), Ok(1200));
        assert_eq!(bounded_dimension(Some(&json!(800)), 1200, 1920), Ok(800));
        assert_eq!(bounded_dimension(Some(&json!(5000)), 1200, 1920), Ok(1920));
        assert_eq!(bounded_dimension(Some(&json!(0)), 1200, 1920), Ok(1200));
        assert_eq!(bounded_dimension(Some(&json!(-5)), 1200, 1920), Ok(1200));
        assert_eq!(bounded_dimension(Some(&json!(99.5)), 1200, 1920), Ok(1200));
        assert_eq!(bounded_dimension(Some(&json!("700")), 1200, 1920), Ok(700));
        assert_eq!(bounded_dimension(Some(&json!(true)), 1200, 1920), Ok(1));
        assert_eq!(bounded_dimension(Some(&json!(1e300)), 1200, 1920), Ok(1200));
        assert_eq!(
            bounded_dimension(Some(&json!({"toString": 1})), 1200, 1920),
            Err(TypeError)
        );
    }

    #[test]
    fn env_values_parse_like_process_env() {
        assert_eq!(env_positive_integer(None, 3000), 3000);
        assert_eq!(env_positive_integer(Some(""), 3000), 3000);
        assert_eq!(env_positive_integer(Some("8080"), 3000), 8080);
        assert_eq!(env_positive_integer(Some("0x1F90"), 3000), 8080);
        assert_eq!(env_positive_integer(Some("abc"), 3000), 3000);
        assert_eq!(env_positive_integer(Some("-1"), 3000), 3000);
    }
}
