// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Neil Blackman
//! Requests answered inside the browser ([`crate::Page::route`], [`crate::Page::route_glob`]) and
//! a read-only file server for them ([`StaticFiles`]).

use std::future::Future;
use std::path::{Component, Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use base64::Engine;
use serde_json::{Value, json};
use tokio::sync::oneshot;

use crate::Error;
use crate::connection::{Connection, lock};
use crate::state::PageState;

type Handler = Arc<dyn Fn(&Request) -> Response + Send + Sync>;
pub(crate) type AsyncHandler =
    Arc<dyn Fn(Route) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;

/// A paused request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    pub url: String,
    pub method: String,
    /// The percent-decoded path, starting with `/`. Not normalised: `..` stays as it is.
    pub path: String,
    /// The query string without `?`, if any.
    pub query: Option<String>,
    /// Request headers as the page sent them, names in lower case. Empty for requests built by
    /// [`Request::parse`].
    pub headers: Vec<(String, String)>,
    /// The body, when the browser reported one.
    pub post_data: Option<String>,
    /// `Document`, `Fetch`, `XHR`, `Script`, ... (empty when unknown).
    pub resource_type: String,
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
            headers: Vec::new(),
            post_data: None,
            resource_type: String::new(),
        }
    }

    /// The request of a `Fetch.requestPaused` event.
    pub(crate) fn from_paused(params: &Value) -> Self {
        let text = |pointer: &str| params.pointer(pointer).and_then(Value::as_str);
        let mut request = Self::parse(
            text("/request/url").unwrap_or(""),
            text("/request/method").unwrap_or("GET"),
        );
        if let Some(fragment) = text("/request/urlFragment") {
            request.url.push_str(fragment);
        }
        request.headers = params
            .pointer("/request/headers")
            .and_then(Value::as_object)
            .map(|headers| {
                headers
                    .iter()
                    .map(|(name, value)| {
                        (
                            name.to_ascii_lowercase(),
                            value.as_str().unwrap_or("").to_owned(),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default();
        request.post_data = text("/request/postData").map(str::to_owned).or_else(|| {
            let entries = params.pointer("/request/postDataEntries")?.as_array()?;
            let mut body = Vec::new();
            for entry in entries {
                let bytes = entry.get("bytes").and_then(Value::as_str).unwrap_or("");
                body.extend(
                    base64::engine::general_purpose::STANDARD
                        .decode(bytes)
                        .ok()?,
                );
            }
            String::from_utf8(body).ok()
        });
        request.resource_type = text("/resourceType").unwrap_or("").to_owned();
        request
    }

    /// A header by case-insensitive name.
    pub fn header(&self, name: &str) -> Option<&str> {
        crate::network::find_header(&self.headers, name)
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

    /// A JSON body with `status`.
    pub fn json(status: u16, body: &Value) -> Self {
        Self::new(status, "application/json", body.to_string().into_bytes())
    }

    /// Add or replace a header.
    pub fn header(mut self, name: &str, value: &str) -> Self {
        self.headers
            .retain(|(existing, _)| !existing.eq_ignore_ascii_case(name));
        self.headers.push((name.to_owned(), value.to_owned()));
        self
    }

    /// The body as UTF-8 text (lossy).
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

/// Which URLs a route answers.
#[derive(Clone)]
pub(crate) enum Matcher {
    /// [`crate::Page::route`]: the URL starts with this prefix.
    Prefix(String),
    /// [`crate::Page::route_glob`]: a Playwright glob.
    Glob(String),
    /// [`crate::Page::route_where`].
    Predicate(Arc<dyn Fn(&str) -> bool + Send + Sync>),
}

impl Matcher {
    pub(crate) fn matches(&self, url: &str) -> bool {
        match self {
            Self::Prefix(prefix) => url.starts_with(prefix.as_str()),
            Self::Glob(glob) => glob_matches(glob, url),
            Self::Predicate(predicate) => predicate(url),
        }
    }

    /// The DevTools `urlPattern` that catches this route's requests.
    pub(crate) fn pattern(&self) -> String {
        match self {
            Self::Prefix(prefix) => {
                let mut pattern = String::new();
                for character in prefix.chars() {
                    if matches!(character, '*' | '?' | '\\') {
                        pattern.push('\\');
                    }
                    pattern.push(character);
                }
                pattern.push('*');
                pattern
            }
            Self::Glob(_) | Self::Predicate(_) => "*".to_owned(),
        }
    }
}

#[derive(Clone)]
pub(crate) enum RouteHandler {
    /// Answers synchronously ([`crate::Page::route`]).
    Fulfil(Handler),
    /// Gets a [`Route`] to fulfil, continue, abort or fall back ([`crate::Page::route_glob`]).
    Async(AsyncHandler),
}

/// One registered route.
#[derive(Clone)]
pub(crate) struct RouteEntry {
    pub(crate) matcher: Matcher,
    pub(crate) handler: RouteHandler,
    /// The text `unroute` matches (the glob), if any.
    pub(crate) key: Option<String>,
}

impl RouteEntry {
    pub(crate) fn prefix(
        prefix: &str,
        handler: impl Fn(&Request) -> Response + Send + Sync + 'static,
    ) -> Self {
        Self {
            matcher: Matcher::Prefix(prefix.to_owned()),
            handler: RouteHandler::Fulfil(Arc::new(handler)),
            key: None,
        }
    }

    pub(crate) fn is_prefix(&self) -> bool {
        matches!(self.matcher, Matcher::Prefix(_))
    }
}

/// The handlers that may answer `url`, in the order they are asked: glob and predicate routes
/// newest first (as in Playwright, so a later route overrides an earlier one, and
/// [`Route::fallback`] passes to the next older one), then the first matching prefix route
/// ([`crate::Page::route`] keeps its first-match-wins rule).
pub(crate) fn candidates(routes: &[RouteEntry], url: &str) -> Vec<RouteHandler> {
    let mut found: Vec<RouteHandler> = routes
        .iter()
        .rev()
        .filter(|route| !route.is_prefix() && route.matcher.matches(url))
        .map(|route| route.handler.clone())
        .collect();
    if let Some(route) = routes
        .iter()
        .find(|route| route.is_prefix() && route.matcher.matches(url))
    {
        found.push(route.handler.clone());
    }
    found
}

/// The `Fetch.enable` patterns for `routes`, or `None` when nothing is routed.
pub(crate) fn fetch_patterns(routes: &[RouteEntry]) -> Option<Value> {
    if routes.is_empty() {
        return None;
    }
    let mut patterns: Vec<String> = routes.iter().map(|route| route.matcher.pattern()).collect();
    if patterns.iter().any(|pattern| pattern == "*") {
        patterns = vec!["*".to_owned()];
    }
    Some(Value::Array(
        patterns
            .into_iter()
            .map(|pattern| json!({"urlPattern": pattern, "requestStage": "Request"}))
            .collect(),
    ))
}

/// Whether `url` matches the Playwright glob `glob` (Playwright 1.60's `globToRegexPattern`):
/// `*` is any run of characters except `/`; `**/` is any run ending in `/` (empty too when the
/// glob has `/` before it); `**` elsewhere is any run of characters; `{a,b}` is either
/// alternative; `\` escapes the next character; everything else (`?` too) is literal.
pub fn glob_matches(glob: &str, url: &str) -> bool {
    let tokens = parse_glob(glob);
    let url: Vec<char> = url.chars().collect();
    match_tokens(&tokens, &url)
}

#[derive(Debug, Clone, PartialEq)]
enum GlobToken {
    Literal(char),
    /// `*`: `[^/]*`.
    Star,
    /// `**` not followed by `/`: `.*`.
    Any,
    /// `**/`: `.*/`, or `((.+/)|)` when `/` comes before it.
    Segments {
        after_slash: bool,
    },
    /// `{a,b}`.
    Group(Vec<Vec<GlobToken>>),
}

fn parse_glob(glob: &str) -> Vec<GlobToken> {
    let chars: Vec<char> = glob.chars().collect();
    let mut tokens = Vec::new();
    let mut group: Option<Vec<Vec<GlobToken>>> = None;
    let mut index = 0;
    let push = |token: GlobToken,
                tokens: &mut Vec<GlobToken>,
                group: &mut Option<Vec<Vec<GlobToken>>>| match group {
        Some(alternatives) => alternatives
            .last_mut()
            .expect("a group has an alternative")
            .push(token),
        None => tokens.push(token),
    };
    while index < chars.len() {
        let character = chars[index];
        match character {
            '\\' if index + 1 < chars.len() => {
                index += 1;
                push(GlobToken::Literal(chars[index]), &mut tokens, &mut group);
            }
            '*' => {
                let before = index.checked_sub(1).map(|before| chars[before]);
                let mut stars = 1;
                while chars.get(index + 1) == Some(&'*') {
                    stars += 1;
                    index += 1;
                }
                let token = if stars == 1 {
                    GlobToken::Star
                } else if chars.get(index + 1) == Some(&'/') {
                    index += 1;
                    GlobToken::Segments {
                        after_slash: before == Some('/'),
                    }
                } else {
                    GlobToken::Any
                };
                push(token, &mut tokens, &mut group);
            }
            '{' if group.is_none() => group = Some(vec![Vec::new()]),
            '}' if group.is_some() => {
                let alternatives = group.take().unwrap_or_default();
                tokens.push(GlobToken::Group(alternatives));
            }
            ',' if group.is_some() => {
                if let Some(alternatives) = &mut group {
                    alternatives.push(Vec::new());
                }
            }
            other => push(GlobToken::Literal(other), &mut tokens, &mut group),
        }
        index += 1;
    }
    if let Some(alternatives) = group {
        // An unclosed group: its text is literal.
        tokens.push(GlobToken::Literal('{'));
        for (position, alternative) in alternatives.into_iter().enumerate() {
            if position > 0 {
                tokens.push(GlobToken::Literal(','));
            }
            tokens.extend(alternative);
        }
    }
    tokens
}

fn match_tokens(tokens: &[GlobToken], input: &[char]) -> bool {
    let Some((first, rest)) = tokens.split_first() else {
        return input.is_empty();
    };
    match first {
        GlobToken::Literal(character) => {
            input.first() == Some(character) && match_tokens(rest, &input[1..])
        }
        GlobToken::Star => {
            let mut end = 0;
            loop {
                if match_tokens(rest, &input[end..]) {
                    return true;
                }
                if end == input.len() || input[end] == '/' {
                    return false;
                }
                end += 1;
            }
        }
        GlobToken::Any => (0..=input.len()).any(|end| match_tokens(rest, &input[end..])),
        GlobToken::Segments { after_slash } => (0..=input.len()).any(|end| {
            let ends_in_slash = end > 0 && input[end - 1] == '/';
            let fits = if *after_slash {
                end == 0 || (end >= 2 && ends_in_slash)
            } else {
                ends_in_slash
            };
            fits && match_tokens(rest, &input[end..])
        }),
        GlobToken::Group(alternatives) => alternatives.iter().any(|alternative| {
            let mut joined = alternative.clone();
            joined.extend_from_slice(rest);
            match_tokens(&joined, input)
        }),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Outcome {
    Pending,
    Handled,
    Fallback,
}

/// A paused request handed to a [`crate::Page::route_glob`] handler. Answer it with
/// [`Route::fulfill`], [`Route::continue_request`] or [`Route::abort`], or pass it to the next
/// matching route with [`Route::fallback`]. A handler that returns without answering continues
/// the request unchanged.
pub struct Route {
    request: Request,
    request_id: String,
    session: String,
    connection: Weak<Connection>,
    state: Weak<PageState>,
    timeout: Duration,
    outcome: Arc<Mutex<Outcome>>,
}

impl Route {
    pub(crate) fn new(
        request: Request,
        request_id: String,
        session: String,
        connection: Weak<Connection>,
        state: Weak<PageState>,
        timeout: Duration,
    ) -> (Self, Arc<Mutex<Outcome>>) {
        let outcome = Arc::new(Mutex::new(Outcome::Pending));
        let route = Self {
            request,
            request_id,
            session,
            connection,
            state,
            timeout,
            outcome: Arc::clone(&outcome),
        };
        (route, outcome)
    }

    pub fn request(&self) -> &Request {
        &self.request
    }

    async fn send(&self, method: &str, params: Value) -> Result<Value, Error> {
        let connection = self
            .connection
            .upgrade()
            .ok_or_else(|| Error::Closed(format!("{method}: the browser is gone")))?;
        connection
            .call(Some(&self.session), method, params, self.timeout)
            .await
    }

    fn settle(&self, outcome: Outcome) {
        *lock(&self.outcome) = outcome;
    }

    /// Answer with `response`; nothing reaches the network.
    pub async fn fulfill(self, response: Response) -> Result<(), Error> {
        self.settle(Outcome::Handled);
        self.send(
            "Fetch.fulfillRequest",
            fulfil_params(&self.request_id, &response),
        )
        .await?;
        Ok(())
    }

    /// Let the request go on unchanged.
    pub async fn continue_request(self) -> Result<(), Error> {
        self.settle(Outcome::Handled);
        self.send(
            "Fetch.continueRequest",
            json!({"requestId": self.request_id}),
        )
        .await?;
        Ok(())
    }

    /// Fail the request with a DevTools `Network.ErrorReason` such as `Failed`, `Aborted`,
    /// `ConnectionRefused` or `BlockedByClient`.
    pub async fn abort(self, reason: &str) -> Result<(), Error> {
        self.settle(Outcome::Handled);
        self.send(
            "Fetch.failRequest",
            json!({"requestId": self.request_id, "errorReason": reason}),
        )
        .await?;
        Ok(())
    }

    /// Hand the request to the next matching (older) route, or to the network if none is left.
    pub fn fallback(self) {
        self.settle(Outcome::Fallback);
    }

    /// Send the request on and return the real response without passing it to the page yet
    /// (Playwright's `route.fetch()`). Then answer with [`Route::fulfill`] (for example with a
    /// changed body) or [`Route::continue_request`] (to pass the response through unchanged).
    pub async fn fetch(&mut self) -> Result<Response, Error> {
        let state = self
            .state
            .upgrade()
            .ok_or_else(|| Error::Closed("route.fetch: the page is gone".into()))?;
        let (sender, receiver) = oneshot::channel();
        lock(&state.response_stage).insert(self.request_id.clone(), sender);
        drop(state);
        self.send(
            "Fetch.continueRequest",
            json!({"requestId": self.request_id, "interceptResponse": true}),
        )
        .await?;
        let (session, paused) = tokio::time::timeout(self.timeout, receiver)
            .await
            .map_err(|_| Error::Timeout {
                what: format!("response of {}", self.request.url),
                after: self.timeout,
            })?
            .map_err(|_| Error::Closed("route.fetch: the page is gone".into()))?;
        self.session = session;
        if let Some(reason) = paused.get("responseErrorReason").and_then(Value::as_str) {
            return Err(Error::Navigation(format!("{}: {reason}", self.request.url)));
        }
        let status = paused
            .get("responseStatusCode")
            .and_then(Value::as_u64)
            .and_then(|status| u16::try_from(status).ok())
            .unwrap_or(0);
        let headers = paused
            .get("responseHeaders")
            .and_then(Value::as_array)
            .map(|headers| {
                headers
                    .iter()
                    .map(|header| {
                        (
                            header["name"].as_str().unwrap_or("").to_owned(),
                            header["value"].as_str().unwrap_or("").to_owned(),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default();
        let body = self
            .send(
                "Fetch.getResponseBody",
                json!({"requestId": self.request_id}),
            )
            .await?;
        let text = body.get("body").and_then(Value::as_str).unwrap_or("");
        let bytes = if body.get("base64Encoded").and_then(Value::as_bool) == Some(true) {
            base64::engine::general_purpose::STANDARD
                .decode(text)
                .map_err(|error| Error::Protocol(format!("response body: {error}")))?
        } else {
            text.as_bytes().to_vec()
        };
        Ok(Response {
            status,
            headers,
            body: bytes,
        })
    }
}

pub(crate) fn fulfil_params(request_id: &str, response: &Response) -> Value {
    let headers: Vec<Value> = response
        .headers
        .iter()
        .map(|(name, value)| json!({"name": name, "value": value}))
        .collect();
    json!({
        "requestId": request_id,
        "responseCode": response.status,
        "responseHeaders": headers,
        "body": base64::engine::general_purpose::STANDARD.encode(&response.body),
    })
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
        let route = Matcher::Prefix("https://h.test/a*b?".into());
        assert_eq!(route.pattern(), "https://h.test/a\\*b\\?*");
        assert!(route.matches("https://h.test/a*b?/x"));
        assert!(!route.matches("https://other.test/"));
    }

    #[test]
    fn globs_follow_playwright() {
        let url = "http://127.0.0.1:3483/gallery/preferences?org=a";
        assert!(glob_matches("**/gallery/preferences*", url));
        assert!(!glob_matches("**/gallery/preference", url));
        assert!(glob_matches(
            "**/collections*include=projection*",
            "http://127.0.0.1:1/collections?org=x&include=projection"
        ));
        assert!(glob_matches("**/collections", "http://h/collections"));
        assert!(!glob_matches("**/collections", "http://h/collections/abc"));
        // `**` without a following `/` crosses segments.
        assert!(glob_matches(
            "**/collections/abc**",
            "http://h/collections/abc"
        ));
        assert!(glob_matches(
            "**/collections/abc**",
            "http://h/collections/abc/members"
        ));
        assert!(glob_matches(
            "**/collections/abc/memberships*",
            "http://h/collections/abc/memberships?x=1"
        ));
        assert!(glob_matches("**/x/*.js", "http://h/a/b/x/app.js"));
        assert!(!glob_matches("**/x/*.js", "http://h/x/a/app.js"));
        assert!(glob_matches("http://h/a/**/b", "http://h/a/b"));
        assert!(glob_matches("http://h/a/**/b", "http://h/a/x/y/b"));
        assert!(glob_matches("http://h/{a,b}.css", "http://h/b.css"));
        assert!(glob_matches("http://h/a?b", "http://h/a?b"));
        assert!(!glob_matches("http://h/a?b", "http://h/axb"));
        assert!(glob_matches(
            "**/__qa_hold_shell",
            "http://h/__qa_hold_shell"
        ));
        assert!(glob_matches("**/abc", "http://h/x/abc"));
        assert!(!glob_matches("**/abc", "http://h/x/zabc"));
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
