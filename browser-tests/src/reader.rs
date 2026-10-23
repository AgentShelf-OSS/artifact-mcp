// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Neil Blackman
//! The reader regression suite behind `cargo test --test reader`: what
//! `playwright/reader-fixture.cjs` and `playwright/reader-runner.mjs` provided.
//!
//! * **Runner** ([`main`]): the cases run one at a time, each with a fresh browser
//!   (`chrome-headless-shell` with `--autoplay-policy=no-user-gesture-required` and
//!   [`Network::LoopbackOnly`]), a 90 s timeout per case and 10 minutes per run. A case returns
//!   its report line (the old programs' `PASS: ...` or JSON summary), printed under its result.
//! * **Fixture** ([`ReaderCtx::fixture`], the old `createReaderFixture`): a stub speech worker in
//!   this process ([`Worker`]: WAV, PCM and timed-PCM bodies, latency, request log and
//!   concurrency counter), and the release binary ([`crate::server`]) with text to speech on and
//!   every provider (`TTS`, `POCKET`, `QWEN`, `RAVEN`, `MOSS`) pointed at the worker. Only the
//!   worker's HTTP boundary is a stub; the application's speech routes are real.
//! * **Stub server** ([`StubServer`]): a minimal HTTP/1.1 server on a free loopback port, for the
//!   worker and for pages a case serves itself.

use std::any::Any;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::ExitCode;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::{JoinHandle, JoinSet};

use crate::connection::lock;
use crate::http::{Body, Client};
use crate::network::find_header;
use crate::server::{Server, ServerOptions};
use crate::{Browser, BrowserOptions, Error, Network, Page};

/// The viewer and administrator identity of the reader fixture (`ADMIN_EMAILS`).
pub const VIEWER_EMAIL: &str = "reader-fixture@example.test";

/// The title [`ReaderFixture::publish`] callers use when the old fixture's default applied.
pub const DEFAULT_TITLE: &str = "Reader fixture";

/// Per-case timeout (`caseTimeoutMs`).
pub const CASE_TIMEOUT: Duration = Duration::from_secs(90);

/// Whole-run timeout (`runTimeoutMs`).
pub const RUN_TIMEOUT: Duration = Duration::from_secs(600);

/// Prefix of the fixtures' temporary directories under `$TMPDIR`.
pub const READER_DIR_PREFIX: &str = "artifact-reader-";

/// The Chromium switch the old suite launched with, so audio plays without a gesture.
pub const AUTOPLAY_SWITCH: &str = "--autoplay-policy=no-user-gesture-required";

/// The stub worker's PCM sample rate.
const SAMPLE_RATE: usize = 24_000;

// ---------------------------------------------------------------------------------------------
// Stub HTTP server
// ---------------------------------------------------------------------------------------------

/// A request the stub server received.
#[derive(Debug, Clone)]
pub struct StubRequest {
    pub method: String,
    /// The request target: path and query.
    pub target: String,
    /// Header names in lower case.
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl StubRequest {
    /// The target without its query.
    pub fn path(&self) -> &str {
        self.target.split(['?', '#']).next().unwrap_or("")
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        find_header(&self.headers, name)
    }
}

/// A stub server's answer.
pub struct StubResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    /// Dropped once the response is written (or the client went away).
    keep: Option<Box<dyn Any + Send>>,
}

impl StubResponse {
    pub fn new(status: u16, content_type: &str, body: impl Into<Vec<u8>>) -> Self {
        Self {
            status,
            headers: vec![("content-type".into(), content_type.into())],
            body: body.into(),
            keep: None,
        }
    }

    /// A response with no body.
    pub fn empty(status: u16) -> Self {
        Self {
            status,
            headers: Vec::new(),
            body: Vec::new(),
            keep: None,
        }
    }

    /// Keep `value` alive until the response is sent, as Node's `response.once("close")` runs
    /// after the body is written.
    pub fn keep_until_sent(mut self, value: impl Any + Send) -> Self {
        self.keep = Some(Box::new(value));
        self
    }
}

type StubHandler =
    Arc<dyn Fn(StubRequest) -> Pin<Box<dyn Future<Output = StubResponse> + Send>> + Send + Sync>;

/// A minimal HTTP/1.1 server on `127.0.0.1` and a free port: one request per connection
/// (`Connection: close`), `Content-Length` or chunked request bodies. A handler still running
/// when its client disconnects is dropped. Dropping the server stops it and every connection.
pub struct StubServer {
    port: u16,
    accept: JoinHandle<()>,
}

impl StubServer {
    /// Listen and answer every request with `handler`.
    pub async fn start<F, Fut>(handler: F) -> Result<Self, Error>
    where
        F: Fn(StubRequest) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = StubResponse> + Send + 'static,
    {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .map_err(|error| Error::Launch(format!("stub server: {error}")))?;
        let port = listener
            .local_addr()
            .map_err(|error| Error::Launch(format!("stub server: {error}")))?
            .port();
        let handler: StubHandler = Arc::new(move |request| Box::pin(handler(request)));
        let accept = tokio::spawn(accept_loop(listener, handler));
        Ok(Self { port, accept })
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// `http://127.0.0.1:<port>`.
    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }
}

impl Drop for StubServer {
    fn drop(&mut self) {
        // The accept task owns the connections' `JoinSet`, which aborts them when dropped.
        self.accept.abort();
    }
}

async fn accept_loop(listener: TcpListener, handler: StubHandler) {
    let mut connections = JoinSet::new();
    loop {
        tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => {
                    connections.spawn(serve(stream, Arc::clone(&handler)));
                }
                Err(_) => tokio::time::sleep(Duration::from_millis(10)).await,
            },
            Some(_) = connections.join_next(), if !connections.is_empty() => {}
        }
    }
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        _ => "Status",
    }
}

async fn serve(mut stream: TcpStream, handler: StubHandler) {
    let Some(request) = read_request(&mut stream).await else {
        return;
    };
    let response = tokio::select! {
        response = handler(request) => response,
        () = closed(&stream) => return,
    };
    let mut head = format!(
        "HTTP/1.1 {} {}\r\n",
        response.status,
        reason(response.status)
    );
    for (name, value) in &response.headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str(&format!(
        "content-length: {}\r\nconnection: close\r\n\r\n",
        response.body.len()
    ));
    if stream.write_all(head.as_bytes()).await.is_ok() {
        let _ = stream.write_all(&response.body).await;
        let _ = stream.flush().await;
    }
    let _ = stream.shutdown().await;
    drop(response);
}

/// Resolves when the client closes its side (or the socket fails).
async fn closed(stream: &TcpStream) {
    let mut buffer = [0_u8; 512];
    loop {
        if stream.readable().await.is_err() {
            return;
        }
        match stream.try_read(&mut buffer) {
            Ok(0) => return,
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(_) => return,
        }
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// Read one request: head, then a `Content-Length` or chunked body. `None` on EOF or garbage.
async fn read_request(stream: &mut TcpStream) -> Option<StubRequest> {
    const LIMIT: usize = 16 * 1024 * 1024;
    let mut data = Vec::new();
    let mut chunk = [0_u8; 16 * 1024];
    let head_end = loop {
        if let Some(end) = find(&data, b"\r\n\r\n") {
            break end;
        }
        let read = stream.read(&mut chunk).await.ok()?;
        if read == 0 || data.len() > LIMIT {
            return None;
        }
        data.extend_from_slice(&chunk[..read]);
    };
    let head = String::from_utf8_lossy(&data[..head_end]).into_owned();
    let mut lines = head.split("\r\n");
    let mut request_line = lines.next()?.split(' ');
    let method = request_line.next()?.to_owned();
    let target = request_line.next()?.to_owned();
    let headers: Vec<(String, String)> = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_owned()))
        .collect();
    let mut rest = data[head_end + 4..].to_vec();
    let chunked = find_header(&headers, "transfer-encoding")
        .is_some_and(|value| value.to_ascii_lowercase().contains("chunked"));
    let body = if chunked {
        loop {
            if let Some(body) = dechunk(&rest) {
                break body;
            }
            let read = stream.read(&mut chunk).await.ok()?;
            if read == 0 || rest.len() > LIMIT {
                return None;
            }
            rest.extend_from_slice(&chunk[..read]);
        }
    } else {
        let length: usize = find_header(&headers, "content-length")
            .and_then(|value| value.parse().ok())
            .unwrap_or(0);
        if length > LIMIT {
            return None;
        }
        while rest.len() < length {
            let read = stream.read(&mut chunk).await.ok()?;
            if read == 0 {
                return None;
            }
            rest.extend_from_slice(&chunk[..read]);
        }
        rest.truncate(length);
        rest
    };
    Some(StubRequest {
        method,
        target,
        headers,
        body,
    })
}

/// A complete chunked body, or `None` while more is needed.
fn dechunk(data: &[u8]) -> Option<Vec<u8>> {
    let mut body = Vec::new();
    let mut at = 0;
    loop {
        let line_end = at + find(&data[at..], b"\r\n")?;
        let size_text = String::from_utf8_lossy(&data[at..line_end]);
        let size = usize::from_str_radix(size_text.split(';').next()?.trim(), 16).ok()?;
        at = line_end + 2;
        if size == 0 {
            return Some(body);
        }
        if data.len() < at + size + 2 {
            return None;
        }
        body.extend_from_slice(&data[at..at + size]);
        at += size + 2;
    }
}

// ---------------------------------------------------------------------------------------------
// Stub speech worker
// ---------------------------------------------------------------------------------------------

/// The stub worker's starting audio options (`createReaderFixture({ audio })`).
#[derive(Debug, Clone, Copy)]
pub struct Audio {
    /// How long each synthesized body plays.
    pub duration_seconds: f64,
    /// How long the worker waits before it answers.
    pub latency_ms: u64,
}

impl Audio {
    pub fn seconds(duration_seconds: f64) -> Self {
        Self {
            duration_seconds,
            latency_ms: 0,
        }
    }

    pub fn latency(mut self, latency_ms: u64) -> Self {
        self.latency_ms = latency_ms;
        self
    }
}

/// A speech request the worker received.
#[derive(Debug, Clone)]
pub struct WorkerRequest {
    /// `/speech`, `/speech/stream` or `/speech/stream-timed`.
    pub path: String,
    pub body: Value,
    /// When its body had arrived, in milliseconds since the epoch (`Date.now()`).
    pub at: u128,
}

type LatencyFor = Arc<dyn Fn(&Value) -> Option<u64> + Send + Sync>;

struct WorkerState {
    duration_seconds: f64,
    latency_ms: u64,
    latency_for: Option<LatencyFor>,
    requests: Vec<WorkerRequest>,
    /// Speech requests being served now, and the most at once.
    active: usize,
    max: usize,
}

/// The stub speech worker (the old `createWorker`): `POST /speech` answers a WAV,
/// `/speech/stream` length-prefixed PCM frames and `/speech/stream-timed` typed frames with word
/// timings, after the configured latency. Requests need `Authorization: Bearer <token>`. Cheap
/// to clone; clones share the state.
#[derive(Clone)]
pub struct Worker {
    token: String,
    state: Arc<Mutex<WorkerState>>,
}

/// Counts one active speech request until dropped (Node's `response.once("close")`).
struct Load(Arc<Mutex<WorkerState>>);

impl Drop for Load {
    fn drop(&mut self) {
        let mut state = lock(&self.0);
        state.active = state.active.saturating_sub(1);
    }
}

impl Worker {
    fn new(audio: Audio, token: String) -> Self {
        Self {
            token,
            state: Arc::new(Mutex::new(WorkerState {
                duration_seconds: audio.duration_seconds,
                latency_ms: audio.latency_ms,
                latency_for: None,
                requests: Vec::new(),
                active: 0,
                max: 0,
            })),
        }
    }

    /// `worker.configure({ durationSeconds })`.
    pub fn set_duration(&self, seconds: f64) {
        lock(&self.state).duration_seconds = seconds;
    }

    /// `worker.configure({ latencyMs })`.
    pub fn set_latency(&self, latency_ms: u64) {
        lock(&self.state).latency_ms = latency_ms;
    }

    /// `worker.configure({ latencyFor })`: a per-request latency from the request body; `None`
    /// falls back to the configured latency.
    pub fn set_latency_for(&self, latency: impl Fn(&Value) -> Option<u64> + Send + Sync + 'static) {
        lock(&self.state).latency_for = Some(Arc::new(latency));
    }

    /// `worker.requests`: every speech request since the last [`Worker::reset`].
    pub fn requests(&self) -> Vec<WorkerRequest> {
        lock(&self.state).requests.clone()
    }

    pub fn request_count(&self) -> usize {
        lock(&self.state).requests.len()
    }

    /// `worker.load.max`: the most speech requests served at once.
    pub fn max_load(&self) -> usize {
        lock(&self.state).max
    }

    /// `worker.reset()`: forget the requests; the maximum restarts from the current load.
    pub fn reset(&self) {
        let mut state = lock(&self.state);
        state.requests.clear();
        state.max = state.active;
    }

    async fn answer(self, request: StubRequest) -> StubResponse {
        if request.header("authorization") != Some(format!("Bearer {}", self.token).as_str()) {
            return StubResponse::empty(401);
        }
        let path = request.path().to_owned();
        let speech = matches!(
            path.as_str(),
            "/speech" | "/speech/stream" | "/speech/stream-timed"
        );
        if request.method != "POST" || !speech {
            return StubResponse::empty(404);
        }
        let Ok(body) = serde_json::from_slice::<Value>(&request.body) else {
            return StubResponse::empty(400);
        };
        let (delay, duration) = {
            let mut state = lock(&self.state);
            state.requests.push(WorkerRequest {
                path: path.clone(),
                body: body.clone(),
                at: now_millis(),
            });
            state.active += 1;
            state.max = state.max.max(state.active);
            let custom = state
                .latency_for
                .as_ref()
                .and_then(|latency| latency(&body));
            (custom.unwrap_or(state.latency_ms), state.duration_seconds)
        };
        let load = Load(Arc::clone(&self.state));
        tokio::time::sleep(Duration::from_millis(delay)).await;
        let text = body.get("text").and_then(Value::as_str).unwrap_or("");
        let response = if path == "/speech" {
            StubResponse::new(200, "audio/wav", wav_bytes(text, duration))
        } else if path.ends_with("-timed") {
            StubResponse::new(
                200,
                "application/vnd.artifact.pcm-timed;v=1",
                timed_body(text, duration),
            )
        } else {
            StubResponse::new(200, "application/vnd.artifact.pcm", pcm_body(duration))
        };
        response.keep_until_sent(load)
    }
}

/// Milliseconds since the epoch (`Date.now()`).
pub fn now_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis())
        .unwrap_or(0)
}

/// `Math.max(low, Math.min(high, Number(seconds) || 1))`.
fn clamp_seconds(seconds: f64, low: f64, high: f64) -> f64 {
    let seconds = if seconds.is_nan() || seconds == 0.0 {
        1.0
    } else {
        seconds
    };
    seconds.min(high).max(low)
}

/// A mono 16-bit 24 kHz WAV of a sine whose phase depends on `text`.
pub fn wav_bytes(text: &str, duration_seconds: f64) -> Vec<u8> {
    let seconds = clamp_seconds(duration_seconds, 0.05, 30.0);
    let samples = (SAMPLE_RATE as f64 * seconds).floor() as usize;
    let length = 44 + samples * 2;
    let mut bytes = Vec::with_capacity(length);
    let u32_le = |value: usize| u32::try_from(value).unwrap_or(u32::MAX).to_le_bytes();
    bytes.extend_from_slice(b"RIFF");
    bytes.extend_from_slice(&u32_le(length - 8));
    bytes.extend_from_slice(b"WAVEfmt ");
    bytes.extend_from_slice(&16_u32.to_le_bytes());
    bytes.extend_from_slice(&1_u16.to_le_bytes());
    bytes.extend_from_slice(&1_u16.to_le_bytes());
    bytes.extend_from_slice(&u32_le(SAMPLE_RATE));
    bytes.extend_from_slice(&u32_le(SAMPLE_RATE * 2));
    bytes.extend_from_slice(&2_u16.to_le_bytes());
    bytes.extend_from_slice(&16_u16.to_le_bytes());
    bytes.extend_from_slice(b"data");
    bytes.extend_from_slice(&u32_le(samples * 2));
    let seed: u64 = text
        .chars()
        .map(|character| u64::from(u32::from(character)))
        .sum();
    for index in 0..samples {
        let value = (((index as u64 + seed) as f64 / 12.0).sin() * 1000.0).round() as i16;
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    bytes
}

/// Silent PCM for `duration_seconds`, in frames of at most 96 000 bytes with a 4-byte
/// big-endian length, then a zero-length end frame.
pub fn pcm_body(duration_seconds: f64) -> Vec<u8> {
    let seconds = clamp_seconds(duration_seconds, 0.05, 30.0);
    let mut remaining = (seconds * SAMPLE_RATE as f64).floor() as usize * 2;
    let mut body = Vec::with_capacity(remaining + remaining / 96_000 * 4 + 8);
    // One frame for a two-second prefetch fixture makes each scheduled buffer a paragraph.
    // Longer control fixtures stay below the application's 192 KiB frame limit.
    while remaining > 0 {
        let bytes = remaining.min(96_000);
        body.extend_from_slice(&u32::try_from(bytes).unwrap_or(u32::MAX).to_be_bytes());
        body.resize(body.len() + bytes, 0);
        remaining -= bytes;
    }
    body.extend_from_slice(&[0, 0, 0, 0]);
    body
}

fn timed_frame(kind: u8, payload: &[u8]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(payload.len() + 5);
    frame.extend_from_slice(
        &u32::try_from(payload.len() + 1)
            .unwrap_or(u32::MAX)
            .to_be_bytes(),
    );
    frame.push(kind);
    frame.extend_from_slice(payload);
    frame
}

/// The words of `text` (`/[\p{L}\p{N}]+/gu`).
fn words(text: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut current = String::new();
    for character in text.chars() {
        if character.is_alphanumeric() {
            current.push(character);
        } else if !current.is_empty() {
            words.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        words.push(current);
    }
    words
}

/// Timed PCM: a type-2 JSON frame per word (`{word, index, start, end}`, 0.3 s each), then
/// type-1 frames of 24 000 silent bytes for the duration, then a zero-length end frame.
pub fn timed_body(text: &str, duration_seconds: f64) -> Vec<u8> {
    let duration = if duration_seconds.is_nan() || duration_seconds == 0.0 {
        1.0
    } else {
        duration_seconds
    }
    .max(0.2);
    let mut body = Vec::new();
    for (index, word) in words(text).into_iter().enumerate() {
        let start = index as f64 * 0.3;
        let timing = json!({"word": word, "index": index, "start": start, "end": start + 0.3});
        body.extend(timed_frame(2, timing.to_string().as_bytes()));
    }
    let frames = ((duration * 48_000.0) / 24_000.0).ceil().max(1.0) as usize;
    for _ in 0..frames {
        body.extend(timed_frame(1, &[0; 24_000]));
    }
    body.extend_from_slice(&[0, 0, 0, 0]);
    body
}

// ---------------------------------------------------------------------------------------------
// The fixture: worker and server
// ---------------------------------------------------------------------------------------------

/// An artifact [`ReaderFixture::publish`] created.
#[derive(Debug, Clone)]
pub struct Published {
    pub id: String,
    /// `<base>/<id>`.
    pub url: String,
}

/// The server with speech on and its stub worker (`createReaderFixture`). Dropping it stops the
/// server and the worker and removes its directory.
pub struct ReaderFixture {
    server: Server,
    worker: Worker,
    _stub: StubServer,
    dir: PathBuf,
    api_key: String,
}

fn random_hex(bytes: usize) -> String {
    use std::io::Read;
    let mut buffer = vec![0_u8; bytes];
    let filled = std::fs::File::open("/dev/urandom")
        .and_then(|mut source| source.read_exact(&mut buffer))
        .is_ok();
    if !filled {
        for (index, byte) in buffer.iter_mut().enumerate() {
            let seed = now_millis().wrapping_mul(0x9E37_79B9_7F4A_7C15) >> (index % 8 * 8);
            *byte = (seed as u8) ^ (index as u8).wrapping_mul(31);
        }
    }
    buffer.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn fixture_dir() -> Result<PathBuf, Error> {
    for attempt in 0..100 {
        let dir = std::env::temp_dir().join(format!(
            "{READER_DIR_PREFIX}{}-{}-{attempt}",
            std::process::id(),
            now_millis()
        ));
        if std::fs::create_dir(&dir).is_ok() {
            return Ok(dir);
        }
    }
    Err(Error::Launch(
        "could not create a reader fixture directory".into(),
    ))
}

fn write_secret(path: &Path, text: &str) -> std::io::Result<()> {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    options.open(path)?.write_all(text.as_bytes())
}

impl ReaderFixture {
    /// Start the worker and the server (`createReaderFixture({ audio })`).
    pub async fn start(binary: &Path, audio: Audio) -> Result<Self, Error> {
        let dir = fixture_dir()?;
        let result = Self::start_in(binary, audio, &dir).await;
        if result.is_err() {
            let _ = std::fs::remove_dir_all(&dir);
        }
        result
    }

    async fn start_in(binary: &Path, audio: Audio, dir: &Path) -> Result<Self, Error> {
        let worker = Worker::new(audio, random_hex(24));
        let answering = worker.clone();
        let stub = StubServer::start(move |request| answering.clone().answer(request)).await?;
        let token_file = dir.join("worker-token");
        write_secret(&token_file, &format!("{}\n", worker.token))
            .map_err(|error| Error::Launch(format!("{}: {error}", token_file.display())))?;
        let api_key = random_hex(32);
        let worker_url = stub.url();
        let token_path = token_file.to_string_lossy().into_owned();
        let mut options = ServerOptions::new(binary);
        options.admin_email = VIEWER_EMAIL.to_owned();
        let mut env: Vec<(String, String)> = [
            ("REQUIRE_ACCESS_JWT", "0"),
            ("CF_ACCESS_AUD", ""),
            ("CF_ACCESS_TEAM_DOMAIN", ""),
            ("TTS_ENABLED", "1"),
            ("TTS_ARTIFACT_IDS", ""),
            // Listen place sync and the checks' own state reads share one source address.
            ("INGRESS_STATE_PER_WINDOW", "1000"),
            ("QWEN_TTS_REFERENCE_VOICE_ENABLED", "1"),
            ("QWEN_TTS_CUSTOM_VOICES_ENABLED", "1"),
            ("INGRESS_READS_PER_WINDOW", "1000"),
            ("INGRESS_MUTATIONS_PER_WINDOW", "1000"),
            ("INGRESS_MCP_PER_WINDOW", "1000"),
            ("INGRESS_UPLOADS_PER_WINDOW", "1000"),
        ]
        .into_iter()
        .map(|(name, value)| (name.to_owned(), value.to_owned()))
        .collect();
        env.push((
            "ARTIFACT_API_KEYS".into(),
            format!("reader-fixture:homelab:{api_key}"),
        ));
        for provider in ["TTS", "POCKET_TTS", "QWEN_TTS", "RAVEN_TTS", "MOSS_TTS"] {
            env.push((format!("{provider}_WORKER_URL"), worker_url.clone()));
            env.push((format!("{provider}_WORKER_TOKEN_FILE"), token_path.clone()));
        }
        if let Some(home) = std::env::var_os("HOME") {
            env.push(("HOME".into(), home.to_string_lossy().into_owned()));
        }
        options.env = env;
        options.startup_timeout = Duration::from_secs(15);
        let server = Server::start(&options).await?;
        Ok(Self {
            server,
            worker,
            _stub: stub,
            dir: dir.to_owned(),
            api_key,
        })
    }

    /// `http://127.0.0.1:<port>` of the server.
    pub fn base(&self) -> &str {
        self.server.base_url()
    }

    pub fn server(&self) -> &Server {
        &self.server
    }

    /// The stub worker.
    pub fn worker(&self) -> &Worker {
        &self.worker
    }

    /// `viewerHeaders`: the viewer identity and the portal's mutation headers.
    pub fn viewer_headers(&self) -> Vec<(String, String)> {
        vec![
            (
                "cf-access-authenticated-user-email".into(),
                VIEWER_EMAIL.into(),
            ),
            ("x-artifact-mutation".into(), "1".into()),
            ("sec-fetch-site".into(), "same-origin".into()),
        ]
    }

    /// API calls with the viewer headers.
    pub fn viewer_client(&self) -> Client {
        let headers = self.viewer_headers();
        let borrowed: Vec<(&str, &str)> = headers
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_str()))
            .collect();
        Client::new(self.base(), &borrowed)
    }

    /// Publish an artifact with the fixture's API key (`fixture.publish({ title, html })`).
    pub async fn publish(&self, title: &str, html: &str) -> Result<Published, Error> {
        let response = Client::new(self.base(), &[])
            .send(
                "POST",
                "/mcp",
                Some(Body::Json(json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": "tools/call",
                    "params": {"name": "publish_artifact", "arguments": {"title": title, "html": html}},
                }))),
                &[("authorization", &format!("Bearer {}", self.api_key))],
            )
            .await?;
        if !response.ok() {
            return Err(Error::Assertion(format!(
                "publish failed: {}",
                response.status
            )));
        }
        let body = response.json();
        let result = match body.pointer("/result/structuredContent") {
            Some(result) if result.is_object() => result.clone(),
            _ => body
                .pointer("/result/content")
                .and_then(Value::as_array)
                .and_then(|content| content.iter().find(|item| item["type"] == json!("text")))
                .and_then(|item| item["text"].as_str())
                .and_then(|text| serde_json::from_str(text).ok())
                .unwrap_or(Value::Null),
        };
        let id = result["id"].as_str().unwrap_or("").to_owned();
        if id.is_empty() {
            return Err(Error::Assertion(format!(
                "publish returned no artifact id: {body}"
            )));
        }
        Ok(Published {
            url: format!("{}/{id}", self.base()),
            id,
        })
    }

    /// `assertSpeechRoutes(id)`: the voices route lists every provider; `/speech`,
    /// `/speech/stream` and `/speech/stream-timed` answer audio of the right type through the
    /// worker; the shell carries the reader and the raw document its bridge. Then the worker's
    /// log is reset.
    pub async fn assert_speech_routes(&self, id: &str) -> Result<(), Error> {
        let client = self.viewer_client();
        let voices_response = client.get(&format!("/{id}/speech/voices")).await?;
        if !voices_response.ok() {
            return Err(Error::Assertion(format!(
                "speech voices route failed: {}",
                voices_response.status
            )));
        }
        let voices = voices_response.json();
        let list = voices["voices"].as_array().cloned().unwrap_or_default();
        if voices["enabled"] != json!(true) || list.is_empty() {
            return Err(Error::Assertion("speech voices are not enabled".into()));
        }
        let choose = |prefix: &str| {
            list.iter()
                .filter_map(|voice| voice["id"].as_str())
                .find(|voice| voice.starts_with(prefix))
                .map(str::to_owned)
                .ok_or_else(|| {
                    Error::Assertion(format!("speech provider voice {prefix} is missing"))
                })
        };
        let probes = [
            (format!("/{id}/speech"), choose("bm_")?, "audio/wav"),
            (
                format!("/{id}/speech/stream"),
                choose("qwen_")?,
                "application/vnd.artifact.pcm",
            ),
            (
                format!("/{id}/speech/stream-timed"),
                choose("pocket_")?,
                "application/vnd.artifact.pcm-timed",
            ),
        ];
        for (path, voice, expected) in probes {
            let response = client
                .send(
                    "POST",
                    &path,
                    Some(Body::Json(
                        json!({"text": "deterministic reader probe", "voice": voice}),
                    )),
                    &[],
                )
                .await?;
            if !response.ok() {
                return Err(Error::Assertion(format!(
                    "{path} failed: {}",
                    response.status
                )));
            }
            let content_type = response
                .header("content-type")
                .unwrap_or("")
                .to_ascii_lowercase();
            if !content_type.starts_with(expected) {
                return Err(Error::Assertion(format!(
                    "{path} content type {content_type}"
                )));
            }
            if response.body.len() < 8 {
                return Err(Error::Assertion(format!("{path} returned empty audio")));
            }
            if expected == "audio/wav" && !response.body.starts_with(b"RIFF") {
                return Err(Error::Assertion("invalid WAV fixture".into()));
            }
        }
        let shell = client.get(&format!("/{id}")).await?;
        if !shell.ok() || !shell.text().contains("vreader-toggle") {
            return Err(Error::Assertion("reader shell injection is missing".into()));
        }
        let raw = client.get(&format!("/raw/{id}?reader=1")).await?;
        if !raw.ok() || !raw.text().contains("artifact-reader-bridge") {
            return Err(Error::Assertion(
                "reader bridge injection is missing".into(),
            ));
        }
        self.worker.reset();
        Ok(())
    }
}

impl Drop for ReaderFixture {
    fn drop(&mut self) {
        if self
            .dir
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with(READER_DIR_PREFIX))
        {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Cases and runner
// ---------------------------------------------------------------------------------------------

/// What one case gets: its own browser, and the server binary for [`ReaderCtx::fixture`].
pub struct ReaderCtx {
    browser: Arc<Browser>,
    binary: PathBuf,
}

impl ReaderCtx {
    pub fn browser(&self) -> &Browser {
        &self.browser
    }

    /// `browser.newPage()`: a page in its own context, 1280×720, evaluating with a user gesture
    /// as Playwright does.
    pub async fn page(&self) -> Result<Page, Error> {
        let mut page = self.browser.new_page().await?;
        page.set_evaluate_user_gesture(true);
        Ok(page)
    }

    /// `browser.newContext({ viewport, extraHTTPHeaders }).newPage()`.
    ///
    /// `Sec-Fetch-*` headers are left out: Chrome 156 fails every request the page itself makes
    /// (for example `fetch`) with `net::ERR_INVALID_ARGUMENT` when DevTools adds such a forbidden
    /// header, and the browser sends its own, accurate `Sec-Fetch-Site` anyway.
    pub async fn page_with(
        &self,
        viewport: Option<(u32, u32)>,
        headers: &[(String, String)],
    ) -> Result<Page, Error> {
        let page = self.page().await?;
        if let Some((width, height)) = viewport {
            page.set_viewport(width, height, 1.0).await?;
        }
        let borrowed: Vec<(&str, &str)> = headers
            .iter()
            .filter(|(name, _)| !name.to_ascii_lowercase().starts_with("sec-fetch-"))
            .map(|(name, value)| (name.as_str(), value.as_str()))
            .collect();
        page.set_extra_http_headers(&borrowed).await?;
        Ok(page)
    }

    /// Start the server and the stub worker (`createReaderFixture({ audio })`).
    pub async fn fixture(&self, audio: Audio) -> Result<ReaderFixture, Error> {
        ReaderFixture::start(&self.binary, audio).await
    }
}

/// The future a case returns: its report line.
pub type CaseFuture = Pin<Box<dyn Future<Output = Result<String, Error>> + Send>>;

/// One reader case.
#[derive(Clone, Copy)]
pub struct Case {
    /// The old program's name, for example `reader-controls`.
    pub name: &'static str,
    /// `chromium` (the bridge alone) or `rust` (against the server).
    pub runtime: &'static str,
    pub run: fn(ReaderCtx) -> CaseFuture,
}

impl Case {
    /// `name [runtime]`, the name filters match.
    pub fn full_name(&self) -> String {
        format!("{} [{}]", self.name, self.runtime)
    }
}

/// Register `async fn(ReaderCtx) -> Result<String, Error>` as the reader case `$name`.
#[macro_export]
macro_rules! reader_case {
    ($name:expr, $runtime:expr, $function:path) => {
        $crate::reader::Case {
            name: $name,
            runtime: $runtime,
            run: |ctx| Box::pin($function(ctx)),
        }
    };
}

fn panic_text(payload: &(dyn Any + Send)) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|text| (*text).to_owned())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "panicked".to_owned())
}

/// The `main` of `tests/reader`: run the selected cases one at a time, each with a fresh
/// browser, libtest style. Arguments: name substrings, `--exact`, `--list`.
///
/// Without `CHROME_HEADLESS_SHELL` or `ARTIFACT_MCP_BIN` the run is skipped, unless `CI` is set,
/// where it fails. The binary is required even for cases that do not start the server, as the
/// old runner required it: a missing build never looks like a pass.
pub fn main(cases: &[Case], failure_dir: PathBuf) -> ExitCode {
    let mut filters = Vec::new();
    let (mut exact, mut list) = (false, false);
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--exact" => exact = true,
            "--list" => list = true,
            "--format" | "--color" | "--skip" | "-Z" | "--logfile" | "--report-time"
            | "--test-threads" => {
                let _ = args.next();
            }
            other if other.starts_with('-') => {}
            filter => filters.push(filter.to_owned()),
        }
    }
    let selected: Vec<Case> = cases
        .iter()
        .copied()
        .filter(|case| {
            let name = case.full_name();
            filters.is_empty()
                || filters.iter().any(|filter| {
                    if exact {
                        name == *filter || case.name == filter
                    } else {
                        name.contains(filter.as_str())
                    }
                })
        })
        .collect();
    if list {
        for case in &selected {
            println!("{}: test", case.full_name());
        }
        println!("\n{} tests, 0 benchmarks", selected.len());
        return ExitCode::SUCCESS;
    }
    let chrome = match std::env::var_os("CHROME_HEADLESS_SHELL") {
        Some(path) if !path.is_empty() => PathBuf::from(path),
        _ if std::env::var_os("CI").is_some() => {
            eprintln!(
                "CHROME_HEADLESS_SHELL must be set in CI; reader tests may not be skipped there"
            );
            return ExitCode::FAILURE;
        }
        _ => {
            println!("\nrunning 0 tests: CHROME_HEADLESS_SHELL is not set, reader tests skipped\n");
            return ExitCode::SUCCESS;
        }
    };
    let binary = match crate::server::binary_from_env() {
        Ok(Some(binary)) => binary,
        Ok(None) => {
            println!("\nrunning 0 tests: ARTIFACT_MCP_BIN is not set, reader tests skipped\n");
            return ExitCode::SUCCESS;
        }
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::FAILURE;
        }
    };
    let filtered = cases.len() - selected.len();
    crate::expect::set_failure_dir(failure_dir);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    runtime.block_on(run(selected, filtered, chrome, binary))
}

async fn run(cases: Vec<Case>, filtered: usize, chrome: PathBuf, binary: PathBuf) -> ExitCode {
    let started = Instant::now();
    println!(
        "\nrunning {} reader cases against {}",
        cases.len(),
        binary.display()
    );
    let mut options = BrowserOptions::new(&chrome).network(Network::LoopbackOnly);
    options.extra_args.push(AUTOPLAY_SWITCH.into());
    let mut failures: Vec<(String, String)> = Vec::new();
    let mut passed = 0;
    for case in &cases {
        let name = case.full_name();
        crate::expect::set_current_test(&name);
        crate::expect::begin_step("starting".into());
        let begun = Instant::now();
        let remaining = RUN_TIMEOUT.saturating_sub(started.elapsed());
        let failure = if remaining.is_zero() {
            Err(format!(
                "the reader run exceeded {} minutes",
                RUN_TIMEOUT.as_secs() / 60
            ))
        } else {
            match Browser::launch(&options).await {
                Err(error) => Err(format!("could not launch the browser: {error}")),
                Ok(browser) => {
                    let browser = Arc::new(browser);
                    let ctx = ReaderCtx {
                        browser: Arc::clone(&browser),
                        binary: binary.clone(),
                    };
                    let timeout = CASE_TIMEOUT.min(remaining);
                    let handle = tokio::spawn((case.run)(ctx));
                    let abort = handle.abort_handle();
                    let outcome = match tokio::time::timeout(timeout, handle).await {
                        Ok(Ok(Ok(report))) => Ok(report),
                        Ok(Ok(Err(error))) => Err(error.to_string()),
                        Ok(Err(join)) if join.is_panic() => {
                            Err(format!("panicked: {}", panic_text(&*join.into_panic())))
                        }
                        Ok(Err(join)) => Err(join.to_string()),
                        Err(_) => {
                            abort.abort();
                            Err(format!(
                                "{name} exceeded {}s while: {}",
                                timeout.as_secs(),
                                crate::expect::current_step()
                            ))
                        }
                    };
                    // Let an aborted case drop its fixture before the browser goes.
                    tokio::task::yield_now().await;
                    if let Ok(browser) = Arc::try_unwrap(browser) {
                        browser.close().await;
                    }
                    outcome
                }
            }
        };
        let elapsed = begun.elapsed().as_millis();
        match failure {
            Ok(report) => {
                passed += 1;
                println!("test {name} ... ok ({elapsed} ms)");
                for line in report.lines() {
                    println!("    {line}");
                }
            }
            Err(failure) => {
                println!("test {name} ... FAILED ({elapsed} ms)");
                failures.push((name, failure));
            }
        }
    }
    if !failures.is_empty() {
        println!("\nfailures:\n");
        for (name, failure) in &failures {
            println!("---- {name} ----\n{failure}\n");
        }
        println!("failures:");
        for (name, _) in &failures {
            println!("    {name}");
        }
    }
    let ok = failures.is_empty();
    println!(
        "\ntest result: {}. {passed} passed; {} failed; 0 ignored; 0 measured; {filtered} filtered out; finished in {:.2}s\n",
        if ok { "ok" } else { "FAILED" },
        failures.len(),
        started.elapsed().as_secs_f64()
    );
    if ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_bodies_match_the_node_fixture() {
        let wav = wav_bytes("ab", 0.05);
        assert_eq!(&wav[..4], b"RIFF");
        assert_eq!(&wav[8..16], b"WAVEfmt ");
        assert_eq!(wav.len(), 44 + 1200 * 2);
        assert_eq!(
            u32::from_le_bytes([wav[4], wav[5], wav[6], wav[7]]) as usize,
            wav.len() - 8
        );
        // seed = 'a' + 'b' = 195; sample 0 = round(sin(195 / 12) * 1000)
        let first = i16::from_le_bytes([wav[44], wav[45]]);
        assert_eq!(first, ((195.0_f64 / 12.0).sin() * 1000.0).round() as i16);
        assert_eq!(wav_bytes("", 0.0).len(), 44 + 24_000 * 2);

        let pcm = pcm_body(2.0);
        assert_eq!(&pcm[..4], &96_000_u32.to_be_bytes());
        assert_eq!(pcm.len(), 4 + 96_000 + 4);
        assert_eq!(&pcm[pcm.len() - 4..], &[0, 0, 0, 0]);
        assert_eq!(pcm_body(4.0).len(), 2 * (4 + 96_000) + 4);

        let timed = timed_body("Hi café", 1.0);
        let first_length = u32::from_be_bytes([timed[0], timed[1], timed[2], timed[3]]) as usize;
        assert_eq!(timed[4], 2);
        let word: Value = serde_json::from_slice(&timed[5..4 + first_length]).unwrap();
        assert_eq!(word["word"], json!("Hi"));
        assert_eq!(
            words("Talk to Shiv awn. She reads sequel."),
            ["Talk", "to", "Shiv", "awn", "She", "reads", "sequel"]
        );
        assert_eq!(words("café 😀 words"), ["café", "words"]);
        assert_eq!(dechunk(b"3\r\nabc\r\n0\r\n\r\n"), Some(b"abc".to_vec()));
        assert_eq!(dechunk(b"3\r\nab"), None);
    }
}
