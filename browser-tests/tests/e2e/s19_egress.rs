// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Neil Blackman
//! What an artifact in the viewer shell can and cannot reach.
//!
//! ADR-0003 (amended 2026-11-22) states the limits. This journey keeps two of them tested in a
//! real browser: the artifact CSP stops the connection APIs, and the shell's `frame-src 'self'`
//! stops the artifact frame from navigating to another origin. WebRTC is a known open channel and
//! is not asserted here, because it needs a non-loopback route that a test host may not have.

use std::io::{Read as _, Write as _};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use browser_tests::e2e::{Case, Ctx};
use browser_tests::{Error, e2e_case, expect};

const SPEC: &str = "19-egress";

pub fn cases() -> Vec<Case> {
    vec![e2e_case!(
        SPEC,
        "an artifact in the shell cannot connect out or navigate its frame to another origin",
        shell_artifact_egress
    )]
}

/// A second origin on loopback. It answers every request, so a request that the browser did
/// send would succeed, and it counts the connections it accepts. It runs on its own thread with
/// blocking sockets and lives until the test process exits.
fn other_origin() -> Result<(u16, Arc<AtomicUsize>), Error> {
    let listener = TcpListener::bind("127.0.0.1:0")
        .map_err(|error| Error::Assertion(format!("bind the second origin: {error}")))?;
    let port = listener
        .local_addr()
        .map_err(|error| Error::Assertion(format!("second origin address: {error}")))?
        .port();
    let accepted = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&accepted);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            counter.fetch_add(1, Ordering::SeqCst);
            std::thread::spawn(move || {
                // The reply does not depend on the request, and a failed read or write only
                // ends this connection.
                let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));
                let mut request = [0_u8; 1024];
                let _ = stream.read(&mut request);
                let _ = stream.write_all(
                    b"HTTP/1.1 200 OK\r\nAccess-Control-Allow-Origin: *\r\n\
                      Content-Type: text/html\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
                );
            });
        }
    });
    Ok((port, accepted))
}

fn artifact_html(port: u16) -> String {
    format!(
        r#"<!doctype html><title>egress</title><output id="result">running</output><script>
const http = "http://127.0.0.1:{port}";
const out = {{}};
async function attempt(name, run) {{
  try {{ await run(); out[name] = "sent"; }} catch (_) {{ out[name] = "blocked"; }}
}}
(async () => {{
  await attempt("fetch", () => fetch(http + "/fetch", {{ mode: "no-cors" }}));
  await attempt("xhr", () => new Promise((ok, fail) => {{
    const request = new XMLHttpRequest();
    request.open("GET", http + "/xhr"); request.onload = ok; request.onerror = fail; request.send();
  }}));
  await attempt("websocket", () => new Promise((ok, fail) => {{
    const socket = new WebSocket("ws://127.0.0.1:{port}/ws"); socket.onopen = ok; socket.onerror = fail;
  }}));
  await attempt("eventsource", () => new Promise((ok, fail) => {{
    const source = new EventSource(http + "/events"); source.onopen = ok; source.onerror = fail;
  }}));
  try {{ navigator.sendBeacon(http + "/beacon", "x"); }} catch (_) {{}}
  document.getElementById("result").textContent = JSON.stringify(out);
  setTimeout(() => {{ location.href = http + "/navigated"; }}, 100);
}})();
</script>"#
    )
}

async fn shell_artifact_egress(t: Ctx) -> Result<(), Error> {
    let (port, accepted) = other_origin()?;
    // Control: the second origin is reachable and the counter sees a connection.
    drop(TcpStream::connect(("127.0.0.1", port)).map_err(|error| {
        Error::Assertion(format!("the second origin is not reachable: {error}"))
    })?);
    for _ in 0..100 {
        if accepted.load(Ordering::SeqCst) == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(accepted.load(Ordering::SeqCst), 1, "control connection");

    let key = t.publisher_key().await?;
    let artifact = t
        .publish(
            &key,
            &format!("PW Egress {}", t.org),
            &artifact_html(port),
            None,
        )
        .await?;
    let id = artifact["id"].as_str().unwrap_or("").to_owned();

    let page = t.page().await?;
    page.goto(&format!("{}/{id}", t.base_url())).await?;
    let result = page.frame_locator("#vframe").locator("#result");
    expect(&result)
        .to_have_text(
            r#"{"fetch":"blocked","xhr":"blocked","websocket":"blocked","eventsource":"blocked"}"#,
        )
        .await?;
    // The artifact then tries to navigate its own frame to the second origin. Give the browser
    // time to send that request if it is going to.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(
        accepted.load(Ordering::SeqCst),
        1,
        "the artifact reached the second origin: a connection API or a frame navigation got out"
    );
    page.close().await;
    Ok(())
}
