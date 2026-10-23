// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Neil Blackman
//! Tests of the harness itself, against the pinned browser. They need `CHROME_HEADLESS_SHELL`;
//! without it they are skipped, unless `CI` is set, in which case they fail.

use std::io::{Read, Write};
use std::net::{IpAddr, TcpListener, UdpSocket};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use browser_tests::{
    Browser, BrowserOptions, Error, LogEntry, Network, Page, Response, chrome_from_env,
};
use serde_json::json;

const ORIGIN: &str = "https://harness.test";

async fn launch(network: Network) -> Option<Browser> {
    let chrome = chrome_from_env()?;
    Some(
        Browser::launch(&BrowserOptions::new(chrome).network(network))
            .await
            .expect("launch"),
    )
}

/// A page on [`ORIGIN`] answering `/` with an empty document and `/hello.txt` with text.
async fn routed_page(browser: &Browser) -> Page {
    let page = browser.new_page().await.unwrap();
    page.route(&format!("{ORIGIN}/"), |request| {
        match request.path.as_str() {
            "/" => Response::ok(
                "text/html; charset=utf-8",
                "<!doctype html><title>t</title>",
            ),
            "/hello.txt" => Response::ok("text/plain", "hello from Rust"),
            other => Response::error(404, &format!("no {other}")),
        }
    })
    .await
    .unwrap();
    page.goto(&format!("{ORIGIN}/")).await.unwrap();
    page
}

#[tokio::test]
async fn evaluate_awaits_promises_and_passes_json_arguments() {
    let Some(browser) = launch(Network::LockedDown).await else {
        return;
    };
    let page = browser.new_page().await.unwrap();
    assert_eq!(
        page.evaluate("Promise.resolve({ a: [1, 'x', null], b: true })")
            .await
            .unwrap(),
        json!({"a": [1, "x", null], "b": true})
    );
    assert_eq!(page.evaluate("undefined").await.unwrap(), json!(null));
    let sum = page
        .call("async (a, b) => a + b.n", &[json!(2), json!({"n": 3})])
        .await
        .unwrap();
    assert_eq!(sum, json!(5));
    // Arguments are data: quotes and script-like text stay inert.
    let echoed = page
        .call(
            "(text) => text",
            &[json!("\"); throw 1; (\"\u{2028}</script>")],
        )
        .await
        .unwrap();
    assert_eq!(echoed, json!("\"); throw 1; (\"\u{2028}</script>"));
    browser.close().await;
}

#[tokio::test]
async fn thrown_errors_and_rejections_are_evaluation_errors() {
    let Some(browser) = launch(Network::LockedDown).await else {
        return;
    };
    let page = browser.new_page().await.unwrap();
    let thrown = page
        .evaluate("throw new TypeError('boom')")
        .await
        .unwrap_err();
    assert!(
        matches!(&thrown, Error::Evaluation(text) if text.contains("TypeError: boom")),
        "{thrown}"
    );
    let rejected = page
        .evaluate("Promise.reject(new RangeError('later'))")
        .await
        .unwrap_err();
    assert!(
        matches!(&rejected, Error::Evaluation(text) if text.contains("RangeError: later")),
        "{rejected}"
    );
    let hung = page
        .evaluate_with_timeout("new Promise(() => {})", Duration::from_millis(200))
        .await
        .unwrap_err();
    assert!(matches!(hung, Error::Timeout { .. }), "{hung}");
    // The page still answers after a timed-out evaluation.
    assert_eq!(page.evaluate("1 + 1").await.unwrap(), json!(2));
    browser.close().await;
}

#[tokio::test]
async fn console_output_and_uncaught_errors_are_recorded_in_order() {
    let Some(browser) = launch(Network::LockedDown).await else {
        return;
    };
    let page = browser.new_page().await.unwrap();
    let mut cursor = page.log_mark();
    page.evaluate(
        "console.log('plain', 1, { a: 2 }); console.warn('careful'); console.error('broken');
         setTimeout(() => { throw new Error('from a timer'); }, 0);
         Promise.reject(new Error('never handled'));
         const late = Promise.reject(new Error('handled later'));
         setTimeout(() => late.catch(() => {}), 0);
         new Promise((resolve) => setTimeout(resolve, 50))",
    )
    .await
    .unwrap();
    let log = page.log_after(&mut cursor);
    let texts: Vec<String> = log.iter().map(ToString::to_string).collect();
    assert!(
        matches!(&log[0], LogEntry::Console { level, text, .. } if level == "log" && text == "plain 1 Object"),
        "{texts:#?}"
    );
    assert!(matches!(&log[1], LogEntry::Console { level, .. } if level == "warning"));
    assert!(
        matches!(&log[2], LogEntry::Console { level, text, .. } if level == "error" && text == "broken")
    );
    assert!(
        texts.iter().any(|text| text.contains("from a timer")),
        "{texts:#?}"
    );
    assert!(
        texts.iter().any(|text| text.contains("never handled")),
        "{texts:#?}"
    );
    assert!(
        !texts.iter().any(|text| text.contains("handled later")),
        "a rejection handled later is withdrawn: {texts:#?}"
    );
    let failures: Vec<&LogEntry> = log.iter().filter(|entry| entry.is_failure()).collect();
    assert_eq!(failures.len(), 3, "{texts:#?}");
    assert!(page.log_after(&mut cursor).is_empty());
    browser.close().await;
}

#[tokio::test]
async fn compile_checks_parse_without_running() {
    let Some(browser) = launch(Network::LockedDown).await else {
        return;
    };
    let page = routed_page(&browser).await;
    assert_eq!(
        page.compile_script("window.ran = true;", "ok.js")
            .await
            .unwrap(),
        None
    );
    let error = page
        .compile_script("let a = 1;\nlet b = ;", "bad.js")
        .await
        .unwrap()
        .expect("syntax error");
    assert_eq!(error.line, 2, "{error}");
    assert!(error.message.contains("SyntaxError"), "{error}");
    // Module goal: `await` at top level parses, an unterminated block does not, and nothing runs.
    assert_eq!(
        page.compile_module("window.ran = await 1;", "ok.mjs")
            .await
            .unwrap(),
        None
    );
    assert!(
        page.compile_module("function f() {", "bad.mjs")
            .await
            .unwrap()
            .is_some()
    );
    // Classic scripts may use `await` as a name; modules may not.
    assert_eq!(
        page.compile_script("var await = 1;", "sloppy.js")
            .await
            .unwrap(),
        None
    );
    assert!(
        page.compile_module("var await = 1;", "strict.mjs")
            .await
            .unwrap()
            .is_some()
    );
    assert_eq!(
        page.evaluate("window.ran === undefined").await.unwrap(),
        json!(true)
    );
    assert!(
        page.log().iter().all(|entry| !entry.is_failure()),
        "{:?}",
        page.log()
    );
    browser.close().await;
}

#[tokio::test]
async fn routes_serve_a_secure_virtual_origin() {
    let Some(browser) = launch(Network::LockedDown).await else {
        return;
    };
    let page = routed_page(&browser).await;
    assert_eq!(
        page.evaluate("[location.href, isSecureContext]")
            .await
            .unwrap(),
        json!([format!("{ORIGIN}/"), true])
    );
    assert_eq!(
        page.evaluate("fetch('/hello.txt').then((r) => r.text())")
            .await
            .unwrap(),
        json!("hello from Rust")
    );
    assert_eq!(
        page.evaluate("fetch('/missing').then((r) => r.status)")
            .await
            .unwrap(),
        json!(404)
    );
    browser.close().await;
}

#[tokio::test]
async fn viewport_content_and_dialogs() {
    let Some(browser) = launch(Network::LockedDown).await else {
        return;
    };
    let page = browser.new_page().await.unwrap();
    assert_eq!(
        page.evaluate("[innerWidth, innerHeight]").await.unwrap(),
        json!([1280, 720])
    );
    page.set_viewport(320, 480, 2.0).await.unwrap();
    assert_eq!(
        page.evaluate("[innerWidth, innerHeight, devicePixelRatio]")
            .await
            .unwrap(),
        json!([320, 480, 2])
    );
    page.set_content("<h1 id=t>Title</h1><img src='data:image/gif;base64,R0lGODlhAQABAAAAACw='>")
        .await
        .unwrap();
    assert_eq!(
        page.evaluate("[document.getElementById('t').textContent, document.readyState]")
            .await
            .unwrap(),
        json!(["Title", "complete"])
    );
    let mark = page.log_mark();
    assert_eq!(
        page.evaluate("confirm('sure?')").await.unwrap(),
        json!(false)
    );
    let log = page.log_since(mark);
    assert_eq!(
        log,
        vec![LogEntry::Dialog {
            kind: "confirm".into(),
            message: "sure?".into()
        }]
    );
    assert!(log[0].is_failure());
    browser.close().await;
}

#[tokio::test]
async fn pages_are_isolated_from_each_other() {
    let Some(browser) = launch(Network::LockedDown).await else {
        return;
    };
    let first = routed_page(&browser).await;
    let second = routed_page(&browser).await;
    first
        .evaluate("localStorage.setItem('k', 'first')")
        .await
        .unwrap();
    assert_eq!(
        second.evaluate("localStorage.getItem('k')").await.unwrap(),
        json!(null)
    );
    first.close().await;
    assert_eq!(second.evaluate("2 * 21").await.unwrap(), json!(42));
    browser.close().await;
}

#[tokio::test]
async fn close_leaves_no_browser_process() {
    let Some(browser) = launch(Network::LockedDown).await else {
        return;
    };
    let pgid = browser.pgid();
    assert!(pgid > 1);
    assert!(!group_members(pgid).is_empty());
    browser.close().await;
    assert_eq!(group_members(pgid), Vec::<u32>::new());
}

/// Running (non-zombie) processes in process group `pgid`.
fn group_members(pgid: u32) -> Vec<u32> {
    std::fs::read_dir("/proc")
        .unwrap()
        .flatten()
        .filter_map(|entry| {
            let pid: u32 = entry.file_name().to_str()?.parse().ok()?;
            let stat = std::fs::read_to_string(entry.path().join("stat")).ok()?;
            let rest = &stat[stat.rfind(')')? + 1..];
            let mut fields = rest.split_ascii_whitespace();
            let state = fields.next()?;
            let _ppid = fields.next();
            let group: u32 = fields.next()?.parse().ok()?;
            (group == pgid && state != "Z" && state != "X").then_some(pid)
        })
        .collect()
}

/// A TCP listener that answers every connection with a tiny HTTP page and counts them.
struct Canary {
    port: u16,
    hits: Arc<AtomicUsize>,
}

impl Canary {
    fn start(address: IpAddr) -> Option<Self> {
        let listener = TcpListener::bind((address, 0)).ok()?;
        let port = listener.local_addr().ok()?.port();
        let hits = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&hits);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                counter.fetch_add(1, Ordering::SeqCst);
                let mut buffer = [0; 2048];
                let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));
                let _ = stream.read(&mut buffer);
                let body = "<!doctype html><title>canary</title>";
                let _ = write!(
                    stream,
                    "HTTP/1.1 200 OK\r\ncontent-type: text/html\r\naccess-control-allow-origin: *\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
            }
        });
        Some(Self { port, hits })
    }

    fn hits(&self) -> usize {
        self.hits.load(Ordering::SeqCst)
    }
}

/// This machine's address on its default route, if it has one that is not loopback.
fn lan_address() -> Option<IpAddr> {
    let socket = UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("192.0.2.1:9").ok()?;
    let address = socket.local_addr().ok()?.ip();
    (!address.is_loopback() && !address.is_unspecified()).then_some(address)
}

/// Try to reach `url` from the page with fetch, then let late packets arrive.
async fn poke(page: &Page, url: &str) {
    let _ = page
        .call(
            "(url) => fetch(url, { mode: 'no-cors' }).then(() => 'reached', (error) => String(error))",
            &[json!(url)],
        )
        .await;
    tokio::time::sleep(Duration::from_millis(200)).await;
}

#[tokio::test]
async fn locked_down_pages_reach_neither_loopback_nor_the_lan() {
    let Some(browser) = launch(Network::LockedDown).await else {
        return;
    };
    let page = routed_page(&browser).await;
    let loopback = Canary::start(IpAddr::from([127, 0, 0, 1])).unwrap();
    poke(&page, &format!("http://127.0.0.1:{}/", loopback.port)).await;
    poke(&page, &format!("http://localhost:{}/", loopback.port)).await;
    assert_eq!(loopback.hits(), 0);
    if let Some(lan) = lan_address().and_then(Canary::start) {
        let address = lan_address().unwrap();
        poke(&page, &format!("http://{address}:{}/", lan.port)).await;
        assert_eq!(lan.hits(), 0);
    }
    browser.close().await;
}

#[tokio::test]
async fn loopback_only_pages_reach_loopback_but_not_the_lan() {
    let Some(browser) = launch(Network::LoopbackOnly).await else {
        return;
    };
    let page = browser.new_page().await.unwrap();
    let loopback = Canary::start(IpAddr::from([127, 0, 0, 1])).unwrap();
    page.goto(&format!("http://127.0.0.1:{}/", loopback.port))
        .await
        .unwrap();
    assert_eq!(
        page.evaluate("document.title").await.unwrap(),
        json!("canary")
    );
    let before = loopback.hits();
    poke(&page, &format!("http://localhost:{}/again", loopback.port)).await;
    assert!(loopback.hits() > before, "localhost resolves and connects");
    if let Some(v6) = Canary::start(IpAddr::from(std::net::Ipv6Addr::LOCALHOST)) {
        poke(&page, &format!("http://[::1]:{}/", v6.port)).await;
        assert!(v6.hits() > 0, "[::1] connects");
    }
    if let Some(address) = lan_address() {
        let lan = Canary::start(address).unwrap();
        poke(&page, &format!("http://{address}:{}/", lan.port)).await;
        assert_eq!(lan.hits(), 0, "a LAN address is not loopback");
        let navigation = page
            .goto(&format!("http://{address}:{}/", lan.port))
            .await
            .unwrap_err();
        assert!(matches!(navigation, Error::Navigation(_)), "{navigation}");
        assert_eq!(lan.hits(), 0);
    } else {
        eprintln!("no non-loopback address: skipping the LAN half");
    }

    let named = page.goto("http://example.com/").await.unwrap_err();
    assert!(matches!(named, Error::Navigation(_)), "{named}");
    browser.close().await;
}

/// `js/harness.js` keeps the `node:vm` and `node:assert/strict` behaviour the ported suites rely
/// on. Each expectation below was checked against Node 22 first.
#[tokio::test]
async fn the_in_page_harness_keeps_node_semantics() {
    let Some(browser) = launch(Network::LockedDown).await else {
        return;
    };
    let page = browser_tests::suite::open_repo_page(&browser, &browser_tests::repo_root())
        .await
        .unwrap();
    let failures = page
        .evaluate(
            r#"import("/browser-tests/js/harness.js").then(async (harness) => {
                const { assert, runInNewContext, createContext, runInContext, setImmediate } = harness;
                const failures = [];
                const check = (name, ok) => { if (!ok) failures.push(name); };
                const throws = (fn) => { try { fn(); return false; } catch { return true; } };

                // node:vm: a separate realm whose globals are the sandbox plus ECMAScript built-ins.
                check("a literal from another realm is not deep-equal", throws(() => assert.deepEqual(runInNewContext("({ a: 1 })"), { a: 1 })));
                check("a JSON copy is deep-equal", !throws(() => assert.deepEqual(JSON.parse(JSON.stringify(runInNewContext("({ a: 1 })"))), { a: 1 })));
                check("no web globals leak in", runInNewContext("typeof window + typeof document + typeof setTimeout + typeof fetch + typeof console + typeof Promise") === "undefinedundefinedundefinedundefinedobjectfunction");
                check("using a missing global fails", throws(() => runInNewContext("window.location")));
                const sandbox = { n: 1 };
                const read = runInNewContext("(function () { return n; })", sandbox);
                sandbox.n = 2;
                check("later sandbox changes are visible", read() === 2);
                const target = {};
                runInNewContext("leak = 5", target);
                check("implicit globals land on the sandbox", target.leak === 5);
                check("the completion value is returned", runInNewContext("var a = 1; a + 1") === 2);
                check("the context has its own Array", runInNewContext("[] instanceof Array") === true && runInNewContext("x instanceof Array", { x: [] }) === false);
                check("declarations are visible to the code", runInNewContext("function g() { return 7; } g()") === 7);
                check("top-level this is the global", runInNewContext("this === globalThis") === true);
                check("native functions from the sandbox work as plain calls", runInNewContext("typeof setTimeout(() => {}, 0)", { setTimeout }) === "number");
                createContext(sandbox);
                check("createContext and runInContext share one context", runInContext("n + 1", sandbox) === 3 && runInContext("n + 1", sandbox) === 3);

                // node:assert/strict.
                check("-0 is not deep-equal to 0", throws(() => assert.deepEqual(0, -0)));
                check("NaN equals NaN", !throws(() => assert.equal(NaN, NaN)));
                check("maps compare object keys deeply", !throws(() => assert.deepEqual(new Map([[{ k: 1 }, 2]]), new Map([[{ k: 1 }, 2]]))));
                check("match needs a string", throws(() => assert.match(5, /5/)));
                check("doesNotMatch rejects a match", throws(() => assert.doesNotMatch("abc", /b/)));
                check("ok rejects falsy values", throws(() => assert.ok(0)) && !throws(() => assert.ok(1)));
                let validated = false;
                await assert.rejects(Promise.reject(new Error("x")), (error) => (validated = error.message === "x"));
                check("rejects runs the validation function", validated);
                let refused = false;
                try { await assert.rejects(Promise.reject(new Error("x")), () => false); } catch (error) { refused = error instanceof assert.AssertionError; }
                check("a validation function returning false fails", refused);
                let missing = false;
                try { await assert.rejects(Promise.resolve(1)); } catch (error) { missing = error instanceof assert.AssertionError; }
                check("a resolved promise fails rejects", missing);

                // Timers.
                const order = [];
                setImmediate(() => order.push("immediate"));
                Promise.resolve().then(() => order.push("microtask"));
                await new Promise((resolve) => setImmediate(resolve));
                check("setImmediate runs after pending promise jobs", order.join() === "microtask,immediate");
                return failures;
            })"#,
        )
        .await
        .unwrap();
    assert_eq!(failures, json!([]));
    assert!(
        page.log().iter().all(|entry| !entry.is_failure()),
        "{:?}",
        page.log()
    );
    browser.close().await;
}

#[tokio::test]
async fn exposed_functions_call_into_rust_across_documents() {
    let Some(browser) = launch(Network::LockedDown).await else {
        return;
    };
    let page = routed_page(&browser).await;
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&calls);
    page.expose_function("rustAdd", move |args| {
        counter.fetch_add(1, Ordering::SeqCst);
        match (
            args.first().and_then(|a| a.as_f64()),
            args.get(1).and_then(|b| b.as_f64()),
        ) {
            (Some(a), Some(b)) => Ok(json!(a + b)),
            _ => Err(format!("not two numbers: {args:?}")),
        }
    })
    .await
    .unwrap();
    assert_eq!(
        page.evaluate("rustAdd(2, 3)").await.unwrap().as_f64(),
        Some(5.0)
    );
    let rejected = page
        .evaluate("rustAdd('x').then(() => 'resolved', (error) => error.message)")
        .await
        .unwrap();
    assert_eq!(rejected, json!("not two numbers: [String(\"x\")]"));
    // `document.open` keeps the global; a navigation gets the function again.
    page.set_content("<p>replaced</p>").await.unwrap();
    assert_eq!(
        page.evaluate("rustAdd(1, 1)").await.unwrap().as_f64(),
        Some(2.0)
    );
    page.goto(&format!("{ORIGIN}/")).await.unwrap();
    assert_eq!(
        page.evaluate("rustAdd(4, 4)").await.unwrap().as_f64(),
        Some(8.0)
    );
    assert_eq!(calls.load(Ordering::SeqCst), 4);
    browser.close().await;
}

#[tokio::test]
async fn frame_locators_evaluate_wait_and_set_content() {
    let Some(browser) = launch(Network::LockedDown).await else {
        return;
    };
    let page = routed_page(&browser).await;
    page.set_content("<iframe id=\"inner\" srcdoc=\"<p id=greeting>hi</p>\"></iframe>")
        .await
        .unwrap();
    let frame = page.frame_locator("#inner");
    assert_eq!(
        frame
            .evaluate(
                "(suffix) => document.getElementById('greeting').textContent + suffix",
                json!("!")
            )
            .await
            .unwrap(),
        json!("hi!")
    );
    frame
        .evaluate(
            "() => setTimeout(() => { window.done = 42; }, 100)",
            json!(null),
        )
        .await
        .unwrap();
    assert_eq!(
        frame
            .wait_for_function("() => window.done", json!(null))
            .await
            .unwrap(),
        json!(42)
    );
    let error = frame
        .wait_for_function_with_timeout("() => false", json!(null), Duration::from_millis(200))
        .await
        .unwrap_err();
    assert!(matches!(error, Error::Timeout { .. }), "{error}");
    frame
        .set_content("<p id=fresh>new</p><script>window.ran = true</script>")
        .await
        .unwrap();
    assert_eq!(
        frame
            .locator("#fresh")
            .text_content()
            .await
            .unwrap()
            .as_deref(),
        Some("new")
    );
    assert_eq!(
        frame
            .evaluate("() => window.ran", json!(null))
            .await
            .unwrap(),
        json!(true)
    );
    let started = std::time::Instant::now();
    let error = page
        .wait_for_function_with_timeout("() => false", &[], Duration::from_millis(200))
        .await
        .unwrap_err();
    assert!(matches!(error, Error::Timeout { .. }), "{error}");
    assert!(started.elapsed() < Duration::from_secs(5));
    browser.close().await;
}

#[tokio::test]
async fn drag_mouse_drops_and_still_clicks() {
    let Some(browser) = launch(Network::LockedDown).await else {
        return;
    };
    let page = routed_page(&browser).await;
    let html = r#"<div id="source" draggable="true" style="position:absolute;left:20px;top:20px;width:80px;height:40px">drag me</div>
<div id="target" style="position:absolute;left:300px;top:200px;width:120px;height:80px">drop here</div>
<script>
  window.events = [];
  source.addEventListener('dragstart', (event) => { event.dataTransfer.setData('text/plain', 'payload'); events.push('dragstart'); });
  target.addEventListener('dragenter', (event) => { event.preventDefault(); events.push('dragenter'); });
  target.addEventListener('dragover', (event) => event.preventDefault());
  target.addEventListener('drop', (event) => { event.preventDefault(); events.push('drop:' + event.dataTransfer.getData('text/plain')); });
</script>"#;
    page.set_content(html).await.unwrap();
    let mut mouse = page.drag_mouse();
    mouse.move_to(60.0, 40.0, 1).await.unwrap();
    mouse.down().await.unwrap();
    mouse.move_to(70.0, 50.0, 3).await.unwrap();
    assert!(mouse.is_dragging());
    mouse.move_to(360.0, 240.0, 10).await.unwrap();
    mouse.up().await.unwrap();
    assert!(!mouse.is_dragging());
    let events = page.evaluate("events").await.unwrap();
    assert_eq!(events[0], json!("dragstart"));
    assert!(
        events.as_array().unwrap().contains(&json!("dragenter")),
        "{events}"
    );
    assert_eq!(
        events.as_array().unwrap().last(),
        Some(&json!("drop:payload"))
    );
    // Without a drag, the same mouse clicks as usual.
    page.set_content(
        "<button onclick=\"window.clicked = true\" style=\"width:100px;height:40px\">b</button>",
    )
    .await
    .unwrap();
    let mut mouse = page.drag_mouse();
    mouse.move_to(30.0, 20.0, 1).await.unwrap();
    mouse.down().await.unwrap();
    mouse.up().await.unwrap();
    assert_eq!(
        page.evaluate("window.clicked === true").await.unwrap(),
        json!(true)
    );
    browser.close().await;
}
