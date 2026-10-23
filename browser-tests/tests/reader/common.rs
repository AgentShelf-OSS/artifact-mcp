// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Neil Blackman
//! Helpers the reader cases share.

use std::time::Duration;

use browser_tests::{Error, Page, WaitState};
use serde_json::{Value, json};

/// A repository asset as text (`fs.readFileSync('assets/<name>')`).
pub fn asset(name: &str) -> Result<String, Error> {
    let path = browser_tests::repo_root().join("assets").join(name);
    std::fs::read_to_string(&path)
        .map_err(|error| Error::Launch(format!("{}: {error}", path.display())))
}

/// `new Promise(resolve => setTimeout(resolve, ms))` / `page.waitForTimeout(ms)`.
pub async fn wait(ms: u64) {
    tokio::time::sleep(Duration::from_millis(ms)).await;
}

/// `blocks.map(block => block.text)`.
pub fn texts(blocks: &Value) -> Vec<String> {
    blocks
        .as_array()
        .into_iter()
        .flatten()
        .map(|block| block["text"].as_str().unwrap_or("").to_owned())
        .collect()
}

/// A JSON number (`NaN` when it is not one, so comparisons fail).
pub fn num(value: &Value) -> f64 {
    value.as_f64().unwrap_or(f64::NAN)
}

/// JavaScript truthiness.
pub fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(flag) => *flag,
        Value::Number(number) => number.as_f64().is_some_and(|n| n != 0.0 && !n.is_nan()),
        Value::String(text) => !text.is_empty(),
        _ => true,
    }
}

/// Wait for the reader toggle, open the reader and expand it.
pub async fn open_reader(page: &Page) -> Result<(), Error> {
    page.locator("#vreader-toggle")
        .wait_for(WaitState::Visible)
        .await?;
    page.locator("#vreader-toggle").click().await?;
    page.locator("#vreader-expand").click().await
}

/// The saved place (`artifact-reader-place:*` in `localStorage`), parsed, or `null`.
pub async fn checkpoint(page: &Page) -> Result<Value, Error> {
    page.evaluate(
        "(() => { const k = Object.keys(localStorage).find(k => k.startsWith('artifact-reader-place:')); return k ? JSON.parse(localStorage.getItem(k)) : null; })()",
    )
    .await
}

/// `document.querySelector('#vreader-status').textContent`.
pub async fn status(page: &Page) -> Result<String, Error> {
    Ok(page
        .locator("#vreader-status")
        .text_content()
        .await?
        .unwrap_or_default())
}

/// An init script that stores reader preferences in the top window's `localStorage`.
pub fn preferences_script(voice: &str, rate: &str) -> String {
    format!(
        "(() => {{ if (window.top === window) localStorage.setItem('artifact-reader-preferences', JSON.stringify({})); }})()",
        json!({"voice": voice, "rate": rate})
    )
}

/// The page's uncaught errors (`page.on('pageerror')`).
pub fn errors(page: &Page) -> Vec<String> {
    page.page_errors()
}

/// The UTF-16 index of `needle` in `haystack` (JavaScript's `indexOf`), with its byte index.
pub fn index_of(haystack: &str, needle: &str) -> Option<(usize, usize)> {
    let byte = haystack.find(needle)?;
    Some((haystack[..byte].encode_utf16().count(), byte))
}

/// JavaScript's `string.length`.
pub fn js_length(text: &str) -> usize {
    text.encode_utf16().count()
}

/// A JSON object with its keys in the given order (`JSON.stringify` keeps insertion order;
/// `serde_json` sorts).
pub fn ordered(pairs: &[(&str, Value)]) -> String {
    let fields: Vec<String> = pairs
        .iter()
        .map(|(name, value)| format!("{}:{}", json!(name), js_number(value)))
        .collect();
    format!("{{{}}}", fields.join(","))
}

/// Deep equality with JavaScript number semantics (`1` equals `1.0`), as `assert.deepEqual`
/// compares values parsed from JSON.
pub fn same(left: &Value, right: &Value) -> bool {
    match (left, right) {
        (Value::Number(a), Value::Number(b)) => a.as_f64() == b.as_f64(),
        (Value::Array(a), Value::Array(b)) => {
            a.len() == b.len() && a.iter().zip(b).all(|(a, b)| same(a, b))
        }
        (Value::Object(a), Value::Object(b)) => {
            a.len() == b.len()
                && a.iter()
                    .all(|(key, value)| b.get(key).is_some_and(|other| same(value, other)))
        }
        _ => left == right,
    }
}

/// A whole float as an integer, as JavaScript prints it (`3`, not `3.0`).
fn js_number(value: &Value) -> Value {
    match value.as_f64() {
        Some(number) if value.is_f64() && number.fract() == 0.0 && number.abs() < 1e15 => {
            json!(number as i64)
        }
        _ => value.clone(),
    }
}
