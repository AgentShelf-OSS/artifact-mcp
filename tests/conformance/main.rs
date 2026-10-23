// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Neil Blackman
//
//! Language-neutral conformance oracle (blueprint B2/B3).
//!
//! ```sh
//! cargo test --test conformance                                  # replay every golden
//! CONFORMANCE_FILTER=raw cargo test --test conformance           # cases whose id or a tag contains "raw"
//! CONFORMANCE_RECORD=1 cargo test --test conformance             # regenerate goldens (never in CI)
//! ```
//!
//! For each case: copy the named fixture to a fresh temp DATA_DIR, start the server binary built
//! by this package on a random loopback port, wait for /health, execute the HTTP steps, normalize
//! only declared-volatile fields, compare per the case's declared modes, stop the server cleanly,
//! then run post-state SQL / file / directory assertions. Every case gets its own data dir.
//!
//! The case and golden JSON formats are unchanged from the retired Node runner
//! (`conformance/runner.mjs`); see `conformance/README.md`.

mod comparators;
mod http;
// Shared with tests/mcp_surface.rs, which uses a different subset of the helpers.
#[allow(dead_code)]
mod js;
mod server;

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use rusqlite::config::DbConfig;
use rusqlite::types::ValueRef;
use rusqlite::{Connection, OpenFlags};

use comparators::{
    backsub_string, deep_equal, describe_diff, flat_headers, normalize_body, normalize_headers,
    sha256_hex,
};
use js::{Js, Obj, to_js_string, truthy};
use server::Server;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
const VOLATILE: &str = "<volatile>";
/// `Number.MAX_SAFE_INTEGER`; node:sqlite refuses to read larger integers as numbers.
const MAX_SAFE_INTEGER: u64 = (1 << 53) - 1;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn conformance_dir() -> PathBuf {
    repo_root().join("conformance")
}

// --- Well-known test credentials ---------------------------------------------------------------
//
// These are the ONLY publisher keys and viewer identities the oracle uses. They are seeded into
// every server through env at boot, so the same authenticated requests are answered
// deterministically. Secrets are long, fixed, and non-secret (a test fixture, never a deployment).
//
// ARTIFACT_API_KEYS entry format is "clientId:org:secret".
struct Credential {
    client_id: &'static str,
    org: &'static str,
    secret: &'static str,
}

const ACME: Credential = Credential {
    client_id: "pub-acme",
    org: "acme",
    secret: "conf-key-acme-000000000000",
};
const ACME2: Credential = Credential {
    client_id: "pub-acme2",
    org: "acme",
    secret: "conf-key-acme2-11111111111",
};
const BETA: Credential = Credential {
    client_id: "pub-beta",
    org: "beta",
    secret: "conf-key-beta-222222222222",
};
const ADMIN: Credential = Credential {
    client_id: "pub-admin",
    org: "admin",
    secret: "conf-key-admin-33333333333",
};
const CREDENTIALS: [&Credential; 4] = [&ACME, &ACME2, &BETA, &ADMIN];

const ADMIN_EMAIL: &str = "admin@corp.test";
const VIEWER_ACME: &str = "alice@acme.test";
const VIEWER_BETA: &str = "bob@beta.test";

/// A fixed public base so response `url` fields are stable regardless of the random port.
const PUBLIC_BASE: &str = "http://conformance.test";

/// Base environment applied to every server launch. Case-level env overrides win.
fn base_env() -> Obj {
    let keys = CREDENTIALS
        .iter()
        .map(|c| format!("{}:{}:{}", c.client_id, c.org, c.secret))
        .collect::<Vec<_>>()
        .join(",");
    Obj::new()
        .with("LISTEN_HOST", Js::str("127.0.0.1"))
        .with("TRUST_ACCESS_HEADERS", Js::str("1"))
        .with("ARTIFACT_API_KEYS", Js::Str(keys))
        .with("ADMIN_EMAILS", Js::str(ADMIN_EMAIL))
        .with(
            "ORG_EMAIL_DOMAINS",
            Js::str("acme.test:acme,beta.test:beta"),
        )
        .with("PUBLIC_BASE_URL", Js::str(PUBLIC_BASE))
        // Fixed non-secret test key so the server exercises the mandatory tamper-evident ledger.
        .with(
            "AUDIT_LEDGER_HMAC_KEY",
            Js::Str(base64::engine::general_purpose::STANDARD.encode([7_u8; 32])),
        )
        // Keep preview/thumbnail rendering off so no headless browser is required.
        .with("PREVIEW_RENDERER_URL", Js::str(""))
        .with("NODE_ENV", Js::str("test"))
}

/// Constant substitution symbols usable in case steps as ${name}. These are forward-only:
/// injected into requests but never back-substituted out of responses (they are low-entropy and
/// would corrupt goldens). High-entropy runtime captures (artifact ids, share tokens) are tracked
/// separately and DO get back-substituted.
fn constant_symbols() -> Obj {
    Obj::new()
        .with("key_acme", Js::str(ACME.secret))
        .with("key_acme2", Js::str(ACME2.secret))
        .with("key_beta", Js::str(BETA.secret))
        .with("key_admin", Js::str(ADMIN.secret))
        .with("client_acme", Js::str(ACME.client_id))
        .with("client_acme2", Js::str(ACME2.client_id))
        .with("client_beta", Js::str(BETA.client_id))
        .with("admin_email", Js::str(ADMIN_EMAIL))
        .with("viewer_acme", Js::str(VIEWER_ACME))
        .with("viewer_beta", Js::str(VIEWER_BETA))
        .with("public_base", Js::str(PUBLIC_BASE))
}

// --- Small JavaScript-semantics helpers --------------------------------------------------------

/// `value || []` for an array-valued case field.
fn items(value: Option<&Js>) -> &[Js] {
    match value {
        Some(Js::Arr(items)) => items,
        _ => &[],
    }
}

/// `String(value)`, where `None` is `undefined`.
fn show(value: Option<&Js>) -> String {
    value.map_or_else(|| "undefined".to_owned(), to_js_string)
}

/// `a !== b` for JSON-sourced values (two objects are never the same reference).
fn strictly_equal(a: Option<&Js>, b: Option<&Js>) -> bool {
    match (a, b) {
        (None, None) | (Some(Js::Null), Some(Js::Null)) => true,
        (Some(Js::Bool(a)), Some(Js::Bool(b))) => a == b,
        (Some(Js::Num(a)), Some(Js::Num(b))) => a == b,
        (Some(Js::Str(a)), Some(Js::Str(b))) => a == b,
        _ => false,
    }
}

/// `path.split(".").reduce((acc, key) => (acc == null ? undefined : acc[key]), value)`.
fn get_by_path(value: &Js, path: &str) -> Option<Js> {
    let mut current = Some(value.clone());
    for key in path.split('.') {
        current = match current {
            None | Some(Js::Null) => None,
            Some(node) => js::property(&node, key),
        };
    }
    current
}

/// The index an `expect.sameAsStep` value names, as `steps[sameAs]` would resolve it.
fn step_index(value: &Js) -> Option<usize> {
    match value {
        Js::Num(number) if *number >= 0.0 && number.fract() == 0.0 && *number < 4_294_967_295.0 => {
            Some(*number as usize)
        }
        Js::Str(text) => {
            let canonical = text == "0" || (!text.starts_with('0') && !text.is_empty());
            text.parse::<usize>().ok().filter(|_| canonical)
        }
        _ => None,
    }
}

// --- Temporary directories and ports -----------------------------------------------------------

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

struct TempDir(PathBuf);

impl TempDir {
    fn new(prefix: &str) -> Result<Self, String> {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_nanos());
        let path = std::env::temp_dir().join(format!(
            "{prefix}{}-{}-{nanos}",
            std::process::id(),
            TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).map_err(|error| format!("create {}: {error}", path.display()))?;
        Ok(Self(path))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Grab a free loopback TCP port by binding to 0 then releasing it.
fn free_port() -> Result<u16, String> {
    std::net::TcpListener::bind(("127.0.0.1", 0))
        .and_then(|listener| listener.local_addr())
        .map(|address| address.port())
        .map_err(|error| format!("find a free port: {error}"))
}

/// `path.join(base, relative)`: join, then resolve `.`, `..` and repeated separators lexically.
fn join_normalized(base: &Path, relative: &str) -> PathBuf {
    let joined = format!("{}/{relative}", base.display());
    let mut parts: Vec<&str> = Vec::new();
    for part in joined.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            other => parts.push(other),
        }
    }
    PathBuf::from(format!("/{}", parts.join("/")))
}

// --- Cases and goldens -------------------------------------------------------------------------

struct Case {
    id: String,
    definition: Js,
    steps: Vec<Js>,
}

impl Case {
    fn tags(&self) -> impl Iterator<Item = String> + '_ {
        items(self.definition.get("tags")).iter().map(to_js_string)
    }
}

/// Expand `roleMatrix` into one MCP read, write and delete step per role and target.
fn expand_role_matrix(definition: &Js) -> Result<Vec<Js>, String> {
    let mut expanded = Vec::new();
    for role_case in items(definition.get("roleMatrix")) {
        let role = show(role_case.get("role"));
        for target in items(role_case.get("targets")) {
            let scope = show(target.get("scope"));
            let with_id = |object: Obj| match target.get("id") {
                Some(id) => object.with("id", id.clone()),
                None => object,
            };
            let edit = Obj::new()
                .with("find", Js::str("seed"))
                .with("replace", Js::Str(format!("{role}-{scope}")));
            let operations = [
                ("read", "read_artifact", with_id(Obj::new())),
                (
                    "write",
                    "patch_artifact",
                    with_id(Obj::new())
                        .with("expected_revision", Js::Num(1.0))
                        .with("edits", Js::Arr(vec![Js::Obj(edit)])),
                ),
                ("delete", "delete_artifact", with_id(Obj::new())),
            ];
            for (operation, name, arguments) in operations {
                let outcomes = target
                    .get("outcomes")
                    .filter(|outcomes| !matches!(outcomes, Js::Null))
                    .ok_or_else(|| format!("roleMatrix target {scope} has no outcomes"))?;
                let outcome = js::property(outcomes, operation);
                let headers = Obj::new()
                    .with(
                        "authorization",
                        Js::Str(format!("Bearer {}", show(role_case.get("key")))),
                    )
                    .with("content-type", Js::str("application/json"));
                let params = Obj::new()
                    .with("name", Js::str(name))
                    .with("arguments", Js::Obj(arguments));
                let rpc = Obj::new()
                    .with("jsonrpc", Js::str("2.0"))
                    .with("id", Js::Num((expanded.len() + 100) as f64))
                    .with("method", Js::str("tools/call"))
                    .with("params", Js::Obj(params));
                let request = Obj::new()
                    .with("method", Js::str("POST"))
                    .with("path", Js::str("/mcp"))
                    .with("headers", Js::Obj(headers))
                    .with("json", Js::Obj(rpc));
                let mut expect = Obj::new().with("status", Js::Num(200.0));
                match outcome {
                    Some(Js::Str(ok)) if ok == "ok" => expect.insert("mcpSuccess", Js::Bool(true)),
                    Some(error) => expect.insert("mcpError", error),
                    None => {}
                }
                expanded.push(Js::Obj(
                    Obj::new()
                        .with("name", Js::Str(format!("{role} {scope} {operation}")))
                        .with("request", Js::Obj(request))
                        .with("expect", Js::Obj(expect)),
                ));
            }
        }
    }
    Ok(expanded)
}

fn load_cases() -> Result<Vec<Case>, String> {
    let directory = conformance_dir().join("cases");
    if !directory.exists() {
        return Ok(Vec::new());
    }
    let mut files: Vec<String> = fs::read_dir(&directory)
        .map_err(|error| format!("read {}: {error}", directory.display()))?
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.ends_with(".json"))
        .collect();
    files.sort_by(|a, b| js::cmp_utf16(a, b));
    files
        .into_iter()
        .map(|file| {
            let text = fs::read_to_string(directory.join(&file))
                .map_err(|error| format!("read case {file}: {error}"))?;
            let definition = js::parse(&text).map_err(|error| format!("case {file}: {error}"))?;
            let id = match definition.get("id").filter(|id| id.truthy()) {
                Some(id) => to_js_string(id),
                None => file.trim_end_matches(".json").to_owned(),
            };
            let mut steps = items(definition.get("steps")).to_vec();
            steps.extend(
                expand_role_matrix(&definition).map_err(|error| format!("case {file}: {error}"))?,
            );
            Ok(Case {
                id,
                definition,
                steps,
            })
        })
        .collect()
}

fn golden_path(case_id: &str) -> PathBuf {
    conformance_dir()
        .join("goldens")
        .join(format!("{case_id}.json"))
}

fn load_golden(case_id: &str) -> Result<Option<Js>, String> {
    let path = golden_path(case_id);
    if !path.exists() {
        return Ok(None);
    }
    let text = fs::read_to_string(&path).map_err(|error| format!("read golden: {error}"))?;
    js::parse(&text)
        .map(Some)
        .map_err(|error| format!("golden {}: {error}", path.display()))
}

/// Written exactly as `JSON.stringify(normalized, null, 2) + "\n"`.
fn write_golden(case_id: &str, normalized: &Js) -> Result<(), String> {
    let path = golden_path(case_id);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| format!("create goldens dir: {error}"))?;
    }
    fs::write(&path, format!("{}\n", js::stringify_pretty(normalized)))
        .map_err(|error| format!("write {}: {error}", path.display()))
}

// --- Substitution ------------------------------------------------------------------------------

/// Replace `${name}` (name = `[a-zA-Z0-9_]+`) with the symbol's value; unknown names stay.
fn subst_str(text: &str, symbols: &Obj) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let name_length = after
            .bytes()
            .take_while(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
            .count();
        if name_length > 0 && after.as_bytes().get(name_length) == Some(&b'}') {
            match symbols.get(&after[..name_length]) {
                Some(value) => out.push_str(&to_js_string(value)),
                None => out.push_str(&rest[start..start + name_length + 3]),
            }
            rest = &after[name_length + 1..];
        } else {
            // No match can start at the "{" either, so continue after "${".
            out.push_str("${");
            rest = after;
        }
    }
    out.push_str(rest);
    out
}

fn subst_deep(value: &Js, symbols: &Obj) -> Js {
    match value {
        Js::Str(text) => Js::Str(subst_str(text, symbols)),
        Js::Arr(items) => Js::Arr(items.iter().map(|item| subst_deep(item, symbols)).collect()),
        Js::Obj(object) => {
            let mut out = Obj::new();
            for (key, item) in object.iter() {
                out.insert(key.clone(), subst_deep(item, symbols));
            }
            Js::Obj(out)
        }
        other => other.clone(),
    }
}

// --- Request building --------------------------------------------------------------------------

struct BuiltRequest {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: Option<Vec<u8>>,
}

fn build_request(spec: Option<&Js>, symbols: &Obj) -> Result<BuiltRequest, String> {
    let spec = spec
        .filter(|spec| !matches!(spec, Js::Null))
        .ok_or("step has no request")?;
    let mut headers = Obj::new();
    if let Some(Js::Obj(declared)) = spec.get("headers") {
        for (name, value) in declared.iter() {
            headers.insert(
                name.to_lowercase(),
                Js::Str(subst_str(&to_js_string(value), symbols)),
            );
        }
    }
    let content_type = Js::str("application/json");
    let body = if let Some(generator) = spec.get("genBody").filter(|value| value.truthy()) {
        let body = generate_body(generator)?;
        if !headers.contains_key("content-type")
            && generator.get("kind").and_then(Js::as_str) != Some("raw")
        {
            headers.insert("content-type", content_type);
        }
        Some(body)
    } else if let Some(json) = spec.get("json") {
        if !headers.contains_key("content-type") {
            headers.insert("content-type", content_type);
        }
        Some(js::stringify(&subst_deep(json, symbols)).into_bytes())
    } else {
        spec.get("rawBody")
            .map(|raw| subst_str(&to_js_string(raw), symbols).into_bytes())
    };
    // node:http request helper: a payload gets a content-length unless the case set one.
    if let Some(body) = &body
        && !headers.contains_key("content-length")
    {
        headers.insert("content-length", Js::Str(body.len().to_string()));
    }
    Ok(BuiltRequest {
        method: spec
            .get("method")
            .filter(|method| method.truthy())
            .map_or_else(|| "GET".to_owned(), to_js_string),
        path: subst_str(&show(spec.get("path")), symbols),
        headers: headers
            .iter()
            .map(|(name, value)| (name.clone(), to_js_string(value)))
            .collect(),
        body,
    })
}

/// `"x".repeat(count)` argument conversion; a negative or infinite count is a RangeError.
fn repeat_count(value: Option<&Js>) -> Result<usize, String> {
    match value {
        Some(Js::Num(number)) if number.is_nan() => Ok(0),
        Some(Js::Num(number)) if *number < 0.0 || number.is_infinite() => Err(format!(
            "Invalid count value: {}",
            js::number_to_string(*number)
        )),
        Some(Js::Num(number)) => Ok(number.trunc() as usize),
        Some(Js::Bool(true)) => Ok(1),
        Some(Js::Str(text)) => Ok(text
            .trim()
            .parse::<f64>()
            .map_or(0, |n| n.max(0.0).trunc() as usize)),
        _ => Ok(0),
    }
}

fn generate_body(generator: &Js) -> Result<Vec<u8>, String> {
    let kind = generator.get("kind");
    match kind.and_then(Js::as_str) {
        Some("state-value") => {
            let character = generator
                .get("character")
                .filter(|value| value.truthy())
                .map_or_else(|| "x".to_owned(), to_js_string);
            let value = character.repeat(repeat_count(generator.get("length"))?);
            Ok(js::stringify(&Js::Obj(Obj::new().with("value", Js::Str(value)))).into_bytes())
        }
        Some("oversize-json") => {
            // A syntactically valid JSON object whose size exceeds the MCP JSON limit, to trip
            // 413 AFTER auth but at the body-buffering boundary.
            let requested = match generator.get("bytes").filter(|value| value.truthy()) {
                Some(bytes) => repeat_count(Some(bytes))?,
                None => 9_000_000,
            };
            let filler = "x".repeat(requested.max(1));
            Ok(format!(
                r#"{{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{{"name":"publish_artifact","arguments":{{"html":"{filler}"}}}}}}"#
            )
            .into_bytes())
        }
        Some("raw") => Ok(match generator.get("text") {
            None | Some(Js::Null) => Vec::new(),
            Some(text) => to_js_string(text).into_bytes(),
        }),
        _ => Err(format!("unknown genBody kind: {}", show(kind))),
    }
}

// --- Assertions (always evaluated, in record and compare) --------------------------------------

fn eval_step_assertions(
    step: &Js,
    response: &http::Response,
    captures: &Obj,
    label: &str,
) -> Vec<String> {
    let mut failures = Vec::new();
    let expect = step.get("expect").filter(|expect| expect.truthy());

    if let Some(status) = expect.and_then(|expect| expect.get("status"))
        && !strictly_equal(Some(status), Some(&Js::Num(f64::from(response.status))))
    {
        failures.push(format!(
            "{label}: status expected {}, got {}",
            to_js_string(status),
            response.status
        ));
    }

    let mcp_success = truthy(expect.and_then(|expect| expect.get("mcpSuccess")));
    let mcp_error = expect.and_then(|expect| expect.get("mcpError"));
    if mcp_success || mcp_error.is_some() {
        match js::parse(&String::from_utf8_lossy(&response.body)) {
            Err(_) => failures.push(format!(
                "{label}: MCP assertion requires a JSON response body"
            )),
            Ok(parsed) if parsed.truthy() => {
                let result = js::property(&parsed, "result");
                let error = js::property(&parsed, "error");
                let is_error = matches!(
                    result
                        .as_ref()
                        .and_then(|result| js::property(result, "isError")),
                    Some(Js::Bool(true))
                );
                let actual = result
                    .as_ref()
                    .and_then(|result| js::property(result, "content"))
                    .filter(|content| !matches!(content, Js::Null))
                    .and_then(|content| js::property(&content, "0"))
                    .filter(|part| !matches!(part, Js::Null))
                    .and_then(|part| js::property(&part, "text"));
                let nullish = |value: &Option<Js>| matches!(value, None | Some(Js::Null));
                if mcp_success && (truthy(error.as_ref()) || !truthy(result.as_ref())) {
                    let message = error
                        .as_ref()
                        .filter(|error| !matches!(error, Js::Null))
                        .and_then(|error| js::property(error, "message"));
                    let message = if nullish(&message) {
                        "no result".to_owned()
                    } else {
                        show(message.as_ref())
                    };
                    failures.push(format!("{label}: expected MCP success, got {message}"));
                }
                if mcp_success && is_error {
                    let actual = if nullish(&actual) {
                        "tool error".to_owned()
                    } else {
                        show(actual.as_ref())
                    };
                    failures.push(format!("{label}: expected MCP success, got {actual}"));
                }
                if let Some(expected) = mcp_error {
                    let expected = subst_str(&to_js_string(expected), captures);
                    if !is_error {
                        failures.push(format!(
                            "{label}: expected MCP error \"{expected}\", got success"
                        ));
                    } else if actual.as_ref().and_then(Js::as_str) != Some(expected.as_str()) {
                        failures.push(format!(
                            "{label}: MCP error expected \"{expected}\", got \"{}\"",
                            show(actual.as_ref())
                        ));
                    }
                }
            }
            Ok(_) => {}
        }
    }

    if let Some(header_spec) = expect
        .and_then(|expect| expect.get("headers"))
        .filter(|spec| spec.truthy())
    {
        let flat = flat_headers(&response.headers);
        for forbidden in items(header_spec.get("forbid")) {
            let forbidden = to_js_string(forbidden);
            for (name, value) in &flat {
                if value.contains(&forbidden) {
                    failures.push(format!(
                        "{label}: forbidden header substring \"{forbidden}\" present in {name}: {value}"
                    ));
                }
            }
        }
        if let Some(Js::Obj(required)) = header_spec.get("require") {
            for (name, expected) in required.iter() {
                let wanted = name.to_lowercase();
                let actual = flat
                    .iter()
                    .find(|(known, _)| *known == wanted)
                    .map(|(_, value)| value);
                if !matches!((actual, expected), (Some(actual), Js::Str(expected)) if actual == expected)
                {
                    failures.push(format!(
                        "{label}: header {name} expected \"{}\", got \"{}\"",
                        to_js_string(expected),
                        actual.map_or("(absent)", String::as_str)
                    ));
                }
            }
        }
    }

    for spec in items(step.get("assertSymbols")) {
        let symbol = show(spec.get("symbol"));
        let Some(value) = captures.get(&symbol) else {
            failures.push(format!("{label}: symbol ${{{symbol}}} was never captured"));
            continue;
        };
        let text = to_js_string(value);
        let length = js::utf16_len(&text);
        if let Some(expected) = spec.get("length")
            && !strictly_equal(Some(expected), Some(&Js::Num(length as f64)))
        {
            failures.push(format!(
                "{label}: symbol ${{{symbol}}} length {length} != {} (value={text})",
                to_js_string(expected)
            ));
        }
        if let Some(alphabet) = spec.get("alphabet").filter(|alphabet| alphabet.truthy()) {
            let allowed: Vec<char> = to_js_string(alphabet).chars().collect();
            if let Some(bad) = text.chars().find(|character| !allowed.contains(character)) {
                failures.push(format!(
                    "{label}: symbol ${{{symbol}}} char \"{bad}\" not in declared alphabet (value={text})"
                ));
            }
        }
    }

    failures
}

// --- Normalization (recorded / compared) -------------------------------------------------------

fn normalize_step(step: &Js, response: &http::Response, captures: &Obj) -> Result<Js, String> {
    let mut out = Obj::new().with("status", Js::Num(f64::from(response.status)));
    let expect = step.get("expect").filter(|expect| expect.truthy());
    if let Some(headers) = expect
        .and_then(|expect| expect.get("headers"))
        .filter(|headers| headers.truthy())
        && headers.get("mode").and_then(Js::as_str) == Some("exact-header")
    {
        out.insert("headers", normalize_headers(&response.headers, captures));
    }
    if let Some(body_spec) = expect
        .and_then(|expect| expect.get("body"))
        .filter(|body| body.truthy())
        && let Some(mode) = body_spec.get("mode").filter(|mode| mode.truthy())
    {
        let volatile: Vec<String> = items(body_spec.get("volatileFields"))
            .iter()
            .map(to_js_string)
            .collect();
        out.insert(
            "body",
            normalize_body(&to_js_string(mode), &response.body, captures, &volatile)?,
        );
    }
    Ok(Js::Obj(out))
}

/// Open the SQLite database a case left behind, configured as `node:sqlite` opens it (foreign keys
/// enforced, double-quoted string literals rejected) with a 5 s busy timeout. The file must
/// already exist: the server under test creates it, never this harness.
fn open_database(path: &Path, read_only: bool) -> Result<Connection, String> {
    if !path.exists() {
        return Err(format!("SQLite database not found: {}", path.display()));
    }
    let flags = if read_only {
        OpenFlags::SQLITE_OPEN_READ_ONLY
    } else {
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE
    };
    let connection = Connection::open_with_flags(path, flags | OpenFlags::SQLITE_OPEN_NO_MUTEX)
        .map_err(|error| format!("open {}: {error}", path.display()))?;
    connection
        .busy_timeout(Duration::from_millis(5000))
        .and_then(|()| connection.set_db_config(DbConfig::SQLITE_DBCONFIG_ENABLE_FKEY, true))
        .and_then(|_| connection.set_db_config(DbConfig::SQLITE_DBCONFIG_DQS_DML, false))
        .and_then(|_| connection.set_db_config(DbConfig::SQLITE_DBCONFIG_DQS_DDL, false))
        .map_err(|error| format!("configure {}: {error}", path.display()))?;
    Ok(connection)
}

/// Rows as `node:sqlite` returns them from `statement.all()`.
fn query_rows(connection: &Connection, query: &str) -> Result<Vec<Obj>, String> {
    let fail = |error: rusqlite::Error| format!("post-state SQL {query:?}: {error}");
    let mut statement = connection.prepare(query).map_err(fail)?;
    let names: Vec<String> = statement
        .column_names()
        .into_iter()
        .map(str::to_owned)
        .collect();
    let mut rows = statement.query([]).map_err(fail)?;
    let mut out = Vec::new();
    while let Some(row) = rows.next().map_err(fail)? {
        let mut object = Obj::new();
        for (index, name) in names.iter().enumerate() {
            let value = match row.get_ref(index).map_err(fail)? {
                ValueRef::Null => Js::Null,
                ValueRef::Integer(value) => {
                    if value.unsigned_abs() > MAX_SAFE_INTEGER {
                        return Err(format!(
                            "post-state SQL {query:?}: integer {value} in column {name} is outside the JavaScript safe range"
                        ));
                    }
                    Js::Num(value as f64)
                }
                ValueRef::Real(value) => Js::Num(value),
                ValueRef::Text(text) => Js::Str(String::from_utf8_lossy(text).into_owned()),
                // A Uint8Array serializes as an object keyed by byte index.
                ValueRef::Blob(bytes) => {
                    let mut blob = Obj::new();
                    for (position, byte) in bytes.iter().enumerate() {
                        blob.insert(position.to_string(), Js::Num(f64::from(*byte)));
                    }
                    Js::Obj(blob)
                }
            };
            object.insert(name.clone(), value);
        }
        out.push(object);
    }
    Ok(out)
}

fn normalize_post_state(
    post_state: Option<&Js>,
    data_dir: &Path,
    symbols: &Obj,
    captures: &Obj,
) -> Result<Js, String> {
    let mut out = Obj::new();
    let Some(post_state) = post_state.filter(|state| state.truthy()) else {
        return Ok(Js::Obj(out));
    };

    // Files and directory entries FIRST, before any DB open creates -wal/-shm side effects.
    let files = items(post_state.get("files"));
    if !files.is_empty() {
        let mut observed = Obj::new();
        for spec in files {
            let relative = subst_str(&show(spec.get("path")), symbols);
            let full = join_normalized(data_dir, &relative);
            let entry = match fs::metadata(&full) {
                Err(_) => Obj::new().with("present", Js::Bool(false)),
                Ok(metadata) if metadata.is_dir() => Obj::new()
                    .with("present", Js::Bool(true))
                    .with("type", Js::str("dir")),
                Ok(metadata) => {
                    let bytes = fs::read(&full)
                        .map_err(|error| format!("read {}: {error}", full.display()))?;
                    Obj::new()
                        .with("present", Js::Bool(true))
                        .with("type", Js::str("file"))
                        .with("size", Js::Num(metadata.len() as f64))
                        .with("sha256", Js::Str(sha256_hex(&bytes)))
                }
            };
            observed.insert(relative, Js::Obj(entry));
        }
        out.insert("files", Js::Obj(observed));
    }

    let directories = items(post_state.get("dirEntries"));
    if !directories.is_empty() {
        let mut observed = Obj::new();
        for spec in directories {
            let relative = subst_str(&show(spec.get("path")), symbols);
            let full = join_normalized(data_dir, &relative);
            let entries = if full.exists() {
                let mut names: Vec<String> = fs::read_dir(&full)
                    .map_err(|error| format!("read {}: {error}", full.display()))?
                    .map(|entry| {
                        entry
                            .map(|entry| entry.file_name().to_string_lossy().into_owned())
                            .map_err(|error| format!("read {}: {error}", full.display()))
                    })
                    .collect::<Result<_, _>>()?;
                names.sort_by(|a, b| js::cmp_utf16(a, b));
                Js::Arr(names.into_iter().map(Js::Str).collect())
            } else {
                Js::Null
            };
            observed.insert(relative, entries);
        }
        out.insert("dirEntries", Js::Obj(observed));
    }

    let queries = items(post_state.get("sql"));
    if !queries.is_empty() {
        let mut observed = Obj::new();
        let connection = open_database(&data_dir.join("artifacts.db"), true)?;
        for spec in queries {
            let query = subst_str(&show(spec.get("query")), symbols);
            let volatile: Vec<String> = items(spec.get("volatileFields"))
                .iter()
                .map(to_js_string)
                .collect();
            let rows = query_rows(&connection, &query)?
                .into_iter()
                .map(|row| {
                    // Blank declared-volatile columns, then serialize deterministically.
                    let mut keys: Vec<&String> = row.keys().collect();
                    keys.sort_by(|a, b| js::cmp_utf16(a, b));
                    let mut sorted = Obj::new();
                    for key in keys {
                        let value = if volatile.contains(key) {
                            Js::str(VOLATILE)
                        } else {
                            row.get(key).cloned().unwrap_or(Js::Null)
                        };
                        sorted.insert(key.clone(), value);
                    }
                    Js::Obj(sorted)
                })
                .collect();
            observed.insert(show(spec.get("name")), Js::Arr(rows));
        }
        out.insert("sql", Js::Obj(observed));
    }

    // Back-substitute captured ids/tokens across the WHOLE post-state tree (keys such as file
    // paths and directory entries as well as SQL values) so goldens are run-independent.
    js::parse(&backsub_string(&js::stringify(&Js::Obj(out)), captures))
        .map_err(|error| format!("back-substituted post-state is not JSON: {error}"))
}

fn apply_after_sql(statements: Option<&Js>, data_dir: &Path, symbols: &Obj) -> Result<(), String> {
    let statements = items(statements);
    if statements.is_empty() {
        return Ok(());
    }
    let connection = open_database(&data_dir.join("artifacts.db"), false)?;
    for statement in statements {
        let sql = subst_str(&to_js_string(statement), symbols);
        connection
            .execute_batch(&sql)
            .map_err(|error| format!("afterSql {sql:?}: {error}"))?;
    }
    Ok(())
}

// --- Fixtures ----------------------------------------------------------------------------------

fn copy_fixture_tree(source: &Path, destination: &Path) -> Result<(), String> {
    let read = |error: std::io::Error| format!("copy fixture {}: {error}", source.display());
    for entry in fs::read_dir(source).map_err(read)? {
        let entry = entry.map_err(read)?;
        // The .gitkeep placeholder is not part of the fixture.
        if entry.file_name() == ".gitkeep" {
            continue;
        }
        let from = entry.path();
        let to = destination.join(entry.file_name());
        let kind = entry.file_type().map_err(read)?;
        if kind.is_dir() {
            fs::create_dir_all(&to).map_err(read)?;
            copy_fixture_tree(&from, &to)?;
        } else if kind.is_symlink() {
            std::os::unix::fs::symlink(fs::read_link(&from).map_err(read)?, &to).map_err(read)?;
        } else {
            fs::copy(&from, &to).map_err(read)?;
        }
    }
    Ok(())
}

/// Copy fixture contents into the fresh data dir. For the empty-v21 fixture this copies nothing;
/// the server migrates the empty dir on boot, which is exactly the "freshly-migrated empty DB"
/// starting state.
fn materialize_fixture(fixture: Option<&Js>, data_dir: &Path) -> Result<(), String> {
    let name = fixture
        .filter(|name| !matches!(name, Js::Null))
        .map(to_js_string)
        .ok_or("case names no fixture")?;
    let source = join_normalized(&conformance_dir().join("fixtures"), &name);
    if !source.exists() {
        return Err(format!(
            "Unknown fixture \"{name}\" (expected directory {})",
            source.display()
        ));
    }
    copy_fixture_tree(&source, data_dir)
}

// --- Per-case execution ------------------------------------------------------------------------

struct CaseRun {
    normalized: Js,
    assertion_failures: Vec<String>,
}

/// Attempts to start a case's server when its free port was taken before the server bound it.
const START_ATTEMPTS: usize = 3;

/// Copy the fixture into a fresh data dir and start the server on a free port.
fn start_case_server(binary: &Path, case: &Case) -> Result<(TempDir, Server), String> {
    let mut env = base_env();
    if let Some(Js::Obj(case_env)) = case.definition.get("env") {
        for (name, value) in case_env.iter() {
            env.insert(name.clone(), Js::Str(to_js_string(value)));
        }
    }
    let data_dir = TempDir::new("conf-data-rust-")?;
    materialize_fixture(case.definition.get("fixture"), data_dir.path())?;
    if let Some(sources) = case
        .definition
        .get("dataSources")
        .filter(|sources| sources.truthy())
    {
        // The server reads the operator file regardless of its launch directory.
        let path = data_dir.path().join("data-sources.json");
        fs::write(&path, js::stringify(sources))
            .map_err(|error| format!("write {}: {error}", path.display()))?;
        env.insert(
            "ARTIFACT_DATA_SOURCES_FILE",
            Js::Str(path.display().to_string()),
        );
    }
    let env: Vec<(String, String)> = env
        .iter()
        .map(|(name, value)| (name.clone(), to_js_string(value)))
        .collect();
    let port = free_port()?;
    let server = Server::start(binary, &repo_root(), data_dir.path(), port, &env)?;
    Ok((data_dir, server))
}

fn run_case(binary: &Path, case: &Case) -> Result<CaseRun, String> {
    // The server binds only after it has migrated the data dir, so another process can take the
    // released port first. Only that failure is retried, each time from a fresh data dir.
    let mut attempt = 1;
    let (data_dir, mut server) = loop {
        match start_case_server(binary, case) {
            Err(error) if attempt < START_ATTEMPTS && error.contains("Address already in use") => {
                attempt += 1;
            }
            started => break started?,
        }
    };
    server::wait_for_health(server.port(), || None)?;
    let mut symbols = constant_symbols(); // forward-substitution table (constants + captures)
    let mut captures = Obj::new(); // high-entropy runtime symbols (back-substituted)
    let mut assertion_failures = Vec::new();

    let mut steps: Vec<Js> = Vec::new();
    for (index, step) in case.steps.iter().enumerate() {
        let label = match step.get("name").filter(|name| name.truthy()) {
            Some(name) => format!("step[{index}] {}", to_js_string(name)),
            None => format!("step[{index}]"),
        };
        let request = build_request(step.get("request"), &symbols)
            .map_err(|error| format!("{label}: {error}"))?;
        let response = http::send(
            server.port(),
            &http::Request {
                method: &request.method,
                path: &request.path,
                headers: &request.headers,
                body: request.body.as_deref(),
            },
            REQUEST_TIMEOUT,
        )
        .map_err(|error| format!("{label}: {error}\nRust server output:\n{}", server.logs()))?;

        // Captures feed both later substitutions and back-substitution in goldens.
        if let Some(Js::Obj(capture)) = step.get("capture") {
            for (name, path) in capture.iter() {
                let Ok(parsed) = js::parse(&String::from_utf8_lossy(&response.body)) else {
                    assertion_failures.push(format!(
                        "{label}: cannot capture ${{{name}}} — response body is not JSON"
                    ));
                    continue;
                };
                let path = to_js_string(path);
                match get_by_path(&parsed, &path) {
                    None => assertion_failures.push(format!(
                        "{label}: capture path \"{path}\" for ${{{name}}} resolved to undefined"
                    )),
                    Some(value) => {
                        captures.insert(name.clone(), value.clone());
                        symbols.insert(name.clone(), value);
                    }
                }
            }
        }

        apply_after_sql(step.get("afterSql"), data_dir.path(), &symbols)
            .map_err(|error| format!("{label}: {error}"))?;
        assertion_failures.extend(eval_step_assertions(step, &response, &captures, &label));
        let normalized = normalize_step(step, &response, &captures)
            .map_err(|error| format!("{label}: {error}"))?;

        // Uniformity contract: this response must be indistinguishable from an earlier step's
        // (status + headers + body). This is how "invalid, revoked and expired are the SAME 404"
        // and the invariant-3 concealment matrix are proven inside one run, independently of
        // what any golden happens to hold.
        if let Some(same_as) = step
            .get("expect")
            .and_then(|expect| expect.get("sameAsStep"))
        {
            match step_index(same_as).and_then(|index| steps.get(index)) {
                None => assertion_failures.push(format!(
                    "{label}: sameAsStep {} does not refer to an earlier step",
                    to_js_string(same_as)
                )),
                Some(other) if !deep_equal(other, &normalized) => {
                    assertion_failures.push(format!(
                        "{label}: response is distinguishable from step[{}] (concealment/uniformity broken):\n{}",
                        to_js_string(same_as),
                        describe_diff(other, &normalized)
                    ));
                }
                Some(_) => {}
            }
        }
        steps.push(normalized);
    }

    // Stop cleanly BEFORE inspecting persistent state.
    server.stop();

    let post_state = normalize_post_state(
        case.definition.get("postState"),
        data_dir.path(),
        &symbols,
        &captures,
    )?;
    Ok(CaseRun {
        normalized: Js::Obj(
            Obj::new()
                .with("steps", Js::Arr(steps))
                .with("postState", post_state),
        ),
        assertion_failures,
    })
}

// --- Golden comparison -------------------------------------------------------------------------

fn diff_normalized(golden: &Js, actual: &Js) -> Vec<String> {
    let mut diffs = Vec::new();
    let golden_steps = items(golden.get("steps"));
    let actual_steps = items(actual.get("steps"));
    if golden_steps.len() != actual_steps.len() {
        diffs.push(format!(
            "step count: golden {} vs actual {}",
            golden_steps.len(),
            actual_steps.len()
        ));
    }
    let member = |steps: &[Js], index: usize, key: &str| -> Option<Js> {
        steps
            .get(index)
            .filter(|step| step.truthy())
            .and_then(|step| step.get(key))
            .cloned()
    };
    for index in 0..golden_steps.len().max(actual_steps.len()) {
        let golden_status = member(golden_steps, index, "status");
        let actual_status = member(actual_steps, index, "status");
        if !strictly_equal(golden_status.as_ref(), actual_status.as_ref()) {
            diffs.push(format!(
                "step[{index}].status: golden {} vs actual {}",
                show(golden_status.as_ref()),
                show(actual_status.as_ref())
            ));
        }
        for aspect in ["headers", "body"] {
            let expected = member(golden_steps, index, aspect).unwrap_or(Js::Null);
            let observed = member(actual_steps, index, aspect).unwrap_or(Js::Null);
            if !deep_equal(&expected, &observed) {
                diffs.push(format!(
                    "step[{index}].{aspect} differ:\n{}",
                    describe_diff(&expected, &observed)
                ));
            }
        }
    }
    let post_state = |normalized: &Js| match normalized.get("postState") {
        None | Some(Js::Null) => Js::Obj(Obj::new()),
        Some(state) => state.clone(),
    };
    let (expected, observed) = (post_state(golden), post_state(actual));
    if !deep_equal(&expected, &observed) {
        diffs.push(format!(
            "postState differ:\n{}",
            describe_diff(&expected, &observed)
        ));
    }
    diffs
}

// --- Orchestration -----------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum Status {
    Pass,
    Recorded,
    Pending,
    Fail,
    Error,
}

struct CaseResult {
    id: String,
    status: Status,
    detail: Option<String>,
    elapsed: Duration,
}

fn run_one(binary: &Path, case: &Case, record: bool) -> CaseResult {
    let started = Instant::now();
    let result = |status, detail: Option<String>| CaseResult {
        id: case.id.clone(),
        status,
        detail,
        elapsed: started.elapsed(),
    };
    if let Some(reason) = case
        .definition
        .get("pending")
        .filter(|pending| pending.truthy())
    {
        return result(Status::Pending, Some(to_js_string(reason)));
    }
    let run = match run_case(binary, case) {
        Ok(run) => run,
        Err(error) => return result(Status::Error, Some(error)),
    };
    if record {
        // A broken state can never be frozen as "expected".
        if !run.assertion_failures.is_empty() {
            return result(
                Status::Fail,
                Some(format!(
                    "refusing to record (assertions failed):\n  {}",
                    run.assertion_failures.join("\n  ")
                )),
            );
        }
        return match write_golden(&case.id, &run.normalized) {
            Ok(()) => result(Status::Recorded, None),
            Err(error) => result(Status::Error, Some(error)),
        };
    }
    let golden = match load_golden(&case.id) {
        Ok(Some(golden)) => golden,
        Ok(None) => {
            return result(
                Status::Fail,
                Some("no golden recorded (run with CONFORMANCE_RECORD=1)".to_owned()),
            );
        }
        Err(error) => return result(Status::Error, Some(error)),
    };
    let mut problems = run.assertion_failures;
    problems.extend(diff_normalized(&golden, &run.normalized));
    if problems.is_empty() {
        result(Status::Pass, None)
    } else {
        result(Status::Fail, Some(problems.join("\n  ")))
    }
}

/// Worker threads: `CONFORMANCE_JOBS`, or up to four. Every case has its own server and data
/// dir, so cases are independent; results are reported in case order.
fn worker_count() -> usize {
    match std::env::var("CONFORMANCE_JOBS") {
        Ok(jobs) => jobs
            .parse::<usize>()
            .ok()
            .filter(|jobs| *jobs > 0)
            .unwrap_or_else(|| {
                panic!("CONFORMANCE_JOBS must be a positive integer (got {jobs:?})")
            }),
        Err(_) => std::thread::available_parallelism().map_or(1, |cores| cores.get().min(4)),
    }
}

fn run_cases(binary: &Path, cases: &[Case], record: bool) -> Vec<CaseResult> {
    let next = AtomicUsize::new(0);
    let slots: Mutex<Vec<Option<CaseResult>>> = Mutex::new(cases.iter().map(|_| None).collect());
    std::thread::scope(|scope| {
        for _ in 0..worker_count().min(cases.len()) {
            scope.spawn(|| {
                loop {
                    let index = next.fetch_add(1, Ordering::Relaxed);
                    let Some(case) = cases.get(index) else {
                        break;
                    };
                    let result = run_one(binary, case, record);
                    slots.lock().expect("result slots")[index] = Some(result);
                }
            });
        }
    });
    slots
        .into_inner()
        .expect("result slots")
        .into_iter()
        .map(|result| result.expect("every case ran"))
        .collect()
}

fn format_results(results: &[CaseResult]) -> String {
    let mut out = String::from("=== impl: rust ===\n");
    for result in results {
        let tag = match result.status {
            Status::Pass => "PASS ",
            Status::Recorded => "REC  ",
            Status::Pending => "PEND ",
            Status::Fail => "FAIL ",
            Status::Error => "ERROR",
        };
        out.push_str(&format!(
            "  [{tag}] {} ({:.2}s)\n",
            result.id,
            result.elapsed.as_secs_f64()
        ));
        if let Some(detail) = &result.detail {
            out.push_str(&format!("    {}\n", detail.replace('\n', "\n    ")));
        }
    }
    let count = |status| results.iter().filter(|r| r.status == status).count();
    out.push_str(&format!(
        "  -- rust: {} pass, {} recorded, {} fail, {} error, {} pending",
        count(Status::Pass),
        count(Status::Recorded),
        count(Status::Fail),
        count(Status::Error),
        count(Status::Pending)
    ));
    out
}

fn record_mode() -> bool {
    match std::env::var("CONFORMANCE_RECORD") {
        Ok(value) if value == "1" => true,
        Ok(value) if value.is_empty() || value == "0" => false,
        Ok(value) => panic!("CONFORMANCE_RECORD must be 1 to record goldens (got {value:?})"),
        Err(_) => false,
    }
}

/// The gate: every case replays against its recorded golden. Set `CONFORMANCE_RECORD=1` to
/// regenerate goldens instead (refused for any case whose assertions fail; never in CI) and
/// `CONFORMANCE_FILTER=<text>` to run only cases whose id or a tag contains the text.
#[test]
fn conformance_cases_match_the_recorded_goldens() {
    let record = record_mode();
    let filter = std::env::var("CONFORMANCE_FILTER")
        .ok()
        .filter(|filter| !filter.is_empty());
    let mut cases = load_cases().unwrap_or_else(|error| panic!("could not load cases: {error}"));
    if let Some(filter) = &filter {
        cases.retain(|case| {
            case.id.contains(filter.as_str())
                || case.tags().any(|tag| tag.contains(filter.as_str()))
        });
    }
    assert!(!cases.is_empty(), "No cases matched.");

    let started = Instant::now();
    let results = run_cases(
        Path::new(env!("CARGO_BIN_EXE_artifact-mcp")),
        &cases,
        record,
    );
    println!("{}", format_results(&results));
    println!(
        "conformance: {} cases in {:.1?}",
        cases.len(),
        started.elapsed()
    );

    let failed: Vec<&CaseResult> = results
        .iter()
        .filter(|result| matches!(result.status, Status::Fail | Status::Error))
        .collect();
    assert!(
        failed.is_empty(),
        "{} of {} conformance cases failed:\n{}",
        failed.len(),
        results.len(),
        failed
            .iter()
            .map(|result| format!(
                "[{}] {}\n    {}",
                if result.status == Status::Error {
                    "ERROR"
                } else {
                    "FAIL"
                },
                result.id,
                result
                    .detail
                    .as_deref()
                    .unwrap_or_default()
                    .replace('\n', "\n    ")
            ))
            .collect::<Vec<_>>()
            .join("\n")
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn substitution_replaces_known_symbols_and_keeps_unknown_ones() {
        let symbols = Obj::new().with("a", Js::str("A")).with("n", Js::Num(12.0));
        assert_eq!(
            subst_str("${a}/${n}/${missing}/$${a}}/${ a}/${", &symbols),
            "A/12/${missing}/$A}/${ a}/${"
        );
    }

    #[test]
    fn capture_paths_follow_javascript_property_access() {
        let body = js::parse(r#"{"result":{"content":[{"text":"t"}],"none":null}}"#).unwrap();
        assert_eq!(
            show(get_by_path(&body, "result.content.0.text").as_ref()),
            "t"
        );
        assert!(matches!(get_by_path(&body, "result.none"), Some(Js::Null)));
        assert!(get_by_path(&body, "result.none.deeper").is_none());
        assert!(get_by_path(&body, "result.missing.deeper").is_none());
    }

    #[test]
    fn role_matrix_expands_to_read_write_and_delete_steps() {
        let definition = js::parse(
            r#"{"roleMatrix":[{"role":"reader","key":"${key_acme}","targets":[{"scope":"own","id":"${x}","outcomes":{"read":"ok","write":"denied"}}]}]}"#,
        )
        .unwrap();
        let steps = expand_role_matrix(&definition).unwrap();
        assert_eq!(steps.len(), 3);
        assert_eq!(
            js::stringify(&steps[1]),
            r#"{"name":"reader own write","request":{"method":"POST","path":"/mcp","headers":{"authorization":"Bearer ${key_acme}","content-type":"application/json"},"json":{"jsonrpc":"2.0","id":101,"method":"tools/call","params":{"name":"patch_artifact","arguments":{"id":"${x}","expected_revision":1,"edits":[{"find":"seed","replace":"reader-own"}]}}}},"expect":{"status":200,"mcpError":"denied"}}"#
        );
        assert_eq!(
            js::stringify(steps[0].get("expect").unwrap()),
            r#"{"status":200,"mcpSuccess":true}"#
        );
        assert_eq!(
            js::stringify(steps[2].get("expect").unwrap()),
            r#"{"status":200}"#
        );
    }

    #[test]
    fn golden_diffs_cover_status_body_and_post_state() {
        let golden = js::parse(
            r#"{"steps":[{"status":200,"body":{"mode":"canonical-json","json":{"a":1}}}],"postState":{}}"#,
        )
        .unwrap();
        assert!(diff_normalized(&golden, &golden).is_empty());
        let changed = js::parse(
            r#"{"steps":[{"status":201,"body":{"mode":"canonical-json","json":{"a":2}}},{"status":200}],"postState":{"sql":{}}}"#,
        )
        .unwrap();
        let diffs = diff_normalized(&golden, &changed);
        assert_eq!(diffs.len(), 5, "{diffs:#?}");
        assert!(diffs[0].starts_with("step count: golden 1 vs actual 2"));
        assert!(diffs[1].starts_with("step[0].status: golden 200 vs actual 201"));
        assert!(diffs[2].starts_with("step[0].body differ"));
        assert!(diffs[3].starts_with("step[1].status: golden undefined vs actual 200"));
        assert!(diffs[4].starts_with("postState differ"));
    }
}
