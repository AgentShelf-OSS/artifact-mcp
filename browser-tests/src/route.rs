// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Neil Blackman
//! Requests answered inside the browser ([`crate::Page::route`]) and a read-only file server for
//! them ([`StaticFiles`]).

use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

type Handler = Arc<dyn Fn(&Request) -> Response + Send + Sync>;

/// A paused request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    pub url: String,
    pub method: String,
    /// The percent-decoded path, starting with `/`. Not normalised: `..` stays as it is.
    pub path: String,
    /// The query string without `?`, if any.
    pub query: Option<String>,
}

impl Request {
    pub(crate) fn parse(url: &str, method: &str) -> Self {
        let without_fragment = url.split('#').next().unwrap_or("");
        let (before_query, query) = match without_fragment.split_once('?') {
            Some((before, query)) => (before, Some(query.to_owned())),
            None => (without_fragment, None),
        };
        // Skip "scheme://authority".
        let path = match before_query.find("://") {
            Some(scheme_end) => {
                let rest = &before_query[scheme_end + 3..];
                rest.find('/').map_or("/", |slash| &rest[slash..])
            }
            None => before_query,
        };
        Self {
            url: url.to_owned(),
            method: method.to_owned(),
            path: percent_decode(path),
            query,
        }
    }
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%'
            && index + 2 < bytes.len()
            && let (Some(high), Some(low)) = (hex(bytes[index + 1]), hex(bytes[index + 2]))
        {
            decoded.push(high << 4 | low);
            index += 3;
            continue;
        }
        decoded.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&decoded).into_owned()
}

fn hex(digit: u8) -> Option<u8> {
    (digit as char).to_digit(16).map(|value| value as u8)
}

/// The answer to a routed request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Response {
    /// A `status` response with `content_type`, never cached.
    pub fn new(status: u16, content_type: &str, body: impl Into<Vec<u8>>) -> Self {
        Self {
            status,
            headers: vec![
                ("Content-Type".to_owned(), content_type.to_owned()),
                ("Cache-Control".to_owned(), "no-store".to_owned()),
            ],
            body: body.into(),
        }
    }

    /// `200` with `content_type`.
    pub fn ok(content_type: &str, body: impl Into<Vec<u8>>) -> Self {
        Self::new(200, content_type, body)
    }

    /// A plain-text error.
    pub fn error(status: u16, message: &str) -> Self {
        Self::new(
            status,
            "text/plain; charset=utf-8",
            message.as_bytes().to_vec(),
        )
    }
}

pub(crate) struct Route {
    prefix: String,
    handler: Handler,
}

impl Route {
    pub(crate) fn new(
        prefix: &str,
        handler: impl Fn(&Request) -> Response + Send + Sync + 'static,
    ) -> Self {
        Self {
            prefix: prefix.to_owned(),
            handler: Arc::new(handler),
        }
    }

    /// The DevTools `urlPattern`: the prefix with its wildcard characters escaped, then `*`.
    pub(crate) fn pattern(&self) -> String {
        let mut pattern = String::new();
        for character in self.prefix.chars() {
            if matches!(character, '*' | '?' | '\\') {
                pattern.push('\\');
            }
            pattern.push(character);
        }
        pattern.push('*');
        pattern
    }

    pub(crate) fn matches(&self, url: &str) -> bool {
        url.starts_with(&self.prefix)
    }

    pub(crate) fn handler(&self) -> Handler {
        Arc::clone(&self.handler)
    }
}

/// Serves the files under one directory, read-only. Paths with `..`, `.` or empty segments, and
/// files that resolve (through symlinks) outside the directory, get `404`.
#[derive(Debug, Clone)]
pub struct StaticFiles {
    root: PathBuf,
}

impl StaticFiles {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// The file for a request path such as `/assets/shell.js`, if it is safe to serve.
    pub fn resolve(&self, path: &str) -> Option<PathBuf> {
        let relative = path.strip_prefix('/')?;
        if relative.is_empty()
            || relative.contains('\\')
            || relative.contains('\0')
            || relative
                .split('/')
                .any(|segment| segment.is_empty() || segment == "." || segment == "..")
        {
            return None;
        }
        let candidate = self.root.join(relative);
        if !Path::new(relative)
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
        {
            return None;
        }
        let root = self.root.canonicalize().ok()?;
        let resolved = candidate.canonicalize().ok()?;
        (resolved.starts_with(&root) && resolved.is_file()).then_some(resolved)
    }

    /// Answer `request` from the directory.
    pub fn respond(&self, request: &Request) -> Response {
        if request.method != "GET" && request.method != "HEAD" {
            return Response::error(405, "read-only");
        }
        let Some(file) = self.resolve(&request.path) else {
            return Response::error(404, &format!("not found: {}", request.path));
        };
        match std::fs::read(&file) {
            Ok(bytes) => Response::ok(content_type(&file), bytes),
            Err(error) => Response::error(500, &format!("{}: {error}", request.path)),
        }
    }
}

/// The `Content-Type` for a file name. Module scripts need a JavaScript type.
pub fn content_type(path: &Path) -> &'static str {
    match path.extension().and_then(|extension| extension.to_str()) {
        Some("js" | "mjs") => "text/javascript; charset=utf-8",
        Some("html") => "text/html; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("json") => "application/json",
        Some("webmanifest") => "application/manifest+json",
        Some("png") => "image/png",
        Some("svg") => "image/svg+xml",
        Some("wasm") => "application/wasm",
        _ => "text/plain; charset=utf-8",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_split_path_and_query() {
        let request = Request::parse("https://harness.test/a%20b/c.js?x=1#frag", "GET");
        assert_eq!(request.path, "/a b/c.js");
        assert_eq!(request.query.as_deref(), Some("x=1"));
        assert_eq!(Request::parse("https://harness.test", "GET").path, "/");
        assert_eq!(
            Request::parse("https://harness.test/%zz", "GET").path,
            "/%zz"
        );
    }

    #[test]
    fn patterns_escape_wildcards() {
        let route = Route::new("https://h.test/a*b?", |_| Response::error(404, ""));
        assert_eq!(route.pattern(), "https://h.test/a\\*b\\?*");
        assert!(route.matches("https://h.test/a*b?/x"));
        assert!(!route.matches("https://other.test/"));
    }

    #[test]
    fn static_files_stay_inside_the_root() {
        let files = StaticFiles::new(env!("CARGO_MANIFEST_DIR"));
        assert!(files.resolve("/Cargo.toml").is_some());
        for path in [
            "/../Cargo.toml",
            "/src/../Cargo.toml",
            "//etc/passwd",
            "/./Cargo.toml",
            "/",
            "Cargo.toml",
            "/src",
        ] {
            assert_eq!(files.resolve(path), None, "{path}");
        }
        let missing = files.respond(&Request::parse("https://h.test/nope", "GET"));
        assert_eq!(missing.status, 404);
        let posted = files.respond(&Request::parse("https://h.test/Cargo.toml", "POST"));
        assert_eq!(posted.status, 405);
    }
}
