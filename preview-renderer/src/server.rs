// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Neil Blackman
//! The HTTP contract of the Node renderer (`server.js`), unchanged:
//!
//! | Request | Response |
//! |---|---|
//! | `GET /health` | `200 {"status":"ok"}` |
//! | `POST /render` `{html, width, height}` | `200 image/png` |
//! | a second `POST /render` while one is in flight | `503 {"error":"renderer busy"}` |
//! | body above `MAX_BODY_BYTES` | `413 {"error":"request too large"}` |
//! | body that is not JSON | `400 {"error":"invalid JSON"}` |
//! | `html` that is not a string | `400 {"error":"html must be a string"}` |
//! | render error or deadline | `500 {"error":"render failed"}` |
//! | anything else | `404 {"error":"not found"}` |
//!
//! Every response carries `cache-control: no-store` and `x-content-type-options: nosniff`. A
//! request whose body does not arrive within `RENDER_TIMEOUT_MS + 2000` ms gets `408`, as Node's
//! `server.requestTimeout` did; headers must arrive within `RENDER_TIMEOUT_MS + 3000` ms.

use std::convert::Infallible;
use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use http_body_util::{BodyExt, Full};
use hyper::body::{Bytes, Incoming};
use hyper::header::HeaderValue;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::{TokioIo, TokioTimer};
use serde_json::Value;
use tokio::net::TcpListener;

use crate::config::{Config, DEFAULT_HEIGHT, DEFAULT_WIDTH, MAX_HEIGHT, MAX_WIDTH};
use crate::jsnum::bounded_dimension;
use crate::render::Renderer;

const JSON: &str = "application/json; charset=utf-8";
const PNG: &str = "image/png";

struct State {
    renderer: Renderer,
    max_body_bytes: usize,
    request_timeout: Duration,
    /// One render at a time; a second request gets 503 rather than a queue slot.
    busy: AtomicBool,
}

/// Releases the single render slot when dropped, including when the request is abandoned.
struct BusySlot(Arc<State>);

impl BusySlot {
    fn acquire(state: &Arc<State>) -> Option<Self> {
        state
            .busy
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .ok()
            .map(|_| Self(Arc::clone(state)))
    }
}

impl Drop for BusySlot {
    fn drop(&mut self) {
        self.0.busy.store(false, Ordering::Release);
    }
}

pub struct Server {
    listener: TcpListener,
    state: Arc<State>,
    headers_timeout: Duration,
}

impl Server {
    /// Listen on `0.0.0.0:PORT`, as the Node renderer did.
    pub async fn bind(config: &Config) -> io::Result<Self> {
        Self::bind_to(SocketAddr::from(([0, 0, 0, 0], config.port)), config).await
    }

    pub async fn bind_to(address: SocketAddr, config: &Config) -> io::Result<Self> {
        let listener = TcpListener::bind(address).await?;
        let state = Arc::new(State {
            renderer: Renderer::new(config.chrome.clone(), config.render_timeout),
            max_body_bytes: config.max_body_bytes,
            request_timeout: config.request_timeout(),
            busy: AtomicBool::new(false),
        });
        Ok(Self {
            listener,
            state,
            headers_timeout: config.headers_timeout(),
        })
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.listener.local_addr()
    }

    /// Accept connections until the future is dropped.
    pub async fn run(self) {
        loop {
            let stream = match self.listener.accept().await {
                Ok((stream, _)) => stream,
                Err(error) => {
                    // EMFILE and similar are transient; do not spin.
                    eprintln!("[artifact-preview] accept failed: {error}");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }
            };
            let _ = stream.set_nodelay(true);
            let state = Arc::clone(&self.state);
            let headers_timeout = self.headers_timeout;
            tokio::spawn(async move {
                let service = service_fn(move |request| handle(Arc::clone(&state), request));
                let _ = http1::Builder::new()
                    .timer(TokioTimer::new())
                    .header_read_timeout(headers_timeout)
                    .serve_connection(TokioIo::new(stream), service)
                    .await;
            });
        }
    }
}

async fn handle(
    state: Arc<State>,
    request: Request<Incoming>,
) -> Result<Response<Full<Bytes>>, Infallible> {
    // Node compared `req.url`, the raw request target, so a query string or an absolute-form
    // target does not match.
    let target = request.uri().to_string();
    if request.method() == Method::GET && target == "/health" {
        return Ok(respond(StatusCode::OK, r#"{"status":"ok"}"#, JSON));
    }
    if request.method() != Method::POST || target != "/render" {
        return Ok(error(StatusCode::NOT_FOUND, "not found"));
    }
    let Some(slot) = BusySlot::acquire(&state) else {
        return Ok(error(StatusCode::SERVICE_UNAVAILABLE, "renderer busy"));
    };
    Ok(render_route(state, slot, request).await)
}

async fn render_route(
    state: Arc<State>,
    slot: BusySlot,
    request: Request<Incoming>,
) -> Response<Full<Bytes>> {
    let declared_length = request
        .headers()
        .get(hyper::header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok());
    let read = read_body(request.into_body(), state.max_body_bytes, declared_length);
    let body = match tokio::time::timeout(state.request_timeout, read).await {
        Err(_) => return error(StatusCode::REQUEST_TIMEOUT, "request timeout"),
        Ok(Err(BodyError::TooLarge)) => {
            return error(StatusCode::PAYLOAD_TOO_LARGE, "request too large");
        }
        // Node's `for await (const chunk of req)` threw without a status, which became 500.
        Ok(Err(BodyError::Broken)) => {
            return error(StatusCode::INTERNAL_SERVER_ERROR, "render failed");
        }
        Ok(Ok(body)) => body,
    };

    // `JSON.parse(Buffer.concat(chunks).toString("utf8"))`: invalid UTF-8 becomes U+FFFD first.
    let Ok(input) = serde_json::from_str::<Value>(&String::from_utf8_lossy(&body)) else {
        return error(StatusCode::BAD_REQUEST, "invalid JSON");
    };
    drop(body);
    let Value::Object(mut fields) = input else {
        return error(StatusCode::BAD_REQUEST, "html must be a string");
    };
    if !fields.get("html").is_some_and(Value::is_string) {
        return error(StatusCode::BAD_REQUEST, "html must be a string");
    }
    let dimensions =
        bounded_dimension(fields.get("width"), DEFAULT_WIDTH, MAX_WIDTH).and_then(|width| {
            Ok((
                width,
                bounded_dimension(fields.get("height"), DEFAULT_HEIGHT, MAX_HEIGHT)?,
            ))
        });
    let Ok((width, height)) = dimensions else {
        // `Number(value)` threw a TypeError in Node, which surfaced as a render failure.
        return error(StatusCode::INTERNAL_SERVER_ERROR, "render failed");
    };
    let Some(Value::String(html)) = fields.remove("html") else {
        unreachable!("checked above");
    };
    drop(fields);

    // The render runs in its own task that owns the busy slot, so a client that disconnects
    // mid-render cannot cut the cleanup short or free the slot while a browser is still running.
    let renderer = state.renderer.clone();
    let task = tokio::spawn(async move {
        let _slot = slot;
        renderer.render(&html, width, height).await
    });
    match task.await {
        Ok(Ok(png)) => respond(StatusCode::OK, png, PNG),
        Ok(Err(render_error)) => {
            eprintln!("[artifact-preview] {render_error}");
            error(StatusCode::INTERNAL_SERVER_ERROR, "render failed")
        }
        Err(join_error) => {
            eprintln!("[artifact-preview] render task failed: {join_error}");
            error(StatusCode::INTERNAL_SERVER_ERROR, "render failed")
        }
    }
}

enum BodyError {
    TooLarge,
    Broken,
}

async fn read_body(
    mut body: Incoming,
    limit: usize,
    declared_length: Option<u64>,
) -> Result<Vec<u8>, BodyError> {
    if declared_length.is_some_and(|length| length > limit as u64) {
        return Err(BodyError::TooLarge);
    }
    let mut collected = Vec::new();
    while let Some(frame) = body.frame().await {
        let frame = frame.map_err(|_| BodyError::Broken)?;
        if let Ok(data) = frame.into_data() {
            if collected.len().saturating_add(data.len()) > limit {
                return Err(BodyError::TooLarge);
            }
            collected.extend_from_slice(&data);
        }
    }
    Ok(collected)
}

fn error(status: StatusCode, message: &str) -> Response<Full<Bytes>> {
    let body = serde_json::to_vec(&serde_json::json!({"error": message}))
        .expect("a JSON value always serializes");
    respond(status, body, JSON)
}

fn respond(
    status: StatusCode,
    body: impl Into<Bytes>,
    content_type: &'static str,
) -> Response<Full<Bytes>> {
    let body = body.into();
    let length = body.len();
    let mut response = Response::new(Full::new(body));
    *response.status_mut() = status;
    let headers = response.headers_mut();
    headers.insert(
        hyper::header::CONTENT_TYPE,
        HeaderValue::from_static(content_type),
    );
    headers.insert(hyper::header::CONTENT_LENGTH, HeaderValue::from(length));
    headers.insert(
        hyper::header::CACHE_CONTROL,
        HeaderValue::from_static("no-store"),
    );
    headers.insert(
        hyper::header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    response
}
