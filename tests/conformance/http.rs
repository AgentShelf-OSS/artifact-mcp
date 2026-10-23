// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Neil Blackman
//
// A minimal HTTP/1.1 client that behaves like the `node:http` request the goldens were recorded
// with.
//
// The request-target is sent VERBATIM. It is deliberately NOT run through a URL parser, which
// would collapse dot-segments and percent-decode `%2e`; that normalization would silently defuse
// the bundle path-traversal cases before they ever reach the server.
//
// The request headers match what `node:http` sends: the case headers, then `Host`, then
// `Connection: keep-alive`, and `Transfer-Encoding: chunked` (with an empty chunked body) for a
// body-less POST, PUT or PATCH. Response headers are folded with the `node:http` duplicate rules
// and decoded as Latin-1, as `IncomingMessage.headers` does.

use std::io::{BufRead, BufReader, Write};
use std::net::{Shutdown, TcpStream};
use std::thread;
use std::time::Duration;

#[derive(Clone, Debug)]
pub enum HeaderValue {
    One(String),
    Many(Vec<String>),
}

/// Response headers in arrival order with lowercase names, like `IncomingMessage.headers`.
#[derive(Clone, Debug, Default)]
pub struct Headers(Vec<(String, HeaderValue)>);

/// Duplicates of these response headers are dropped (the first value wins) by `node:http`.
const SINGLE_VALUE_HEADERS: [&str; 18] = [
    "age",
    "authorization",
    "content-length",
    "content-type",
    "etag",
    "expires",
    "from",
    "host",
    "if-modified-since",
    "if-unmodified-since",
    "last-modified",
    "location",
    "max-forwards",
    "proxy-authorization",
    "referer",
    "retry-after",
    "server",
    "user-agent",
];

impl Headers {
    fn add(&mut self, name: String, value: String) {
        let existing = self.0.iter_mut().find(|(known, _)| *known == name);
        match (existing, name.as_str()) {
            (None, "set-cookie") => self.0.push((name, HeaderValue::Many(vec![value]))),
            (None, _) => self.0.push((name, HeaderValue::One(value))),
            (Some((_, HeaderValue::Many(values))), _) => values.push(value),
            (Some((_, HeaderValue::One(current))), "cookie") => {
                current.push_str("; ");
                current.push_str(&value);
            }
            (Some(_), known) if SINGLE_VALUE_HEADERS.contains(&known) => {}
            (Some((_, HeaderValue::One(current))), _) => {
                current.push_str(", ");
                current.push_str(&value);
            }
        }
    }

    pub fn iter(&self) -> impl Iterator<Item = (&String, &HeaderValue)> {
        self.0.iter().map(|(name, value)| (name, value))
    }

    fn first(&self, name: &str) -> Option<String> {
        self.0
            .iter()
            .find(|(known, _)| known == name)
            .map(|(_, value)| match value {
                HeaderValue::One(value) => value.clone(),
                HeaderValue::Many(values) => values.join(", "),
            })
    }
}

pub struct Response {
    pub status: u16,
    pub headers: Headers,
    pub body: Vec<u8>,
}

pub struct Request<'a> {
    pub method: &'a str,
    pub path: &'a str,
    /// Header names as the case wrote them (already lowercased by the runner), in order.
    pub headers: &'a [(String, String)],
    pub body: Option<&'a [u8]>,
}

/// Methods for which `node:http` does not default to chunked encoding.
const NO_DEFAULT_BODY: [&str; 6] = ["GET", "HEAD", "DELETE", "OPTIONS", "TRACE", "CONNECT"];

fn latin1(text: &str, what: &str) -> Result<Vec<u8>, String> {
    text.chars()
        .map(|character| {
            u8::try_from(u32::from(character))
                .map_err(|_| format!("{what} contains a character outside Latin-1: {text:?}"))
        })
        .collect()
}

fn encode(port: u16, request: &Request<'_>) -> Result<(Vec<u8>, String), String> {
    let method = request.method.to_ascii_uppercase();
    if method.is_empty() || !method.bytes().all(|byte| byte.is_ascii_graphic()) {
        return Err(format!("invalid HTTP method: {method:?}"));
    }
    if request
        .path
        .chars()
        .any(|character| !('\u{21}'..='\u{ff}').contains(&character))
    {
        return Err(format!(
            "Request path contains unescaped characters: {:?}",
            request.path
        ));
    }
    let mut out = format!("{method} ").into_bytes();
    out.extend(latin1(request.path, "request path")?);
    out.extend_from_slice(b" HTTP/1.1\r\n");
    let has = |name: &str| {
        request
            .headers
            .iter()
            .any(|(known, _)| known.eq_ignore_ascii_case(name))
    };
    for (name, value) in request.headers {
        if value
            .chars()
            .any(|character| character != '\t' && (character < ' ' || character == '\u{7f}'))
        {
            return Err(format!("invalid character in header {name}: {value:?}"));
        }
        out.extend(latin1(name, "header name")?);
        out.extend_from_slice(b": ");
        out.extend(latin1(value, "header value")?);
        out.extend_from_slice(b"\r\n");
    }
    if !has("host") {
        out.extend(format!("Host: 127.0.0.1:{port}\r\n").into_bytes());
    }
    if !has("connection") {
        out.extend_from_slice(b"Connection: keep-alive\r\n");
    }
    let chunked = request.body.is_none()
        && !has("content-length")
        && !has("transfer-encoding")
        && !NO_DEFAULT_BODY.contains(&method.as_str());
    if chunked {
        out.extend_from_slice(b"Transfer-Encoding: chunked\r\n");
    }
    out.extend_from_slice(b"\r\n");
    match request.body {
        Some(body) => out.extend_from_slice(body),
        None if chunked => out.extend_from_slice(b"0\r\n\r\n"),
        None => {}
    }
    Ok((out, method))
}

/// Send one request on a fresh connection and read one response.
///
/// The request is written from a second thread while the response is read, as `node:http` does,
/// so a server that answers (for example `413`) before it has read a large body is still observed.
pub fn send(port: u16, request: &Request<'_>, timeout: Duration) -> Result<Response, String> {
    let (bytes, method) = encode(port, request)?;
    let stream = TcpStream::connect(("127.0.0.1", port))
        .map_err(|error| format!("connect 127.0.0.1:{port}: {error}"))?;
    stream
        .set_read_timeout(Some(timeout))
        .and_then(|()| stream.set_write_timeout(Some(timeout)))
        .and_then(|()| stream.set_nodelay(true))
        .map_err(|error| format!("configure socket: {error}"))?;
    let mut writer = stream
        .try_clone()
        .map_err(|error| format!("clone socket: {error}"))?;
    let write = thread::spawn(move || writer.write_all(&bytes).and_then(|()| writer.flush()));
    let response = read_response(&mut BufReader::new(&stream), method == "HEAD");
    // Unblock a writer that is still sending a body the server will never read.
    let _ = stream.shutdown(Shutdown::Both);
    let written = write
        .join()
        .unwrap_or_else(|_| Err(std::io::Error::other("request writer panicked")));
    match (response, written) {
        (Ok(response), _) => Ok(response),
        (Err(error), Err(write_error)) => Err(format!("{error} (request write: {write_error})")),
        (Err(error), Ok(())) => Err(error),
    }
}

fn read_line(reader: &mut impl BufRead) -> Result<Option<Vec<u8>>, String> {
    let mut line = Vec::new();
    let read = reader
        .read_until(b'\n', &mut line)
        .map_err(|error| format!("read response: {error}"))?;
    if read == 0 {
        return Ok(None);
    }
    while matches!(line.last(), Some(b'\n' | b'\r')) {
        line.pop();
    }
    Ok(Some(line))
}

fn read_response(reader: &mut impl BufRead, head_request: bool) -> Result<Response, String> {
    loop {
        let status_line = read_line(reader)?.ok_or("socket hang up")?;
        let status_line = String::from_utf8_lossy(&status_line).into_owned();
        let status = status_line
            .split(' ')
            .nth(1)
            .and_then(|code| code.parse::<u16>().ok())
            .filter(|_| status_line.starts_with("HTTP/1."))
            .ok_or_else(|| format!("malformed status line: {status_line:?}"))?;
        let mut headers = Headers::default();
        loop {
            let line = read_line(reader)?.ok_or("connection closed inside response headers")?;
            if line.is_empty() {
                break;
            }
            let colon = line.iter().position(|byte| *byte == b':').ok_or_else(|| {
                format!(
                    "malformed header line: {:?}",
                    String::from_utf8_lossy(&line)
                )
            })?;
            // node:http decodes header text as Latin-1 and trims surrounding whitespace.
            let name: String = line[..colon]
                .iter()
                .map(|byte| char::from(*byte).to_ascii_lowercase())
                .collect();
            let value: String = line[colon + 1..]
                .iter()
                .map(|byte| char::from(*byte))
                .collect();
            headers.add(name, value.trim_matches([' ', '\t']).to_owned());
        }
        if (100..200).contains(&status) && status != 101 {
            continue;
        }
        let body = if head_request || (100..200).contains(&status) || status == 204 || status == 304
        {
            Vec::new()
        } else if headers.first("transfer-encoding").is_some_and(|value| {
            value
                .rsplit(',')
                .next()
                .is_some_and(|last| last.trim().eq_ignore_ascii_case("chunked"))
        }) {
            read_chunked(reader)?
        } else if let Some(length) = headers.first("content-length") {
            let length: usize = length
                .trim()
                .parse()
                .map_err(|_| format!("invalid content-length: {length:?}"))?;
            let mut body = vec![0; length];
            reader
                .read_exact(&mut body)
                .map_err(|error| format!("read response body: {error}"))?;
            body
        } else {
            let mut body = Vec::new();
            reader
                .read_to_end(&mut body)
                .map_err(|error| format!("read response body: {error}"))?;
            body
        };
        return Ok(Response {
            status,
            headers,
            body,
        });
    }
}

fn read_chunked(reader: &mut impl BufRead) -> Result<Vec<u8>, String> {
    let mut body = Vec::new();
    loop {
        let line = read_line(reader)?.ok_or("connection closed inside chunked body")?;
        let line = String::from_utf8_lossy(&line);
        let size = line.split(';').next().unwrap_or_default().trim();
        let size =
            usize::from_str_radix(size, 16).map_err(|_| format!("invalid chunk size: {line:?}"))?;
        if size == 0 {
            // Trailers end at the first empty line.
            while read_line(reader)?.is_some_and(|line| !line.is_empty()) {}
            return Ok(body);
        }
        let start = body.len();
        body.resize(start + size, 0);
        reader
            .read_exact(&mut body[start..])
            .map_err(|error| format!("read chunk: {error}"))?;
        read_line(reader)?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_match_node_http_framing_and_keep_the_target_verbatim() {
        let headers = vec![("authorization".to_owned(), "Bearer k".to_owned())];
        let (bytes, _) = encode(
            8080,
            &Request {
                method: "put",
                path: "/raw/a/%2e%2e%2f..//x",
                headers: &headers,
                body: None,
            },
        )
        .unwrap();
        assert_eq!(
            String::from_utf8(bytes).unwrap(),
            "PUT /raw/a/%2e%2e%2f..//x HTTP/1.1\r\nauthorization: Bearer k\r\nHost: 127.0.0.1:8080\r\nConnection: keep-alive\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\n"
        );
        let (bytes, _) = encode(
            1,
            &Request {
                method: "DELETE",
                path: "/x",
                headers: &[],
                body: None,
            },
        )
        .unwrap();
        assert_eq!(
            String::from_utf8(bytes).unwrap(),
            "DELETE /x HTTP/1.1\r\nHost: 127.0.0.1:1\r\nConnection: keep-alive\r\n\r\n"
        );
        assert!(
            encode(
                1,
                &Request {
                    method: "GET",
                    path: "/a b",
                    headers: &[],
                    body: None
                }
            )
            .is_err()
        );
    }

    #[test]
    fn responses_fold_duplicate_headers_like_node_http() {
        let raw = b"HTTP/1.1 200 OK\r\nVary: a\r\nvary: b\r\nSet-Cookie: x=1\r\nset-cookie: y=2\r\nContent-Type: text/plain\r\ncontent-type: text/html\r\nX-Latin: \xe9 \r\nTransfer-Encoding: chunked\r\n\r\n3;ext=1\r\nabc\r\n2\r\nde\r\n0\r\nTrailer: t\r\n\r\n";
        let response = read_response(&mut &raw[..], false).unwrap();
        assert_eq!(response.status, 200);
        assert_eq!(response.body, b"abcde");
        let flat: Vec<(String, String)> = crate::comparators::flat_headers(&response.headers);
        assert_eq!(
            flat,
            vec![
                ("vary".to_owned(), "a, b".to_owned()),
                ("set-cookie".to_owned(), "x=1, y=2".to_owned()),
                ("content-type".to_owned(), "text/plain".to_owned()),
                ("x-latin".to_owned(), "\u{e9}".to_owned()),
                ("transfer-encoding".to_owned(), "chunked".to_owned()),
            ]
        );
    }
}
