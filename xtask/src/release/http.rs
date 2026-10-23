//! A minimal HTTP/1.1 client for the loopback candidate server.
//!
//! The fixture preflight talks only to `http://127.0.0.1:<port>`, so a blocking request over
//! `std::net::TcpStream` with `Connection: close` is enough. It reads `Content-Length` and
//! chunked bodies and skips interim `1xx` responses.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const IO_TIMEOUT: Duration = Duration::from_secs(60);

/// The loopback origin of one candidate server.
#[derive(Clone, Copy, Debug)]
pub struct Origin {
    pub port: u16,
}

impl std::fmt::Display for Origin {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "http://127.0.0.1:{}", self.port)
    }
}

pub struct Response {
    pub status: u16,
    pub body: Vec<u8>,
}

impl Response {
    /// `response.ok`.
    pub fn ok(&self) -> bool {
        (200..300).contains(&self.status)
    }

    /// `response.text()`: UTF-8 with a leading byte order mark removed.
    pub fn text(&self) -> String {
        let body = self
            .body
            .strip_prefix(b"\xEF\xBB\xBF".as_slice())
            .unwrap_or(&self.body);
        String::from_utf8_lossy(body).into_owned()
    }
}

/// `GET <path>`.
pub fn get(origin: Origin, path: &str) -> Result<Response, String> {
    request(origin, "GET", path, &[], None)
}

/// `POST <path>` with a JSON body.
pub fn post_json(
    origin: Origin,
    path: &str,
    headers: &[(&str, &str)],
    body: &str,
) -> Result<Response, String> {
    request(origin, "POST", path, headers, Some(body))
}

fn request(
    origin: Origin,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: Option<&str>,
) -> Result<Response, String> {
    let fail = |error: std::io::Error| format!("TypeError: fetch failed ({origin}{path}: {error})");
    let address = SocketAddr::from(([127, 0, 0, 1], origin.port));
    let mut stream = TcpStream::connect_timeout(&address, CONNECT_TIMEOUT).map_err(fail)?;
    stream.set_read_timeout(Some(IO_TIMEOUT)).map_err(fail)?;
    stream.set_write_timeout(Some(IO_TIMEOUT)).map_err(fail)?;
    let mut head = format!(
        "{method} {path} HTTP/1.1\r\nhost: 127.0.0.1:{}\r\nconnection: close\r\naccept: */*\r\n\
         user-agent: artifact-mcp-xtask\r\n",
        origin.port
    );
    for (name, value) in headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    if let Some(body) = body {
        head.push_str(&format!("content-length: {}\r\n", body.len()));
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes()).map_err(fail)?;
    if let Some(body) = body {
        stream.write_all(body.as_bytes()).map_err(fail)?;
    }
    stream.flush().map_err(fail)?;
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).map_err(fail)?;
    parse_response(&raw)
        .map_err(|error| format!("TypeError: fetch failed ({origin}{path}: {error})"))
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn parse_response(mut raw: &[u8]) -> Result<Response, String> {
    loop {
        let end = find(raw, b"\r\n\r\n").ok_or("incomplete response head")?;
        let head = std::str::from_utf8(&raw[..end]).map_err(|_| "response head is not UTF-8")?;
        let rest = &raw[end + 4..];
        let mut lines = head.split("\r\n");
        let status_line = lines.next().unwrap_or_default();
        let status = status_line
            .split(' ')
            .nth(1)
            .and_then(|code| code.parse::<u16>().ok())
            .ok_or_else(|| format!("malformed status line {status_line:?}"))?;
        if (100..200).contains(&status) && status != 101 {
            raw = rest;
            continue;
        }
        let mut chunked = false;
        let mut length = None;
        for line in lines {
            let Some((name, value)) = line.split_once(':') else {
                continue;
            };
            let value = value.trim();
            if name.eq_ignore_ascii_case("transfer-encoding") {
                chunked = value.to_ascii_lowercase().contains("chunked");
            } else if name.eq_ignore_ascii_case("content-length") {
                length = Some(
                    value
                        .parse::<usize>()
                        .map_err(|_| format!("invalid content-length {value:?}"))?,
                );
            }
        }
        let body = if chunked {
            dechunk(rest)?
        } else if let Some(length) = length {
            rest.get(..length)
                .ok_or("response body is shorter than content-length")?
                .to_vec()
        } else {
            rest.to_vec()
        };
        return Ok(Response { status, body });
    }
}

fn dechunk(mut rest: &[u8]) -> Result<Vec<u8>, String> {
    let mut body = Vec::new();
    loop {
        let end = find(rest, b"\r\n").ok_or("incomplete chunk size")?;
        let size_line = std::str::from_utf8(&rest[..end]).map_err(|_| "chunk size is not UTF-8")?;
        let size_text = size_line.split(';').next().unwrap_or_default().trim();
        let size = usize::from_str_radix(size_text, 16)
            .map_err(|_| format!("invalid chunk size {size_text:?}"))?;
        rest = &rest[end + 2..];
        if size == 0 {
            return Ok(body);
        }
        let chunk = rest.get(..size).ok_or("truncated chunk")?;
        body.extend_from_slice(chunk);
        rest = rest
            .get(size..)
            .and_then(|after| after.strip_prefix(b"\r\n".as_slice()))
            .ok_or("chunk is not terminated")?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_length_chunked_and_interim_responses() {
        let response =
            parse_response(b"HTTP/1.1 200 OK\r\ncontent-length: 5\r\n\r\nhello trailing").unwrap();
        assert_eq!(
            (response.status, response.body.as_slice()),
            (200, b"hello".as_slice())
        );
        let response = parse_response(
            b"HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 404 Not Found\r\nTransfer-Encoding: chunked\r\n\r\n\
              3;x=y\r\nabc\r\n2\r\nde\r\n0\r\n\r\n",
        )
        .unwrap();
        assert_eq!(
            (response.status, response.body.as_slice()),
            (404, b"abcde".as_slice())
        );
        assert!(!response.ok());
        let response = parse_response(b"HTTP/1.1 204 No Content\r\n\r\n").unwrap();
        assert!(response.ok() && response.body.is_empty());
        assert!(parse_response(b"HTTP/1.1 200 OK\r\ncontent-length: 9\r\n\r\nshort").is_err());
        let response = Response {
            status: 200,
            body: b"\xEF\xBB\xBF{}".to_vec(),
        };
        assert_eq!(response.text(), "{}");
    }
}
