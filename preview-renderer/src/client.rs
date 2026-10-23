// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Neil Blackman
//! A minimal HTTP/1.1 client for the `healthcheck` and `smoke` commands and the tests, so the
//! container needs neither Node nor curl.

use std::time::Duration;

use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use hyper::header::HeaderValue;
use hyper::{HeaderMap, Method, Request, StatusCode};
use hyper_util::rt::TokioIo;
use tokio::net::TcpStream;

use crate::config::{DEFAULT_HEIGHT, DEFAULT_WIDTH, DEVICE_SCALE_FACTOR, RenderToken};
use crate::image_info::{PNG_SIGNATURE, png_dimensions};

/// The page `smoke.js` rendered.
pub const SMOKE_HTML: &str =
    "<!doctype html><style>body{background:#123;color:white}</style><h1>preview smoke</h1>";

#[derive(Debug)]
pub struct HttpResponse {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: Bytes,
}

/// Send one request to `authority` (`host:port`). No redirects are followed and no TLS is used.
pub async fn send(
    authority: &str,
    method: Method,
    path: &str,
    body: Option<(&'static str, Bytes)>,
    timeout: Duration,
) -> Result<HttpResponse, String> {
    send_with_authorization(authority, method, path, body, None, timeout).await
}

/// [`send`] with an `authorization` header, for example [`RenderToken::header_value`]. The value
/// is marked sensitive and never appears in an error.
pub async fn send_with_authorization(
    authority: &str,
    method: Method,
    path: &str,
    body: Option<(&'static str, Bytes)>,
    authorization: Option<&str>,
    timeout: Duration,
) -> Result<HttpResponse, String> {
    tokio::time::timeout(
        timeout,
        send_inner(authority, method, path, body, authorization),
    )
    .await
    .map_err(|_| {
        format!(
            "no response from {authority} within {} ms",
            timeout.as_millis()
        )
    })?
}

async fn send_inner(
    authority: &str,
    method: Method,
    path: &str,
    body: Option<(&'static str, Bytes)>,
    authorization: Option<&str>,
) -> Result<HttpResponse, String> {
    let authorization = authorization
        .map(|value| {
            let mut value = HeaderValue::from_str(value)
                .map_err(|_| "the authorization value is not a valid header value".to_owned())?;
            value.set_sensitive(true);
            Ok::<_, String>(value)
        })
        .transpose()?;
    let stream = TcpStream::connect(authority)
        .await
        .map_err(|error| format!("connect {authority}: {error}"))?;
    let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
        .await
        .map_err(|error| error.to_string())?;
    let driver = tokio::spawn(connection);
    let mut builder = Request::builder()
        .method(method)
        .uri(path)
        .header(hyper::header::HOST, authority);
    if let Some(value) = authorization {
        builder = builder.header(hyper::header::AUTHORIZATION, value);
    }
    let payload = match body {
        Some((content_type, bytes)) => {
            builder = builder.header(
                hyper::header::CONTENT_TYPE,
                HeaderValue::from_static(content_type),
            );
            bytes
        }
        None => Bytes::new(),
    };
    let request = builder
        .body(Full::new(payload))
        .map_err(|error| error.to_string())?;
    let response = sender
        .send_request(request)
        .await
        .map_err(|error| error.to_string())?;
    let (parts, incoming) = response.into_parts();
    let body = incoming
        .collect()
        .await
        .map_err(|error| error.to_string())?
        .to_bytes();
    driver.abort();
    Ok(HttpResponse {
        status: parts.status,
        headers: parts.headers,
        body,
    })
}

/// The `host:port` part of an `http://` URL, as `new URL("/render", base)` would use it. Only
/// plain HTTP is supported: the renderer listens on an internal network without TLS.
pub fn authority_of(url: &str) -> Result<String, String> {
    let rest = url
        .strip_prefix("http://")
        .ok_or_else(|| format!("{url} is not an http:// URL"))?;
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    if authority.is_empty() || authority.contains('@') {
        return Err(format!("{url} has no usable host"));
    }
    let has_port = match authority.rfind(']') {
        Some(bracket) => authority[bracket..].contains(':'),
        None => authority.contains(':'),
    };
    Ok(if has_port {
        authority.to_owned()
    } else {
        format!("{authority}:80")
    })
}

/// `GET /health` on the local service. True only for status 200.
pub async fn healthcheck(port: u16) -> bool {
    let authority = format!("127.0.0.1:{port}");
    matches!(
        send(&authority, Method::GET, "/health", None, Duration::from_secs(4)).await,
        Ok(response) if response.status == StatusCode::OK
    )
}

/// The `smoke.js` check: render a small page and verify that a PNG of the expected size comes
/// back. Sends `token` as `authorization: Bearer <token>` when it is set. Returns the PNG size in
/// bytes.
pub async fn smoke(base_url: &str, token: Option<&RenderToken>) -> Result<usize, String> {
    let authority = authority_of(base_url)?;
    let body =
        serde_json::json!({"html": SMOKE_HTML, "width": DEFAULT_WIDTH, "height": DEFAULT_HEIGHT});
    let body = Bytes::from(serde_json::to_vec(&body).expect("a JSON value always serializes"));
    let authorization = token.map(RenderToken::header_value);
    let response = send_with_authorization(
        &authority,
        Method::POST,
        "/render",
        Some(("application/json", body)),
        authorization.as_deref(),
        Duration::from_secs(10),
    )
    .await?;
    if response.status != StatusCode::OK {
        return Err(format!(
            "renderer returned HTTP {}",
            response.status.as_u16()
        ));
    }
    let content_type = response
        .headers
        .get(hyper::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok());
    if !content_type.is_some_and(|value| value.starts_with("image/png")) {
        return Err("renderer did not return image/png".into());
    }
    if !response.body.starts_with(&PNG_SIGNATURE) {
        return Err("renderer response is not a PNG".into());
    }
    let expected = (
        DEFAULT_WIDTH * DEVICE_SCALE_FACTOR,
        DEFAULT_HEIGHT * DEVICE_SCALE_FACTOR,
    );
    match png_dimensions(&response.body) {
        Some(size) if size == expected => Ok(response.body.len()),
        other => Err(format!(
            "renderer returned a PNG of {other:?} pixels, expected {expected:?}"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authority_parsing_matches_url_resolution() {
        assert_eq!(
            authority_of("http://127.0.0.1:3000").unwrap(),
            "127.0.0.1:3000"
        );
        assert_eq!(
            authority_of("http://artifact-preview:3000/ignored/path").unwrap(),
            "artifact-preview:3000"
        );
        assert_eq!(authority_of("http://preview").unwrap(), "preview:80");
        assert_eq!(authority_of("http://[::1]:3000/").unwrap(), "[::1]:3000");
        assert_eq!(authority_of("http://[::1]").unwrap(), "[::1]:80");
        assert!(authority_of("https://preview:3000").is_err());
        assert!(authority_of("http://user@preview:3000").is_err());
        assert!(authority_of("http:///render").is_err());
    }
}
