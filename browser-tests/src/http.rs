// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Neil Blackman
//! A small HTTP/1.1 client for a server on this machine: Playwright's `request` fixture
//! (`APIRequestContext`) for the end-to-end tests. Plain `http://` only, one connection per
//! request (`Connection: close`), `Content-Length` and chunked responses.

use std::time::Duration;

use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::Error;
use crate::network::find_header;

/// A response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpResponse {
    pub status: u16,
    /// Header names in lower case, in arrival order.
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl HttpResponse {
    /// Status 200–299 (Playwright's `ok()`).
    pub fn ok(&self) -> bool {
        (200..300).contains(&self.status)
    }

    /// A header by case-insensitive name.
    pub fn header(&self, name: &str) -> Option<&str> {
        find_header(&self.headers, name)
    }

    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    /// The body as JSON, or `Value::Null` when it is not JSON.
    pub fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or(Value::Null)
    }
}

/// A request body.
#[derive(Debug, Clone)]
pub enum Body {
    /// Sent with `content-type: application/json` (Playwright's `data` with an object).
    Json(Value),
    /// Sent as is, with this content type.
    Raw {
        content_type: String,
        bytes: Vec<u8>,
    },
}

/// Requests against one base URL with fixed extra headers (Playwright's `request` fixture with
/// `extraHTTPHeaders`). Cheap to clone.
#[derive(Debug, Clone)]
pub struct Client {
    base_url: String,
    headers: Vec<(String, String)>,
    timeout: Duration,
}

impl Client {
    /// `base_url` is `http://host:port` (relative paths are appended to it).
    pub fn new(base_url: &str, headers: &[(&str, &str)]) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_owned(),
            headers: headers
                .iter()
                .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
                .collect(),
            timeout: Duration::from_secs(30),
        }
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// The same client with `headers` added (replacing headers of the same name).
    pub fn with_headers(&self, headers: &[(&str, &str)]) -> Self {
        let mut next = self.clone();
        for (name, value) in headers {
            next.headers
                .retain(|(existing, _)| !existing.eq_ignore_ascii_case(name));
            next.headers.push(((*name).to_owned(), (*value).to_owned()));
        }
        next
    }

    pub async fn get(&self, path: &str) -> Result<HttpResponse, Error> {
        self.send("GET", path, None, &[]).await
    }

    pub async fn delete(&self, path: &str) -> Result<HttpResponse, Error> {
        self.send("DELETE", path, None, &[]).await
    }

    pub async fn post_json(&self, path: &str, body: &Value) -> Result<HttpResponse, Error> {
        self.send("POST", path, Some(Body::Json(body.clone())), &[])
            .await
    }

    /// Send `method` to `path` (relative to the base URL, or a full `http://` URL) with an
    /// optional body and extra headers for this request.
    pub async fn send(
        &self,
        method: &str,
        path: &str,
        body: Option<Body>,
        headers: &[(&str, &str)],
    ) -> Result<HttpResponse, Error> {
        let url = if path.starts_with("http://") {
            path.to_owned()
        } else {
            format!("{}{path}", self.base_url)
        };
        let rest = url
            .strip_prefix("http://")
            .ok_or_else(|| Error::Protocol(format!("only http:// URLs are supported: {url}")))?;
        let (authority, target) = match rest.find('/') {
            Some(slash) => (&rest[..slash], &rest[slash..]),
            None => (rest, "/"),
        };
        let mut all: Vec<(String, String)> = vec![
            ("Host".into(), authority.to_owned()),
            ("Connection".into(), "close".into()),
            ("Accept".into(), "*/*".into()),
            ("User-Agent".into(), "browser-tests".into()),
        ];
        let mut add = |name: &str, value: &str| {
            all.retain(|(existing, _)| !existing.eq_ignore_ascii_case(name));
            all.push((name.to_owned(), value.to_owned()));
        };
        for (name, value) in &self.headers {
            add(name, value);
        }
        let bytes = match &body {
            Some(Body::Json(value)) => {
                if !headers
                    .iter()
                    .any(|(name, _)| name.eq_ignore_ascii_case("content-type"))
                {
                    add("Content-Type", "application/json");
                }
                value.to_string().into_bytes()
            }
            Some(Body::Raw {
                content_type,
                bytes,
            }) => {
                add("Content-Type", content_type);
                bytes.clone()
            }
            None => Vec::new(),
        };
        for (name, value) in headers {
            add(name, value);
        }
        if body.is_some() || !matches!(method, "GET" | "HEAD" | "DELETE") {
            add("Content-Length", &bytes.len().to_string());
        }
        let mut request = format!("{method} {target} HTTP/1.1\r\n");
        for (name, value) in &all {
            request.push_str(&format!("{name}: {value}\r\n"));
        }
        request.push_str("\r\n");
        let mut request = request.into_bytes();
        request.extend_from_slice(&bytes);
        tokio::time::timeout(self.timeout, exchange(authority, &request))
            .await
            .map_err(|_| Error::Timeout {
                what: format!("{method} {url}"),
                after: self.timeout,
            })?
            .map_err(|error| Error::Protocol(format!("{method} {url}: {error}")))
    }
}

async fn exchange(authority: &str, request: &[u8]) -> Result<HttpResponse, String> {
    let mut stream = TcpStream::connect(authority)
        .await
        .map_err(|error| error.to_string())?;
    stream
        .write_all(request)
        .await
        .map_err(|error| error.to_string())?;
    let mut raw = Vec::new();
    stream
        .read_to_end(&mut raw)
        .await
        .map_err(|error| error.to_string())?;
    parse_response(&raw)
}

fn parse_response(raw: &[u8]) -> Result<HttpResponse, String> {
    let end = raw
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .ok_or("the response has no header end")?;
    let head = String::from_utf8_lossy(&raw[..end]);
    let mut lines = head.split("\r\n");
    let status_line = lines.next().unwrap_or("");
    let status = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|status| status.parse().ok())
        .ok_or_else(|| format!("bad status line {status_line:?}"))?;
    let headers: Vec<(String, String)> = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_owned()))
        .collect();
    let rest = &raw[end + 4..];
    let chunked = find_header(&headers, "transfer-encoding")
        .is_some_and(|value| value.to_ascii_lowercase().contains("chunked"));
    let body = if chunked {
        decode_chunked(rest)?
    } else if let Some(length) =
        find_header(&headers, "content-length").and_then(|value| value.parse::<usize>().ok())
    {
        rest[..length.min(rest.len())].to_vec()
    } else {
        rest.to_vec()
    };
    Ok(HttpResponse {
        status,
        headers,
        body,
    })
}

fn decode_chunked(mut rest: &[u8]) -> Result<Vec<u8>, String> {
    let mut body = Vec::new();
    loop {
        let line_end = rest
            .windows(2)
            .position(|window| window == b"\r\n")
            .ok_or("truncated chunk size")?;
        let size_text = String::from_utf8_lossy(&rest[..line_end]);
        let size = usize::from_str_radix(size_text.split(';').next().unwrap_or("").trim(), 16)
            .map_err(|_| format!("bad chunk size {size_text:?}"))?;
        rest = &rest[line_end + 2..];
        if size == 0 {
            return Ok(body);
        }
        if rest.len() < size {
            return Err("truncated chunk".into());
        }
        body.extend_from_slice(&rest[..size]);
        rest = rest.get(size + 2..).unwrap_or(&[]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn responses_parse_with_length_or_chunks() {
        let plain = parse_response(
            b"HTTP/1.1 404 Not Found\r\nContent-Length: 9\r\nX-A: b\r\n\r\nNot found",
        )
        .unwrap();
        assert_eq!((plain.status, plain.text().as_str()), (404, "Not found"));
        assert_eq!(plain.header("x-a"), Some("b"));
        let chunked = parse_response(
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n6;x=y\r\n world\r\n0\r\n\r\n",
        )
        .unwrap();
        assert_eq!(chunked.text(), "hello world");
        assert!(parse_response(b"garbage").is_err());
    }
}
