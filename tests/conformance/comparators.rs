// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Neil Blackman
//
// Comparison modes from blueprint B3. Each mode is a pure "normalizer": it turns a raw
// observation (response bytes, header map, SQL rows, files) into a canonical, JSON-serializable
// form. That SAME form is what record mode writes as the golden and what a replay compares
// against, so recording and checking can never drift apart.
//
// Modes:
//   exact-bytes     Raw artifacts, downloads, static assets, selected HTML snapshots.
//   exact-header    Header names lowercased; transport-only headers stripped; values exact.
//   canonical-json  Recursive key-sort; array order significant.
//   exact-json-text MCP content[0].text embedded JSON + tools/list golden (exact string).
//   state           Named SQL query rows + directory entries + sha256 file hashes.
//   html-dom        Large trusted pages: exact snapshot primary (DOM checks are secondary).
//   viewer-handle   The decoded viewer identity on the shell-config element.

use base64::Engine as _;
use sha2::{Digest, Sha256};

use crate::http::{HeaderValue, Headers};
use crate::js::{self, Js, Obj};

/// Transport-only headers are removed before comparison: they vary per connection and per run
/// and say nothing about application behavior.
pub const TRANSPORT_HEADERS: [&str; 6] = [
    "date",
    "server",
    "content-length",
    "connection",
    "keep-alive",
    "transfer-encoding",
];

const VOLATILE: &str = "<volatile>";

pub fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// The characters JavaScript's regular-expression `\s` (and `String.prototype.trim`) match.
pub fn is_js_space(character: char) -> bool {
    matches!(
        character,
        '\t' | '\n' | '\u{0b}' | '\u{0c}' | '\r' | ' ' | '\u{a0}' | '\u{1680}' | '\u{2000}'
            ..='\u{200a}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202f}'
                | '\u{205f}'
                | '\u{3000}'
                | '\u{feff}'
    )
}

fn skip_js_space(text: &str, from: usize) -> usize {
    text[from..]
        .char_indices()
        .find(|(_, character)| !is_js_space(*character))
        .map_or(text.len(), |(offset, _)| from + offset)
}

// --- Volatile fields ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
enum VolatileValue {
    String,
    Number,
}

/// Replace every property whose KEY appears in `fields`, anywhere in the tree, with a fixed
/// sentinel. This is how a case declares "this timestamp is allowed to move".
fn blank_volatile(value: &Js, fields: &[String]) -> Js {
    if fields.is_empty() {
        return value.clone();
    }
    let mut unique: Vec<String> = Vec::new();
    for field in fields {
        if !unique.contains(field) {
            unique.push(field.clone());
        }
    }
    walk_volatile(value, &unique)
}

fn walk_volatile(node: &Js, fields: &[String]) -> Js {
    match node {
        Js::Arr(items) => Js::Arr(
            items
                .iter()
                .map(|item| walk_volatile(item, fields))
                .collect(),
        ),
        Js::Obj(object) => {
            let mut out = Obj::new();
            for (key, value) in object.iter() {
                let blanked = if fields.contains(key) {
                    Js::str(VOLATILE)
                } else {
                    walk_volatile(value, fields)
                };
                out.insert(key.clone(), blanked);
            }
            Js::Obj(out)
        }
        // MCP embeds its payload as JSON *inside* a string (`result.content[].text`). A tree walk
        // sees that as an opaque leaf, so a volatile timestamp in there would survive and make the
        // golden unreproducible. Re-serializing the inner JSON would destroy the byte-exactness that
        // `exact-json-text` exists to protect, so substitute the value in place and leave every
        // other byte (including key order and spacing) untouched.
        Js::Str(text) if text.contains("\":") => Js::Str(blank_inside_embedded_json(text, fields)),
        other => other.clone(),
    }
}

/// For each field: `("FIELD"\s*:\s*)"[^"\\]*"` and then
/// `("FIELD"\s*:\s*)-?\d+(?:\.\d+)?(?:[eE][+-]?\d+)?` are replaced with `$1"<volatile>"`.
fn blank_inside_embedded_json(text: &str, fields: &[String]) -> String {
    let mut out = text.to_owned();
    for field in fields {
        out = replace_embedded_value(&out, field, VolatileValue::String);
        // Numeric volatile values (for example an epoch-millisecond `fire_at`) are blanked too.
        out = replace_embedded_value(&out, field, VolatileValue::Number);
    }
    out
}

fn replace_embedded_value(text: &str, field: &str, kind: VolatileValue) -> String {
    let needle = format!("\"{field}\"");
    let mut out = String::with_capacity(text.len());
    let mut copied = 0;
    let mut search = 0;
    while let Some(offset) = text[search..].find(&needle) {
        let start = search + offset;
        match match_embedded_value(text, start + needle.len(), kind) {
            Some((value_start, end)) => {
                out.push_str(&text[copied..value_start]);
                out.push('"');
                out.push_str(VOLATILE);
                out.push('"');
                copied = end;
                search = end;
            }
            // The needle starts with an ASCII quote, so `start + 1` is a character boundary.
            None => search = start + 1,
        }
    }
    out.push_str(&text[copied..]);
    out
}

/// Match `\s*:\s*VALUE` at `from`; return the byte range of VALUE.
fn match_embedded_value(text: &str, from: usize, kind: VolatileValue) -> Option<(usize, usize)> {
    let colon = skip_js_space(text, from);
    if !text[colon..].starts_with(':') {
        return None;
    }
    let value_start = skip_js_space(text, colon + 1);
    let bytes = text.as_bytes();
    let digit_at = |index: usize| bytes.get(index).is_some_and(u8::is_ascii_digit);
    match kind {
        VolatileValue::String => {
            if bytes.get(value_start) != Some(&b'"') {
                return None;
            }
            let mut end = value_start + 1;
            while end < bytes.len() && bytes[end] != b'"' && bytes[end] != b'\\' {
                end += 1;
            }
            (bytes.get(end) == Some(&b'"')).then_some((value_start, end + 1))
        }
        VolatileValue::Number => {
            let mut end = value_start;
            if bytes.get(end) == Some(&b'-') {
                end += 1;
            }
            let digits = end;
            while digit_at(end) {
                end += 1;
            }
            if end == digits {
                return None;
            }
            if bytes.get(end) == Some(&b'.') && digit_at(end + 1) {
                end += 1;
                while digit_at(end) {
                    end += 1;
                }
            }
            if matches!(bytes.get(end), Some(b'e' | b'E')) {
                let mut exponent = end + 1;
                if matches!(bytes.get(exponent), Some(b'+' | b'-')) {
                    exponent += 1;
                }
                let exponent_digits = exponent;
                while digit_at(exponent) {
                    exponent += 1;
                }
                if exponent > exponent_digits {
                    end = exponent;
                }
            }
            Some((value_start, end))
        }
    }
}

// --- Back-substitution -------------------------------------------------------------------------

/// Back-substitute high-entropy captured values (artifact ids, share tokens) with their symbol
/// name so goldens are run-independent. Only applied to captures, never to the low-entropy
/// constants (org names, emails), which stay literal.
pub fn backsub_string(text: &str, captures: &Obj) -> String {
    let mut entries: Vec<(&String, &Js, String)> = captures
        .iter()
        .map(|(name, value)| (name, value, js::to_js_string(value)))
        .collect();
    // Longest values first so a value that is a prefix of another cannot mis-replace. The sort is
    // stable, so equal lengths keep capture order, as Array.prototype.sort does.
    entries.sort_by_key(|entry| std::cmp::Reverse(js::utf16_len(&entry.2)));
    let mut out = text.to_owned();
    for (name, value, needle) in entries {
        if !value.truthy() {
            continue;
        }
        out = split_join(&out, &needle, &format!("${{{name}}}"));
    }
    out
}

/// `text.split(needle).join(replacement)`.
fn split_join(text: &str, needle: &str, replacement: &str) -> String {
    if needle.is_empty() {
        return text
            .chars()
            .map(String::from)
            .collect::<Vec<_>>()
            .join(replacement);
    }
    text.replace(needle, replacement)
}

// --- Body normalizers --------------------------------------------------------------------------

/// Return the JSON-serializable "normal form" for storage and comparison, or an error on a hard
/// shape problem (for example a body that is not JSON when JSON was expected).
pub fn normalize_body(
    mode: &str,
    body: &[u8],
    captures: &Obj,
    volatile_fields: &[String],
) -> Result<Js, String> {
    let with_mode = || Obj::new().with("mode", Js::str(mode));
    match mode {
        "viewer-handle" => {
            let viewer = shell_viewer_handle(&String::from_utf8_lossy(body))?;
            Ok(Js::Obj(with_mode().with("viewer", Js::Obj(viewer))))
        }
        "canonical-json" | "exact-json-text" => {
            let text = String::from_utf8_lossy(body);
            let parsed = js::parse(&text).map_err(|error| {
                format!("expected JSON body for mode {mode} but parse failed: {error}")
            })?;
            let blanked = blank_volatile(&parsed, volatile_fields);
            // Canonicalize, then back-substitute captured ids/tokens inside string leaves.
            let canonical_text = backsub_string(&js::stable_stringify(&blanked), captures);
            let json = js::parse(&canonical_text)
                .map_err(|error| format!("back-substituted body is not JSON: {error}"))?;
            if mode == "exact-json-text" {
                // Every MCP content[].text that looks like JSON must be valid JSON (the embedded
                // contract). Purely a shape guard; the value comparison is the canonical JSON.
                assert_embedded_text_json(&parsed)?;
            }
            Ok(Js::Obj(with_mode().with("json", json)))
        }
        "exact-bytes" | "html-dom" => {
            // Prefer readable UTF-8 when the body is text; fall back to base64 for binary (png).
            if looks_textual(body) {
                let data = backsub_string(&String::from_utf8_lossy(body), captures);
                return Ok(Js::Obj(
                    with_mode()
                        .with("encoding", Js::str("utf8"))
                        .with("data", Js::Str(data)),
                ));
            }
            Ok(Js::Obj(
                with_mode()
                    .with("encoding", Js::str("base64"))
                    .with("sha256", Js::Str(sha256_hex(body)))
                    .with(
                        "data",
                        Js::Str(base64::engine::general_purpose::STANDARD.encode(body)),
                    ),
            ))
        }
        other => Err(format!("unknown body mode: {other}")),
    }
}

/// Text if there is no NUL byte and UTF-8 decoding needs no replacement character.
fn looks_textual(body: &[u8]) -> bool {
    !body.contains(&0) && !String::from_utf8_lossy(body).contains('\u{fffd}')
}

fn assert_embedded_text_json(parsed: &Js) -> Result<(), String> {
    let messages = match parsed {
        Js::Arr(items) => items.iter().collect::<Vec<_>>(),
        other => vec![other],
    };
    for message in messages {
        let Some(result) = js::property(message, "result").filter(Js::truthy) else {
            continue;
        };
        let Some(Js::Arr(content)) = js::property(&result, "content") else {
            continue;
        };
        for part in content.iter().filter(|part| part.truthy()) {
            if part.get("type").and_then(Js::as_str) != Some("text") {
                continue;
            }
            let Some(text) = part.get("text").and_then(Js::as_str) else {
                continue;
            };
            let trimmed = text.trim_matches(is_js_space);
            if trimmed.starts_with('{') || trimmed.starts_with('[') {
                js::parse(text).map_err(|error| {
                    format!("embedded MCP content text is not valid JSON: {error}")
                })?;
            }
        }
    }
    Ok(())
}

// --- Viewer handle -----------------------------------------------------------------------------

/// Compare the server-rendered identity contract independently of the page chrome. Attributes
/// contain escaped JSON string literals, not raw identity strings.
fn shell_viewer_handle(html: &str) -> Result<Obj, String> {
    let mut configs: Vec<Obj> = Vec::new();
    for tag in div_tags(html) {
        let mut attributes = Obj::new();
        for (name, raw) in tag_attributes(tag) {
            if attributes.contains_key(&name) {
                return Err(format!("duplicate shell attribute: {name}"));
            }
            attributes.insert(name, Js::Str(decode_entities(raw)?));
        }
        if attributes.get("id").and_then(Js::as_str) == Some("shell-config") {
            configs.push(attributes);
        }
    }
    if configs.len() != 1 {
        return Err("expected one shell-config element".to_owned());
    }
    let config = &configs[0];
    let literal = |name: &str| -> Result<Js, String> {
        let raw = config
            .get(name)
            .and_then(Js::as_str)
            .ok_or_else(|| format!("{name} is missing"))?;
        js::parse(raw).map_err(|error| format!("{name} is not a JSON literal: {error}"))
    };
    let id = literal("data-viewer-id")?;
    let name = literal("data-viewer-name")?;
    let valid_id = id.as_str().is_some_and(|id| {
        id.len() == 16
            && id
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    });
    if !valid_id {
        return Err("invalid viewer handle id".to_owned());
    }
    let valid_name = name.as_str().is_some_and(|name| {
        name.chars().count() <= 40
            && !name
                .chars()
                .any(|character| matches!(u32::from(character), 0x00..=0x1f | 0x7f..=0x9f))
    });
    if !valid_name {
        return Err("invalid viewer handle name".to_owned());
    }
    Ok(Obj::new().with("id", id).with("name", name))
}

/// Every match of `/<div\b[^>]*>/gi`.
fn div_tags(html: &str) -> Vec<&str> {
    let bytes = html.as_bytes();
    let mut tags = Vec::new();
    let mut index = 0;
    while index + 4 <= bytes.len() {
        let opens =
            bytes[index] == b'<' && bytes[index + 1..index + 4].eq_ignore_ascii_case(b"div");
        let after = index + 4;
        let boundary = bytes
            .get(after)
            .is_none_or(|byte| !(byte.is_ascii_alphanumeric() || *byte == b'_'));
        if opens && boundary {
            match html[after..].find('>') {
                Some(close) => {
                    let end = after + close + 1;
                    tags.push(&html[index..end]);
                    index = end;
                    continue;
                }
                // No later `>` exists, so no later match can exist either.
                None => break,
            }
        }
        index += 1;
    }
    tags
}

/// Every match of `/\s([^\s=/>]+)\s*=\s*(?:"([^"]*)"|'([^']*)')/g` as (name, raw value).
fn tag_attributes(tag: &str) -> Vec<(String, &str)> {
    let characters: Vec<(usize, char)> = tag.char_indices().collect();
    let at = |index: usize| characters.get(index).map(|(_, character)| *character);
    let offset = |index: usize| {
        characters
            .get(index)
            .map_or(tag.len(), |(offset, _)| *offset)
    };
    let mut attributes = Vec::new();
    let mut index = 0;
    while index < characters.len() {
        let matched = (|| {
            if !at(index).is_some_and(is_js_space) {
                return None;
            }
            let name_start = index + 1;
            let mut cursor = name_start;
            while at(cursor).is_some_and(|c| !(is_js_space(c) || matches!(c, '=' | '/' | '>'))) {
                cursor += 1;
            }
            if cursor == name_start {
                return None;
            }
            let name = tag[offset(name_start)..offset(cursor)].to_owned();
            while at(cursor).is_some_and(is_js_space) {
                cursor += 1;
            }
            if at(cursor) != Some('=') {
                return None;
            }
            cursor += 1;
            while at(cursor).is_some_and(is_js_space) {
                cursor += 1;
            }
            let quote = at(cursor).filter(|c| matches!(c, '"' | '\''))?;
            let value_start = cursor + 1;
            let mut close = value_start;
            while at(close).is_some_and(|c| c != quote) {
                close += 1;
            }
            at(close)?;
            Some((name, &tag[offset(value_start)..offset(close)], close + 1))
        })();
        match matched {
            Some((name, value, next)) => {
                attributes.push((name, value));
                index = next;
            }
            None => index += 1,
        }
    }
    attributes
}

/// Decode `/&(#x[0-9a-f]+|#\d+|quot|apos|amp|lt|gt);/gi`.
fn decode_entities(value: &str) -> Result<String, String> {
    let mut out = String::with_capacity(value.len());
    let mut rest = value;
    while let Some(at) = rest.find('&') {
        out.push_str(&rest[..at]);
        let after = &rest[at + 1..];
        match match_entity(after)? {
            Some((decoded, consumed)) => {
                out.push(decoded);
                rest = &after[consumed..];
            }
            None => {
                out.push('&');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    Ok(out)
}

fn match_entity(text: &str) -> Result<Option<(char, usize)>, String> {
    let bytes = text.as_bytes();
    let code_point = |digits: &str, radix: u32| -> Result<char, String> {
        u32::from_str_radix(digits, radix)
            .ok()
            .and_then(char::from_u32)
            .ok_or_else(|| format!("invalid character reference: {digits}"))
    };
    if bytes.first() == Some(&b'#') {
        if matches!(bytes.get(1), Some(b'x' | b'X')) {
            let digits = bytes[2..]
                .iter()
                .take_while(|b| b.is_ascii_hexdigit())
                .count();
            if digits > 0 && bytes.get(2 + digits) == Some(&b';') {
                return Ok(Some((code_point(&text[2..2 + digits], 16)?, 3 + digits)));
            }
            return Ok(None);
        }
        let digits = bytes[1..].iter().take_while(|b| b.is_ascii_digit()).count();
        if digits > 0 && bytes.get(1 + digits) == Some(&b';') {
            return Ok(Some((code_point(&text[1..1 + digits], 10)?, 2 + digits)));
        }
        return Ok(None);
    }
    for (name, decoded) in [
        ("quot", '"'),
        ("apos", '\''),
        ("amp", '&'),
        ("lt", '<'),
        ("gt", '>'),
    ] {
        let length = name.len();
        if bytes.len() > length
            && bytes[..length].eq_ignore_ascii_case(name.as_bytes())
            && bytes[length] == b';'
        {
            return Ok(Some((decoded, length + 1)));
        }
    }
    Ok(None)
}

// --- Header normalizer -------------------------------------------------------------------------

/// The response headers as one flat string per name (`node:http` joins arrays with ", ").
pub fn flat_headers(headers: &Headers) -> Vec<(String, String)> {
    headers
        .iter()
        .map(|(name, value)| {
            let flat = match value {
                HeaderValue::One(value) => value.clone(),
                HeaderValue::Many(values) => values.join(", "),
            };
            (name.clone(), flat)
        })
        .collect()
}

pub fn normalize_headers(headers: &Headers, captures: &Obj) -> Js {
    let mut kept: Vec<(String, String)> = flat_headers(headers)
        .into_iter()
        .filter(|(name, _)| !TRANSPORT_HEADERS.contains(&name.as_str()))
        .map(|(name, value)| (name, backsub_string(&value, captures)))
        .collect();
    // Sorted object for stable serialization.
    kept.sort_by(|a, b| js::cmp_utf16(&a.0, &b.0));
    let mut sorted = Obj::new();
    for (name, value) in kept {
        sorted.insert(name, Js::Str(value));
    }
    Js::Obj(
        Obj::new()
            .with("mode", Js::str("exact-header"))
            .with("headers", Js::Obj(sorted)),
    )
}

// --- Structural compare ------------------------------------------------------------------------

pub fn deep_equal(a: &Js, b: &Js) -> bool {
    js::stable_stringify(a) == js::stable_stringify(b)
}

/// A compact human-readable diff for a failed comparison.
pub fn describe_diff(expected: &Js, actual: &Js) -> String {
    format!(
        "  expected: {}\n  actual:   {}",
        js::stable_stringify(expected),
        js::stable_stringify(actual)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn viewer(html: &str) -> Result<String, String> {
        normalize_body("viewer-handle", html.as_bytes(), &Obj::new(), &[])
            .map(|normal| js::stringify(&normal))
    }

    // Ported from test/conformance-viewer-config.test.js.
    #[test]
    fn viewer_handle_compares_decoded_shell_attributes_without_unrelated_markup() {
        let node = r#"<main>Node chrome</main><div id="shell-config" data-viewer-id="&quot;8e91af3205d2c700&quot;" data-viewer-name="&quot;Alex &amp; Jo&quot;"></div>"#;
        let rust = r#"<div hidden data-viewer-name="&#34;Alex &#38; Jo&#34;" id="shell-config" data-viewer-id="&#x22;8e91af3205d2c700&#x22;"></div>"#;
        let expected =
            r#"{"mode":"viewer-handle","viewer":{"id":"8e91af3205d2c700","name":"Alex & Jo"}}"#;
        assert_eq!(viewer(node).unwrap(), expected);
        assert_eq!(viewer(rust).unwrap(), expected);
    }

    // Ported from test/conformance-viewer-config.test.js.
    #[test]
    fn viewer_handle_rejects_missing_or_malformed_identity_instead_of_recording_it() {
        for html in [
            r#"<div id="shell-config"></div>"#,
            r#"<div id="shell-config" data-viewer-id="&quot;bad&quot;" data-viewer-name="&quot;Alex&quot;"></div>"#,
            r#"<div id="shell-config" data-viewer-id="&quot;8e91af3205d2c700&quot;" data-viewer-name="null"></div>"#,
            r#"<div id="shell-config" id="shell-config"></div>"#,
            r#"<div id="shell-config"></div><DIV id='shell-config'></DIV>"#,
        ] {
            assert!(viewer(html).is_err(), "{html}");
        }
    }

    #[test]
    fn volatile_fields_are_blanked_in_the_tree_and_inside_embedded_json_text() {
        let body = br#"{"created_at":"x","list":[{"created_at":{"deep":1}}],"text":"{\"created_at\" : \"2026\",\"fire_at\":-1.5e3,\"keep\":\"a\\\"b\",\"created_at\":\"a\\\"b\"}"}"#;
        let normal = normalize_body(
            "canonical-json",
            body,
            &Obj::new(),
            &["created_at".to_owned(), "fire_at".to_owned()],
        )
        .unwrap();
        assert_eq!(
            js::stringify(&normal),
            r#"{"mode":"canonical-json","json":{"created_at":"<volatile>","list":[{"created_at":"<volatile>"}],"text":"{\"created_at\" : \"<volatile>\",\"fire_at\":\"<volatile>\",\"keep\":\"a\\\"b\",\"created_at\":\"a\\\"b\"}"}}"#
        );
    }

    #[test]
    fn captures_are_back_substituted_longest_first() {
        let captures = Obj::new()
            .with("short", Js::str("abc"))
            .with("long", Js::str("abcdef"))
            .with("empty", Js::str(""));
        assert_eq!(backsub_string("abcdef abc", &captures), "${long} ${short}");
    }

    #[test]
    fn binary_bodies_are_stored_as_base64_with_a_digest() {
        let normal = normalize_body("exact-bytes", &[0x89, b'P', 0, 1], &Obj::new(), &[]).unwrap();
        assert_eq!(
            js::stringify(&normal),
            format!(
                r#"{{"mode":"exact-bytes","encoding":"base64","sha256":"{}","data":"iVAAAQ=="}}"#,
                sha256_hex(&[0x89, b'P', 0, 1])
            )
        );
    }

    #[test]
    fn exact_json_text_rejects_invalid_embedded_json() {
        let body = br#"{"result":{"content":[{"type":"text","text":" {not json"}]}}"#;
        assert!(normalize_body("exact-json-text", body, &Obj::new(), &[]).is_err());
        assert!(normalize_body("canonical-json", body, &Obj::new(), &[]).is_ok());
    }
}
