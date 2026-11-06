use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

// Exceptions must name the file and dependency, and explain why the edge remains.
const ALLOWLIST: &[(&str, &str, &str)] = &[
    (
        "src/mcp/tools/data.rs",
        "persistence",
        "Data tools still use push persistence types.",
    ),
    (
        "src/http/routes/state.rs",
        "persistence",
        "State routes still use persistence types.",
    ),
    (
        "src/http/routes/push.rs",
        "persistence",
        "Push routes still use persistence types.",
    ),
    (
        "src/http/routes/feedback.rs",
        "persistence",
        "Feedback routes still use persistence types.",
    ),
    (
        "src/http/routes/admin/organizations.rs",
        "persistence",
        "Organization name normalization has not yet moved behind a port.",
    ),
];

// Replace comments and literals with spaces so tokens on either side stay separate.
fn code_only(source: &str) -> String {
    let bytes = source.as_bytes();
    let mut out = vec![b' '; bytes.len()];
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i..].starts_with(b"//") {
            i += 2;
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
        } else if bytes[i..].starts_with(b"/*") {
            i += 2;
            let mut depth = 1;
            while i < bytes.len() && depth > 0 {
                if bytes[i..].starts_with(b"/*") {
                    depth += 1;
                    i += 2;
                } else if bytes[i..].starts_with(b"*/") {
                    depth -= 1;
                    i += 2;
                } else {
                    i += 1;
                }
            }
        } else {
            let raw_start = if bytes[i] == b'r' {
                Some(i + 1)
            } else if bytes[i..].starts_with(b"br") {
                Some(i + 2)
            } else {
                None
            };
            let raw = raw_start.and_then(|start| {
                let mut end = start;
                while end < bytes.len() && bytes[end] == b'#' {
                    end += 1;
                }
                (bytes.get(end) == Some(&b'"')).then_some((end, end - start))
            });
            if let Some((quote, hashes)) = raw {
                i = quote + 1;
                while i < bytes.len() {
                    if bytes[i] == b'"'
                        && i + 1 + hashes <= bytes.len()
                        && bytes[i + 1..i + 1 + hashes].iter().all(|b| *b == b'#')
                    {
                        i += 1 + hashes;
                        break;
                    }
                    i += 1;
                }
            } else {
                let quote = if bytes[i] == b'"' {
                    Some((i, b'"'))
                } else if bytes[i..].starts_with(b"b\"") {
                    Some((i + 1, b'"'))
                } else if bytes[i..].starts_with(b"b'") {
                    Some((i + 1, b'\''))
                } else if bytes[i] == b'\''
                    && (bytes.get(i + 2) == Some(&b'\'') || bytes.get(i + 1) == Some(&b'\\'))
                {
                    Some((i, b'\''))
                } else {
                    None
                };
                if let Some((start, delimiter)) = quote {
                    i = start + 1;
                    while i < bytes.len() {
                        if bytes[i] == b'\\' {
                            i = (i + 2).min(bytes.len());
                        } else if bytes[i] == delimiter {
                            i += 1;
                            break;
                        } else {
                            i += 1;
                        }
                    }
                } else {
                    out[i] = bytes[i];
                    i += 1;
                }
            }
        }
    }
    String::from_utf8(out).expect("source remains UTF-8")
}

fn tokens(source: &str) -> Vec<String> {
    let mut result = Vec::new();
    let mut word = String::new();
    let code = code_only(source);
    let mut chars = code.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '#'
            && word == "r"
            && chars
                .peek()
                .is_some_and(|c| c.is_ascii_alphabetic() || *c == '_')
        {
            word.clear();
            continue;
        }
        if c.is_ascii_alphanumeric() || c == '_' {
            word.push(c);
        } else {
            if !word.is_empty() {
                result.push(std::mem::take(&mut word));
            }
            if matches!(c, ':' | '{' | '}' | ',' | '*') {
                result.push(c.to_string());
            }
        }
    }
    if !word.is_empty() {
        result.push(word);
    }
    result
}

fn collect(source: &str) -> BTreeSet<Vec<String>> {
    let t = tokens(source);
    let mut found = BTreeSet::new();
    for i in 0..t.len().saturating_sub(2) {
        if t[i] == "crate" && t[i + 1] == ":" && t[i + 2] == ":" {
            paths(&t, &mut (i + 3), &[], &mut found);
        }
    }
    found
}

#[test]
fn collector_ignores_comments_and_literals() {
    let has_http = |s| {
        collect(s)
            .iter()
            .any(|p| p.first().is_some_and(|v| v == "http"))
    };
    assert!(has_http("crate::http::router"));
    assert!(has_http(
        r#"fn f() { let _ = ("//", crate::http::router); }"#
    ));
    assert!(!has_http(r#"let s = "crate::http::router";"#));
    assert!(!has_http("/** crate::http::router */"));
    assert!(!has_http("/* outer /* crate::http::router */ end */"));
    assert!(!has_http("let s = r###\"crate::http::router\"###;"));
    assert!(has_http("fn f<'a>(x: &'a str) { crate::http::router(); }"));
}

#[test]
fn collector_handles_raw_identifiers_and_aliases() {
    assert!(
        collect("crate::r#http::router")
            .iter()
            .any(|p| p.first().is_some_and(|v| v == "http"))
    );
    assert_eq!(
        collect("use crate::{model as http};"),
        BTreeSet::from([vec!["model".to_string()]])
    );
    assert_eq!(
        collect("use crate::{model::{self as m, json}};")
            .iter()
            .filter_map(|p| p.first())
            .collect::<BTreeSet<_>>(),
        BTreeSet::from([&"model".to_string()])
    );
}

// Parse a use tree or path after `crate::`, retaining its first two segments.
fn paths(t: &[String], i: &mut usize, prefix: &[String], found: &mut BTreeSet<Vec<String>>) {
    if t.get(*i).is_some_and(|s| s == "{") {
        *i += 1;
        while *i < t.len() && t[*i] != "}" {
            let before = *i;
            paths(t, i, prefix, found);
            if *i == before {
                *i += 1;
            }
            if t.get(*i).is_some_and(|s| s == ",") {
                *i += 1;
            }
        }
        *i += usize::from(*i < t.len());
    } else if let Some(name) = t.get(*i).filter(|s| {
        s.chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_')
    }) {
        let mut path = prefix.to_vec();
        if name != "self" || prefix.is_empty() {
            path.push(name.clone());
        }
        *i += 1;
        if t.get(*i).is_some_and(|s| s == ":") && t.get(*i + 1).is_some_and(|s| s == ":") {
            *i += 2;
            paths(t, i, &path, found);
        } else {
            found.insert(path);
        }
        if t.get(*i).is_some_and(|s| s == "as") {
            *i += 1;
            *i += usize::from(*i < t.len());
        }
    } else if !prefix.is_empty() {
        found.insert(prefix.to_vec());
        if t.get(*i).is_some_and(|s| s == "*") {
            *i += 1;
        }
    }
}

fn walk(dir: &Path, files: &mut Vec<std::path::PathBuf>) {
    for entry in fs::read_dir(dir).expect("read source directory") {
        let path = entry.expect("read source entry").path();
        if path.is_dir() {
            walk(&path, files);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            files.push(path);
        }
    }
}

fn forbidden(from: &str, path: &[String]) -> Option<&'static str> {
    let to = path.first()?.as_str();
    let listed = |names: &[&str]| names.contains(&to);
    match from {
        "model"
            if listed(&[
                "http",
                "mcp",
                "persistence",
                "integrations",
                "adapters",
                "startup",
                "data",
                "security",
                "artifacts",
                "ports",
                "render",
                "config",
            ]) =>
        {
            Some("model dependency direction")
        }
        "ports" if listed(&["http", "mcp", "persistence", "adapters", "startup"]) => {
            Some("ports dependency direction")
        }
        "persistence" if listed(&["http", "mcp", "adapters", "startup"]) => {
            Some("persistence dependency direction")
        }
        "persistence"
            if to == "integrations"
                && !path.get(1).is_some_and(|s| {
                    [
                        "delivery_envelope",
                        "discussion_envelope",
                        "discord_discussion",
                        "discord_inbound",
                    ]
                    .contains(&s.as_str())
                }) =>
        {
            Some("persistence integration contract")
        }
        "config" | "security" | "data" | "artifacts" if listed(&["http", "mcp"]) => {
            Some("core dependency direction")
        }
        "http" | "mcp" if to == "persistence" => Some("ingress dependency direction"),
        _ => None,
    }
}

#[test]
fn dependency_direction() {
    let mut files = Vec::new();
    walk(Path::new("src"), &mut files);
    files.sort();
    let mut used = BTreeSet::new();
    let mut violations = Vec::new();
    for file in files {
        let name = file.to_string_lossy().replace('\\', "/");
        if ["src/lib.rs", "src/main.rs", "src/app.rs"].contains(&name.as_str()) {
            continue;
        }
        let from = name
            .trim_start_matches("src/")
            .split('/')
            .next()
            .unwrap_or("")
            .trim_end_matches(".rs");
        let source = fs::read_to_string(&file).expect("read Rust source");
        let found = collect(&source);
        for path in found {
            if let Some(rule) = forbidden(from, &path) {
                let module = &path[0];
                if let Some(index) = ALLOWLIST
                    .iter()
                    .position(|(f, m, _)| *f == name && *m == module)
                {
                    used.insert(index);
                } else {
                    violations.push(format!("{name}: imports crate::{module} ({rule})"));
                }
            }
        }
    }
    for (i, (file, module, reason)) in ALLOWLIST.iter().enumerate() {
        if !used.contains(&i) {
            violations.push(format!(
                "unused allowlist: {file}: crate::{module} ({reason})"
            ));
        }
    }
    assert!(violations.is_empty(), "{}", violations.join("\n"));
}
