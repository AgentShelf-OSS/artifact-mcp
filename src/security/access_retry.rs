//! Cloudflare Access retry decisions.
//!
//! # Why this exists
//!
//! Cloudflare Access sets the `CF_Authorization` cookie on the login redirect but does not always
//! attach `Cf-Access-Jwt-Assertion` to that same first navigation. In `jwt` mode the identity
//! resolver only trusts the assertion, so the landing request resolves anonymous and the shell
//! answers 404; a manual reload then works. This turns exactly that state — cookie present,
//! assertion absent — into one automatic, once-only reload of the same URL.
//!
//! # Guards, all of which must hold
//!
//! | Guard | Reason |
//! |---|---|
//! | mode is `jwt` | `header-trust` and `disabled` never need an assertion |
//! | method is `GET` | a retry must not replay a mutation |
//! | a `CF_Authorization` cookie is present | otherwise the user is simply signed out |
//! | `Cf-Access-Jwt-Assertion` is absent/empty | a present assertion means verification already had its chance |
//! | the path is `/` or a single non-reserved artifact id | never a raw, share, thumbnail, or sub-resource route |
//! | the retry parameter is not already set | once only, so a persistent failure cannot loop |
//!
//! Only a path plus query is ever returned, so a protocol-relative request target such as `//evil.example/abcdef123456` yields `/abcdef123456?cf_access_retry=1` and cannot become an open redirect.

use axum::http::{HeaderMap, Method, Uri};
use url::{Url, form_urlencoded};

use crate::{
    artifacts::validation::is_reserved_artifact_id,
    config::AccessIdentityMode,
    security::identity::{access_assertion_header, read_access_cookie},
};

/// `ACCESS_RETRY_PARAM` — the query parameter marking a request as already retried.
pub const ACCESS_RETRY_PARAM: &str = "cf_access_retry";

/// The synthetic origin used to resolve a request target.
const REQUEST_TARGET_BASE: &str = "http://artifact-mcp.local";

/// `isRetryablePath(pathname)`
///
/// `/` is the gallery. Otherwise the path must be exactly one segment that could name an
/// artifact: any nested path (`/raw/…`, `/abcdef123456/history`) and every reserved or malformed
/// id is refused. The empty id is covered because `is_reserved_artifact_id("")` is true.
#[must_use]
pub fn is_retryable_path(pathname: &str) -> bool {
    if pathname == "/" {
        return true;
    }
    let Some(id) = pathname.strip_prefix('/') else {
        return false;
    };
    !id.contains('/') && !is_reserved_artifact_id(id)
}

/// `accessRetryTarget(req, { mode, param })` — the URL to reload, or `None`.
///
/// `target` is the raw request target (`req.url`), i.e. path plus query.
#[must_use]
pub fn access_retry_target(
    method: &Method,
    uri: &Uri,
    headers: &HeaderMap,
    mode: AccessIdentityMode,
    param: &str,
) -> Option<String> {
    if mode != AccessIdentityMode::Jwt || method != Method::GET {
        return None;
    }
    read_access_cookie(headers)?;
    if access_assertion_header(headers).is_some() {
        return None;
    }

    let target = uri.path_and_query().map_or("/", |value| value.as_str());
    // `req.url || "/"`.
    let target = if target.is_empty() { "/" } else { target };
    let base = Url::parse(REQUEST_TARGET_BASE).ok()?;
    // A target the URL parser rejects yields no retry.
    let parsed = Url::options().base_url(Some(&base)).parse(target).ok()?;

    if !is_retryable_path(parsed.path()) {
        return None;
    }
    if parsed.query_pairs().any(|(name, _)| name == param) {
        return None;
    }

    // `searchParams.set` re-serializes the *whole* query, so an existing `%20` comes back as `+`.
    // Rebuilding every pair through the same form-urlencoded serializer reproduces that; simply
    // appending to the raw query string would not.
    let mut query = form_urlencoded::Serializer::new(String::new());
    for (name, value) in parsed.query_pairs() {
        query.append_pair(&name, &value);
    }
    query.append_pair(param, "1");
    Some(format!("{}?{}", parsed.path(), query.finish()))
}
