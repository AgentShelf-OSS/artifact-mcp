//! A small blocking HTTP client for the operator commands.
//!
//! One `reqwest` client runs on a current-thread Tokio runtime. Like Node's `fetch`, it ignores
//! `HTTP(S)_PROXY` variables and needs no `HOME`. Errors are reduced to [`FetchError`], which
//! never carries a header value, so a credential sent in a header cannot reach an error message.

use std::time::Duration;

use reqwest::header::{HeaderMap, HeaderName, HeaderValue};

/// Why a request produced no response.
#[derive(Debug, PartialEq, Eq)]
pub enum FetchError {
    /// The configured timeout elapsed (Node: "The operation was aborted due to timeout").
    Timeout,
    /// Connection, TLS or protocol failure (Node: "fetch failed").
    Network,
    /// A header value is not a valid HTTP field value. The value is deliberately not kept.
    InvalidHeader(&'static str),
}

/// A complete response.
pub struct Response {
    pub status: u16,
    pub body: Vec<u8>,
}

impl Response {
    /// `response.ok`.
    pub fn ok(&self) -> bool {
        (200..300).contains(&self.status)
    }

    /// `response.text()`: UTF-8 with replacement characters and a leading BOM removed.
    pub fn text(&self) -> String {
        let text = String::from_utf8_lossy(&self.body);
        text.strip_prefix('\u{feff}').unwrap_or(&text).to_owned()
    }
}

/// The redirect statuses `fetch` treats as redirects.
pub const REDIRECT_STATUSES: [u16; 5] = [301, 302, 303, 307, 308];

pub struct Http {
    runtime: tokio::runtime::Runtime,
    client: reqwest::Client,
}

impl Http {
    /// A client with an optional whole-request timeout. With `follow_redirects` false a
    /// redirect response is returned as is, so the caller can reject it (`redirect: "error"`).
    pub fn new(timeout: Option<Duration>, follow_redirects: bool) -> Result<Self, String> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|error| format!("could not start the HTTP runtime: {error}"))?;
        let mut builder = reqwest::Client::builder().no_proxy();
        if let Some(timeout) = timeout {
            builder = builder.timeout(timeout);
        }
        builder = builder.redirect(if follow_redirects {
            reqwest::redirect::Policy::limited(20)
        } else {
            reqwest::redirect::Policy::none()
        });
        let client = builder
            .build()
            .map_err(|_| "could not initialise the HTTP client".to_owned())?;
        Ok(Self { runtime, client })
    }

    /// Send one request and read the whole body.
    pub fn send(
        &self,
        method: &str,
        url: &str,
        headers: &[(&'static str, &str)],
        body: Option<Vec<u8>>,
    ) -> Result<Response, FetchError> {
        let method =
            reqwest::Method::from_bytes(method.as_bytes()).map_err(|_| FetchError::Network)?;
        let mut header_map = HeaderMap::new();
        for (name, value) in headers {
            let header_name = HeaderName::from_static(name);
            let header_value =
                HeaderValue::from_str(value).map_err(|_| FetchError::InvalidHeader(name))?;
            header_map.insert(header_name, header_value);
        }
        self.runtime.block_on(async {
            let mut request = self.client.request(method, url).headers(header_map);
            if let Some(body) = body {
                request = request.body(body);
            }
            let response = request.send().await.map_err(classify)?;
            let status = response.status().as_u16();
            let body = response.bytes().await.map_err(classify)?;
            Ok(Response {
                status,
                body: body.to_vec(),
            })
        })
    }
}

fn classify(error: reqwest::Error) -> FetchError {
    if error.is_timeout() {
        FetchError::Timeout
    } else {
        FetchError::Network
    }
}

/// Normalize a header value like `fetch` (strip surrounding HTTP whitespace) and reject the
/// bytes `fetch` rejects. The value itself never appears in the error.
pub fn fetch_header_value(value: &str) -> Option<&str> {
    let trimmed = value.trim_matches(['\t', '\n', '\r', ' ']);
    (!trimmed.contains(['\0', '\n', '\r'])).then_some(trimmed)
}
