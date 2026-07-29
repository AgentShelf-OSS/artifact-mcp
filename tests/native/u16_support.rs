//! U16 test support: an in-process stand-in for the Node/Chromium preview sidecar.
//!
//! The real sidecar (`preview-renderer/server.js`) needs Chromium and a compose profile; no test
//! in this suite may depend on either. [`StubRenderer`] is a blocking `std::net` HTTP/1.1 server
//! on an ephemeral loopback port that speaks the exact sidecar contract the Rust client codes
//! against, and can also misbehave in every way the client must survive:
//!
//! * scripted per-request replies, with a default for anything past the script;
//! * arbitrary status, `content-type`, `content-length` (including one that lies), chunked bodies
//!   with no length at all, redirects, and accept-then-never-answer;
//! * a gate that holds each request until the test releases it, which is what makes queue
//!   ordering and coalescing assertions deterministic rather than timing-dependent;
//! * a record of every request the client actually sent, in arrival order.
//!
//! `std::net` on a dedicated thread rather than tokio: the crate's tokio feature set has no
//! `io-util`, and blocking sockets keep the stub's own behaviour trivially predictable.

use std::collections::VecDeque;
use std::io::{Read as _, Write as _};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use artifact_mcp::config::PreviewConfig;
use artifact_mcp::integrations::thumbnails::PNG_SIGNATURE;

/// A syntactically valid PNG of `total` bytes: the 8-byte signature plus filler.
///
/// `valid_png` only inspects the signature and the length, so filler is sufficient and lets a
/// test choose an exact size — including one byte over a cap.
#[must_use]
pub fn png_of(total: usize) -> Vec<u8> {
    let mut png = PNG_SIGNATURE.to_vec();
    png.resize(total.max(PNG_SIGNATURE.len()), 0x5a);
    png
}

/// The canonical small PNG used wherever the exact bytes do not matter.
#[must_use]
pub fn sample_png() -> Vec<u8> {
    png_of(64)
}

/// What the stub answers with.
#[derive(Clone, Debug)]
pub enum StubReply {
    /// `200 image/png` with a truthful `content-length`. The sidecar's success shape.
    Png(Vec<u8>),
    /// `200 image/png` with a `content-length` the test chooses, which may exceed the body.
    Declared { declared: u64, body: Vec<u8> },
    /// `200` with `transfer-encoding: chunked` and no `content-length` at all.
    Chunked {
        content_type: String,
        body: Vec<u8>,
        chunk: usize,
    },
    /// Any other status/content-type/body — `503 renderer busy`, `500 render failed`, …
    Status {
        status: u16,
        content_type: String,
        body: Vec<u8>,
    },
    /// `302` with a `location`. The client must never follow it.
    Redirect(String),
    /// Read the request and then never answer. Forces the client-side timeout.
    Hang,
}

impl StubReply {
    /// `500 {"error":"render failed"}` — [preview-renderer/server.js:120]
    #[must_use]
    pub fn render_failed() -> Self {
        Self::Status {
            status: 500,
            content_type: "application/json; charset=utf-8".to_owned(),
            body: br#"{"error":"render failed"}"#.to_vec(),
        }
    }

    /// `503 {"error":"renderer busy"}` — [preview-renderer/server.js:104]
    #[must_use]
    pub fn renderer_busy() -> Self {
        Self::Status {
            status: 503,
            content_type: "application/json; charset=utf-8".to_owned(),
            body: br#"{"error":"renderer busy"}"#.to_vec(),
        }
    }
}

/// One request the client sent to the stub.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StubRequest {
    pub method: String,
    pub target: String,
    pub content_type: String,
    pub html: String,
    pub width: u64,
    pub height: u64,
}

/// A counting gate: handlers block until the test grants permits.
#[derive(Debug, Default)]
struct Gate {
    permits: Mutex<usize>,
    ready: Condvar,
}

impl Gate {
    fn acquire(&self) {
        let mut permits = self
            .permits
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while *permits == 0 {
            permits = self
                .ready
                .wait(permits)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
        *permits -= 1;
    }

    fn release(&self, count: usize) {
        let mut permits = self
            .permits
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *permits += count;
        self.ready.notify_all();
    }
}

#[derive(Debug)]
struct StubState {
    script: Mutex<VecDeque<StubReply>>,
    fallback: Mutex<StubReply>,
    requests: Mutex<Vec<StubRequest>>,
    gated: AtomicBool,
    gate: Gate,
    stop: AtomicBool,
}

/// An HTTP stand-in for the preview sidecar, listening on `127.0.0.1:<ephemeral>`.
#[derive(Debug)]
pub struct StubRenderer {
    address: SocketAddr,
    state: Arc<StubState>,
}

impl StubRenderer {
    /// Start a stub that answers every request with `reply`.
    #[must_use]
    pub fn start(reply: StubReply) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind stub renderer");
        let address = listener.local_addr().expect("stub renderer address");
        let state = Arc::new(StubState {
            script: Mutex::new(VecDeque::new()),
            fallback: Mutex::new(reply),
            requests: Mutex::new(Vec::new()),
            gated: AtomicBool::new(false),
            gate: Gate::default(),
            stop: AtomicBool::new(false),
        });

        let accepting = Arc::clone(&state);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                if accepting.stop.load(Ordering::SeqCst) {
                    return;
                }
                let Ok(stream) = stream else { continue };
                let connection = Arc::clone(&accepting);
                std::thread::spawn(move || serve(&connection, stream));
            }
        });

        Self { address, state }
    }

    /// Start a stub that always renders successfully.
    #[must_use]
    pub fn rendering(png: Vec<u8>) -> Self {
        Self::start(StubReply::Png(png))
    }

    /// Queue a reply for the next request, ahead of the fallback.
    pub fn push_reply(&self, reply: StubReply) -> &Self {
        self.state
            .script
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push_back(reply);
        self
    }

    /// `http://127.0.0.1:<port>/render` — the endpoint U02 derives from `PREVIEW_RENDERER_URL`.
    #[must_use]
    pub fn endpoint(&self) -> String {
        format!("http://{}/render", self.address)
    }

    /// A configuration pointed at this stub.
    #[must_use]
    pub fn config(&self) -> PreviewConfig {
        PreviewConfig {
            renderer_endpoint: Some(self.endpoint()),
            ..PreviewConfig::default()
        }
    }

    /// Hold every request until [`StubRenderer::release`] grants a permit.
    pub fn gate(&self) -> &Self {
        self.state.gated.store(true, Ordering::SeqCst);
        self
    }

    /// Let `count` more held requests answer.
    pub fn release(&self, count: usize) {
        self.state.gate.release(count);
    }

    /// Every request received, in arrival order.
    #[must_use]
    pub fn requests(&self) -> Vec<StubRequest> {
        self.state
            .requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// How many requests the client has actually sent.
    #[must_use]
    pub fn request_count(&self) -> usize {
        self.state
            .requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
    }

    /// The `html` field of each request, in arrival order — the execution order of a queue.
    #[must_use]
    pub fn received_html(&self) -> Vec<String> {
        self.requests()
            .into_iter()
            .map(|request| request.html)
            .collect()
    }

    /// Block until at least `count` requests have arrived. Panics rather than hanging a suite.
    pub async fn wait_for_requests(&self, count: usize) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while self.request_count() < count {
            assert!(
                Instant::now() < deadline,
                "stub renderer saw {} of {count} expected request(s)",
                self.request_count()
            );
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    }
}

impl Drop for StubRenderer {
    fn drop(&mut self) {
        self.state.stop.store(true, Ordering::SeqCst);
        // Release anything still parked so its thread can exit, then poke `accept`.
        self.state.gate.release(1024);
        let _ = TcpStream::connect(self.address);
    }
}

/// Read one request, record it, then answer according to the script.
fn serve(state: &Arc<StubState>, mut stream: TcpStream) {
    let Some((head, body)) = read_request(&mut stream) else {
        return;
    };
    state
        .requests
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .push(parse_request(&head, &body));

    if state.gated.load(Ordering::SeqCst) {
        state.gate.acquire();
        if state.stop.load(Ordering::SeqCst) {
            return;
        }
    }

    let reply = {
        let mut script = state
            .script
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        script.pop_front()
    }
    .unwrap_or_else(|| {
        state
            .fallback
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    });

    write_reply(&mut stream, &reply);
    let _ = stream.shutdown(Shutdown::Both);
}

/// Read headers, then exactly `content-length` body bytes.
fn read_request(stream: &mut TcpStream) -> Option<(String, Vec<u8>)> {
    let mut buffer = Vec::new();
    let mut chunk = [0_u8; 4096];
    let header_end = loop {
        if let Some(position) = find(&buffer, b"\r\n\r\n") {
            break position;
        }
        let read = stream.read(&mut chunk).ok()?;
        if read == 0 {
            return None;
        }
        buffer.extend_from_slice(&chunk[..read]);
    };

    let head = String::from_utf8_lossy(&buffer[..header_end]).into_owned();
    let length = header_value(&head, "content-length")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(0);

    let body_start = header_end + 4;
    while buffer.len() < body_start + length {
        let read = stream.read(&mut chunk).ok()?;
        if read == 0 {
            break;
        }
        buffer.extend_from_slice(&chunk[..read]);
    }
    let end = (body_start + length).min(buffer.len());
    Some((head, buffer[body_start..end].to_vec()))
}

fn parse_request(head: &str, body: &[u8]) -> StubRequest {
    let mut parts = head.lines().next().unwrap_or_default().split_whitespace();
    let method = parts.next().unwrap_or_default().to_owned();
    let target = parts.next().unwrap_or_default().to_owned();
    let json: serde_json::Value = serde_json::from_slice(body).unwrap_or(serde_json::Value::Null);
    StubRequest {
        method,
        target,
        content_type: header_value(head, "content-type").unwrap_or_default(),
        html: json
            .get("html")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        width: json
            .get("width")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or_default(),
        height: json
            .get("height")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or_default(),
    }
}

fn header_value(head: &str, name: &str) -> Option<String> {
    head.lines()
        .skip(1)
        .filter_map(|line| line.split_once(':'))
        .find(|(key, _)| key.trim().eq_ignore_ascii_case(name))
        .map(|(_, value)| value.trim().to_owned())
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// Every write is best effort: a client that aborts mid-body (the streaming cap) closes the
/// socket, and the stub must not panic when that happens.
fn write_reply(stream: &mut TcpStream, reply: &StubReply) {
    match reply {
        StubReply::Png(body) => {
            let _ = write_headers(stream, 200, "image/png", Some(body.len() as u64), &[]);
            let _ = stream.write_all(body);
        }
        StubReply::Declared { declared, body } => {
            let _ = write_headers(stream, 200, "image/png", Some(*declared), &[]);
            let _ = stream.write_all(body);
        }
        StubReply::Status {
            status,
            content_type,
            body,
        } => {
            let _ = write_headers(stream, *status, content_type, Some(body.len() as u64), &[]);
            let _ = stream.write_all(body);
        }
        StubReply::Redirect(location) => {
            let _ = write_headers(
                stream,
                302,
                "text/plain",
                Some(0),
                &[("location", location.as_str())],
            );
        }
        StubReply::Chunked {
            content_type,
            body,
            chunk,
        } => {
            let _ = write_headers(
                stream,
                200,
                content_type,
                None,
                &[("transfer-encoding", "chunked")],
            );
            for piece in body.chunks((*chunk).max(1)) {
                if write!(stream, "{:x}\r\n", piece.len()).is_err()
                    || stream.write_all(piece).is_err()
                    || stream.write_all(b"\r\n").is_err()
                    || stream.flush().is_err()
                {
                    return;
                }
            }
            let _ = stream.write_all(b"0\r\n\r\n");
        }
        StubReply::Hang => {
            // Hold the connection open without answering; the client must time out.
            std::thread::sleep(Duration::from_secs(30));
        }
    }
    let _ = stream.flush();
}

fn write_headers(
    stream: &mut TcpStream,
    status: u16,
    content_type: &str,
    content_length: Option<u64>,
    extra: &[(&str, &str)],
) -> std::io::Result<()> {
    let mut head = format!("HTTP/1.1 {status} STUB\r\ncontent-type: {content_type}\r\n");
    if let Some(length) = content_length {
        head.push_str(&format!("content-length: {length}\r\n"));
    }
    for (key, value) in extra {
        head.push_str(&format!("{key}: {value}\r\n"));
    }
    head.push_str("cache-control: no-store\r\nconnection: close\r\n\r\n");
    stream.write_all(head.as_bytes())
}
