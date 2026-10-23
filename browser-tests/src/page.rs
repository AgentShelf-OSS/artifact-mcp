// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Neil Blackman
//! One page (a tab in its own browser context) and what tests do with it.

use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine;
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::Error;
use crate::connection::{Connection, EventHub, EventSink, Subscription, lock};
use crate::route::{Request, Response, Route};

/// Something the page reported on its own: console output, an uncaught exception, a dialog or a
/// crash. [`LogEntry::is_failure`] says whether a test should fail because of it.
#[derive(Debug, Clone, PartialEq)]
pub enum LogEntry {
    /// A `console.*` call. `level` is the DevTools type: `log`, `debug`, `info`, `warning`,
    /// `error`, `assert`, `trace` and so on.
    Console {
        level: String,
        text: String,
        location: Option<String>,
    },
    /// An uncaught exception, or a promise rejection that stayed unhandled.
    Exception {
        text: String,
        location: Option<String>,
    },
    /// A JavaScript dialog (`alert`, `confirm`, `prompt`, `beforeunload`). The harness dismissed
    /// it.
    Dialog { kind: String, message: String },
    /// The page's renderer crashed or its target went away.
    Crash { reason: String },
}

impl LogEntry {
    /// Console errors, failed `console.assert` calls, uncaught exceptions, dialogs and crashes.
    pub fn is_failure(&self) -> bool {
        match self {
            Self::Console { level, .. } => level == "error" || level == "assert",
            Self::Exception { .. } | Self::Dialog { .. } | Self::Crash { .. } => true,
        }
    }
}

impl fmt::Display for LogEntry {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let at = |location: &Option<String>| {
            location
                .as_ref()
                .map(|location| format!(" (at {location})"))
                .unwrap_or_default()
        };
        match self {
            Self::Console {
                level,
                text,
                location,
            } => write!(formatter, "console.{level}: {text}{}", at(location)),
            Self::Exception { text, location } => {
                write!(formatter, "uncaught: {text}{}", at(location))
            }
            Self::Dialog { kind, message } => write!(formatter, "dialog {kind}: {message}"),
            Self::Crash { reason } => write!(formatter, "crash: {reason}"),
        }
    }
}

/// A syntax error reported by [`Page::compile_script`] or [`Page::compile_module`]. Line and
/// column are 1-based; 0 means the browser did not say.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScriptError {
    pub message: String,
    pub line: u64,
    pub column: u64,
}

impl fmt::Display for ScriptError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.line == 0 {
            return formatter.write_str(&self.message);
        }
        write!(formatter, "{}:{}: {}", self.line, self.column, self.message)
    }
}

struct Logged {
    entry: LogEntry,
    /// `Runtime.exceptionThrown` id, so `Runtime.exceptionRevoked` can withdraw the entry.
    exception_id: Option<i64>,
    revoked: bool,
}

/// What the reader task records for one page.
pub(crate) struct PageState {
    log: Mutex<Vec<Logged>>,
    hub: EventHub,
    /// Events the page's own task must answer (`Fetch.requestPaused`, dialogs).
    active: mpsc::UnboundedSender<(String, Value)>,
}

impl PageState {
    fn push(&self, entry: LogEntry, exception_id: Option<i64>) {
        lock(&self.log).push(Logged {
            entry,
            exception_id,
            revoked: false,
        });
    }
}

impl EventSink for PageState {
    fn on_event(&self, method: &str, params: &Value) {
        match method {
            "Runtime.consoleAPICalled" => self.push(console_entry(params), None),
            "Runtime.exceptionThrown" => {
                let details = params.get("exceptionDetails").unwrap_or(&Value::Null);
                let entry = LogEntry::Exception {
                    text: exception_text(details),
                    location: exception_location(details),
                };
                self.push(entry, details.get("exceptionId").and_then(Value::as_i64));
            }
            "Runtime.exceptionRevoked" => {
                let id = params.get("exceptionId").and_then(Value::as_i64);
                for logged in lock(&self.log).iter_mut() {
                    if id.is_some() && logged.exception_id == id {
                        logged.revoked = true;
                    }
                }
            }
            "Page.javascriptDialogOpening" => {
                let text = |name| {
                    params
                        .get(name)
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_owned()
                };
                self.push(
                    LogEntry::Dialog {
                        kind: text("type"),
                        message: text("message"),
                    },
                    None,
                );
                let _ = self.active.send((method.to_owned(), params.clone()));
            }
            "Fetch.requestPaused" => {
                let _ = self.active.send((method.to_owned(), params.clone()));
            }
            "Inspector.targetCrashed" | "Inspector.detached" => self.push(
                LogEntry::Crash {
                    reason: method.to_owned(),
                },
                None,
            ),
            _ => {}
        }
        self.hub.publish(method, params);
    }
}

fn console_entry(params: &Value) -> LogEntry {
    let level = params
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("log")
        .to_owned();
    let text = params
        .get("args")
        .and_then(Value::as_array)
        .map(|args| {
            args.iter()
                .map(remote_object_text)
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_default();
    let location = params
        .pointer("/stackTrace/callFrames/0")
        .map(|frame| frame_location(frame, "lineNumber", "columnNumber"));
    LogEntry::Console {
        level,
        text,
        location,
    }
}

/// A readable rendering of one `Runtime.RemoteObject`.
fn remote_object_text(object: &Value) -> String {
    if let Some(value) = object.get("value") {
        return match value {
            Value::String(text) => text.clone(),
            other => other.to_string(),
        };
    }
    if let Some(Value::String(text)) = object.get("unserializableValue") {
        return text.clone();
    }
    if let Some(Value::String(text)) = object.get("description") {
        return text.clone();
    }
    object
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("undefined")
        .to_owned()
}

fn frame_location(frame: &Value, line: &str, column: &str) -> String {
    let url = frame.get("url").and_then(Value::as_str).unwrap_or("");
    let line = frame.get(line).and_then(Value::as_u64).unwrap_or(0) + 1;
    let column = frame.get(column).and_then(Value::as_u64).unwrap_or(0) + 1;
    format!("{url}:{line}:{column}")
}

/// The message of `Runtime.ExceptionDetails`: the thrown error's description (with its stack)
/// when there is one, else the summary text.
pub(crate) fn exception_text(details: &Value) -> String {
    let exception = details.get("exception");
    if let Some(Value::String(description)) =
        exception.and_then(|exception| exception.get("description"))
    {
        return description.clone();
    }
    let text = details
        .get("text")
        .and_then(Value::as_str)
        .unwrap_or("exception");
    match exception.and_then(|exception| exception.get("value")) {
        Some(value) => format!("{text} {value}"),
        None => text.to_owned(),
    }
}

fn exception_location(details: &Value) -> Option<String> {
    if let Some(frame) = details.pointer("/stackTrace/callFrames/0") {
        return Some(frame_location(frame, "lineNumber", "columnNumber"));
    }
    details
        .get("url")
        .and_then(Value::as_str)
        .filter(|url| !url.is_empty())
        .map(|_| frame_location(details, "lineNumber", "columnNumber"))
}

/// One tab in its own browser context: fresh cookies, storage and cache. Created by
/// [`crate::Browser::new_page`].
pub struct Page {
    connection: Arc<Connection>,
    session: String,
    target: String,
    context: String,
    main_frame: String,
    state: Arc<PageState>,
    routes: Arc<Mutex<Vec<Route>>>,
    events: JoinHandle<()>,
    timeout: Duration,
}

impl Page {
    pub(crate) async fn open(
        connection: Arc<Connection>,
        timeout: Duration,
    ) -> Result<Self, Error> {
        let call = |method: &'static str, params: Value| {
            let connection = Arc::clone(&connection);
            async move { connection.call(None, method, params, timeout).await }
        };
        let context = string_field(
            &call(
                "Target.createBrowserContext",
                json!({"disposeOnDetach": true}),
            )
            .await?,
            "browserContextId",
        )?;
        let target = string_field(
            &call(
                "Target.createTarget",
                json!({"url": "about:blank", "browserContextId": context}),
            )
            .await?,
            "targetId",
        )?;
        let session = string_field(
            &call(
                "Target.attachToTarget",
                json!({"targetId": target, "flatten": true}),
            )
            .await?,
            "sessionId",
        )?;
        let (active, active_events) = mpsc::unbounded_channel();
        let state = Arc::new(PageState {
            log: Mutex::new(Vec::new()),
            hub: EventHub::default(),
            active,
        });
        connection.register_session(&session, Arc::clone(&state) as Arc<dyn EventSink>);
        let routes = Arc::new(Mutex::new(Vec::new()));
        let events = tokio::spawn(answer_events(
            Arc::downgrade(&connection),
            session.clone(),
            Arc::clone(&routes),
            active_events,
            timeout,
        ));
        let mut page = Self {
            connection,
            session,
            target,
            context,
            main_frame: String::new(),
            state,
            routes,
            events,
            timeout,
        };
        for (method, params) in [
            ("Page.enable", json!({})),
            ("Page.setLifecycleEventsEnabled", json!({"enabled": true})),
            ("Runtime.enable", json!({})),
            // Playwright's default: the page behaves as if its window had focus.
            (
                "Emulation.setFocusEmulationEnabled",
                json!({"enabled": true}),
            ),
        ] {
            page.command(method, params).await?;
        }
        let tree = page.command("Page.getFrameTree", json!({})).await?;
        page.main_frame = tree
            .pointer("/frameTree/frame/id")
            .and_then(Value::as_str)
            .ok_or_else(|| Error::Protocol("frame tree has no main frame".into()))?
            .to_owned();
        page.set_viewport(1280, 720, 1.0).await?;
        Ok(page)
    }

    /// Send a raw DevTools command to this page's session, with the default timeout. The escape
    /// hatch for anything the harness does not wrap yet.
    pub async fn command(&self, method: &str, params: Value) -> Result<Value, Error> {
        self.command_with_timeout(method, params, self.timeout)
            .await
    }

    pub async fn command_with_timeout(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, Error> {
        self.connection
            .call(Some(&self.session), method, params, timeout)
            .await
    }

    /// The default timeout for commands, navigation and evaluation on this page.
    pub fn set_default_timeout(&mut self, timeout: Duration) {
        self.timeout = timeout;
    }

    /// Events of `method` on this page from now on. Subscribe before triggering the event.
    pub fn subscribe(&self, method: &str) -> Subscription {
        self.state.hub.subscribe(method)
    }

    /// Emulate a `width`×`height` CSS-pixel desktop viewport. New pages start at 1280×720, scale 1,
    /// as in Playwright.
    pub async fn set_viewport(
        &self,
        width: u32,
        height: u32,
        device_scale_factor: f64,
    ) -> Result<(), Error> {
        self.command(
            "Emulation.setDeviceMetricsOverride",
            json!({
                "width": width,
                "height": height,
                "deviceScaleFactor": device_scale_factor,
                "mobile": false,
                "screenWidth": width,
                "screenHeight": height,
            }),
        )
        .await?;
        Ok(())
    }

    /// Navigate the main frame and wait for its `load` event.
    pub async fn goto(&self, url: &str) -> Result<(), Error> {
        let mut lifecycle = self.subscribe("Page.lifecycleEvent");
        let navigation = self
            .command(
                "Page.navigate",
                json!({"url": url, "frameId": self.main_frame}),
            )
            .await?;
        if let Some(Value::String(error)) = navigation.get("errorText") {
            return Err(Error::Navigation(format!("{url}: {error}")));
        }
        // A same-document navigation has no loader and fires no load event.
        let Some(loader) = navigation.get("loaderId").and_then(Value::as_str) else {
            return Ok(());
        };
        lifecycle
            .next_matching(self.timeout, |event| {
                event.get("name").and_then(Value::as_str) == Some("load")
                    && event.get("loaderId").and_then(Value::as_str) == Some(loader)
            })
            .await
            .map_err(|error| match error {
                Error::Timeout { after, .. } => Error::Timeout {
                    what: format!("load event of {url}"),
                    after,
                },
                other => other,
            })?;
        Ok(())
    }

    /// Replace the document with `html` (`document.open/write/close`, as Playwright's
    /// `setContent`) and wait for its `load` event. The HTML is a call argument, never part of
    /// script source.
    pub async fn set_content(&self, html: &str) -> Result<(), Error> {
        self.call(
            r#"(html) => {
                document.open();
                document.write(html);
                document.close();
                if (document.readyState === "complete") return;
                return new Promise((resolve) => window.addEventListener("load", () => resolve(), { once: true }));
            }"#,
            &[Value::from(html)],
        )
        .await?;
        Ok(())
    }

    /// Evaluate `expression` in the main frame, await a returned promise, and return the result
    /// as JSON (`undefined` becomes `null`). A thrown exception or rejection is an
    /// [`Error::Evaluation`].
    pub async fn evaluate(&self, expression: &str) -> Result<Value, Error> {
        self.evaluate_with_timeout(expression, self.timeout).await
    }

    pub async fn evaluate_with_timeout(
        &self,
        expression: &str,
        timeout: Duration,
    ) -> Result<Value, Error> {
        let result = self
            .command_with_timeout(
                "Runtime.evaluate",
                json!({"expression": expression, "awaitPromise": true, "returnByValue": true}),
                timeout,
            )
            .await?;
        if let Some(details) = result.get("exceptionDetails") {
            return Err(Error::Evaluation(exception_text(details)));
        }
        Ok(result
            .pointer("/result/value")
            .cloned()
            .unwrap_or(Value::Null))
    }

    /// Call the JavaScript function `function` (source text, for example `"(a, b) => a + b"`)
    /// with JSON arguments, and return its awaited result as JSON.
    pub async fn call(&self, function: &str, args: &[Value]) -> Result<Value, Error> {
        self.call_with_timeout(function, args, self.timeout).await
    }

    pub async fn call_with_timeout(
        &self,
        function: &str,
        args: &[Value],
        timeout: Duration,
    ) -> Result<Value, Error> {
        // JSON text is a JavaScript expression, so the arguments cannot change the code around
        // them.
        let args = Value::Array(args.to_vec()).to_string();
        self.evaluate_with_timeout(&format!("({function})(...{args})"), timeout)
            .await
    }

    /// Parse `source` as a classic script without running it (`Runtime.compileScript`).
    /// `Ok(None)` means it parses.
    pub async fn compile_script(
        &self,
        source: &str,
        url: &str,
    ) -> Result<Option<ScriptError>, Error> {
        let result = self
            .command(
                "Runtime.compileScript",
                json!({"expression": source, "sourceURL": url, "persistScript": false}),
            )
            .await?;
        Ok(result.get("exceptionDetails").map(|details| ScriptError {
            message: exception_text(details),
            line: details
                .get("lineNumber")
                .and_then(Value::as_u64)
                .unwrap_or(0)
                + 1,
            column: details
                .get("columnNumber")
                .and_then(Value::as_u64)
                .unwrap_or(0)
                + 1,
        }))
    }

    /// Parse `source` as an ES module without running its body. `Runtime.compileScript` only
    /// knows classic scripts, so the module is imported from a `blob:` URL with a `throw` in
    /// front of its first statement: a syntax error rejects the import before anything runs, and
    /// a module that parses stops at that `throw`. Static `import`s are resolved first, so a
    /// module with relative imports reports a resolution error here.
    pub async fn compile_module(
        &self,
        source: &str,
        url: &str,
    ) -> Result<Option<ScriptError>, Error> {
        let outcome = self
            .call(
                r#"async (source, url) => {
                    const sentinel = "__browser_tests_module_parsed__";
                    const blob = new Blob([`throw ${JSON.stringify(sentinel)};` + source + `\n//# sourceURL=${url}`], { type: "text/javascript" });
                    const href = URL.createObjectURL(blob);
                    try {
                        await import(href);
                        return { message: "the module body ran past the parse-only guard" };
                    } catch (error) {
                        if (error === sentinel) return null;
                        const where = /:(\d+):(\d+)\)?\s*$/m.exec(String(error && error.stack));
                        return { message: String(error), line: where ? Number(where[1]) : 0, column: where ? Number(where[2]) : 0 };
                    } finally {
                        URL.revokeObjectURL(href);
                    }
                }"#,
                &[Value::from(source), Value::from(url)],
            )
            .await?;
        if outcome.is_null() {
            return Ok(None);
        }
        Ok(Some(ScriptError {
            message: outcome
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("module failed to parse")
                .to_owned(),
            line: outcome.get("line").and_then(Value::as_u64).unwrap_or(0),
            column: outcome.get("column").and_then(Value::as_u64).unwrap_or(0),
        }))
    }

    /// Answer every request whose URL starts with `url_prefix` from `handler`, inside the
    /// browser (DevTools `Fetch` interception): nothing goes to the network, so this works under
    /// [`artifact_cdp::Network::LockedDown`]. Use an origin such as `https://harness.test` to give
    /// pages a secure, same-origin home. Later routes do not replace earlier ones; the first
    /// matching prefix wins.
    pub async fn route(
        &self,
        url_prefix: &str,
        handler: impl Fn(&Request) -> Response + Send + Sync + 'static,
    ) -> Result<(), Error> {
        let patterns: Vec<Value> = {
            let mut routes = lock(&self.routes);
            routes.push(Route::new(url_prefix, handler));
            routes
                .iter()
                .map(|route| json!({"urlPattern": route.pattern(), "requestStage": "Request"}))
                .collect()
        };
        self.command("Fetch.enable", json!({"patterns": patterns}))
            .await?;
        Ok(())
    }

    /// Everything the page has logged so far, except exceptions that were handled later.
    pub fn log(&self) -> Vec<LogEntry> {
        self.log_since(0)
    }

    /// A position in the log, for [`Page::log_since`].
    pub fn log_mark(&self) -> usize {
        lock(&self.state.log).len()
    }

    /// Entries recorded after `mark`.
    pub fn log_since(&self, mark: usize) -> Vec<LogEntry> {
        let mut cursor = mark;
        self.log_after(&mut cursor)
    }

    /// Entries recorded at or after `*cursor`; then moves `*cursor` to the end, in one step, so
    /// consecutive calls never miss or repeat an entry.
    pub fn log_after(&self, cursor: &mut usize) -> Vec<LogEntry> {
        let log = lock(&self.state.log);
        let entries = log
            .iter()
            .skip(*cursor)
            .filter(|logged| !logged.revoked)
            .map(|logged| logged.entry.clone())
            .collect();
        *cursor = log.len();
        entries
    }

    /// Close the tab and dispose of its browser context.
    pub async fn close(self) {
        let _ = self
            .connection
            .call(
                None,
                "Target.closeTarget",
                json!({"targetId": self.target}),
                self.timeout,
            )
            .await;
        let _ = self
            .connection
            .call(
                None,
                "Target.disposeBrowserContext",
                json!({"browserContextId": self.context}),
                self.timeout,
            )
            .await;
        self.connection.unregister_session(&self.session);
    }
}

impl Drop for Page {
    fn drop(&mut self) {
        self.events.abort();
    }
}

/// Answer the events that need a reply: serve routed requests and dismiss dialogs.
async fn answer_events(
    connection: std::sync::Weak<Connection>,
    session: String,
    routes: Arc<Mutex<Vec<Route>>>,
    mut events: mpsc::UnboundedReceiver<(String, Value)>,
    timeout: Duration,
) {
    while let Some((method, params)) = events.recv().await {
        let Some(connection) = connection.upgrade() else {
            return;
        };
        let (command, reply) = match method.as_str() {
            "Fetch.requestPaused" => fulfil(&routes, &params),
            "Page.javascriptDialogOpening" => {
                ("Page.handleJavaScriptDialog", json!({"accept": false}))
            }
            _ => continue,
        };
        // The request may already be cancelled (navigation away); nothing to do then.
        let _ = connection
            .call(Some(&session), command, reply, timeout)
            .await;
    }
}

fn fulfil(routes: &Mutex<Vec<Route>>, params: &Value) -> (&'static str, Value) {
    let request_id = params.get("requestId").cloned().unwrap_or(Value::Null);
    let url = params
        .pointer("/request/url")
        .and_then(Value::as_str)
        .unwrap_or("");
    let method = params
        .pointer("/request/method")
        .and_then(Value::as_str)
        .unwrap_or("GET");
    let handler = lock(routes)
        .iter()
        .find(|route| route.matches(url))
        .map(Route::handler);
    let Some(handler) = handler else {
        return ("Fetch.continueRequest", json!({"requestId": request_id}));
    };
    let request = Request::parse(url, method);
    let response: Response = handler(&request);
    let headers: Vec<Value> = response
        .headers
        .iter()
        .map(|(name, value)| json!({"name": name, "value": value}))
        .collect();
    (
        "Fetch.fulfillRequest",
        json!({
            "requestId": request_id,
            "responseCode": response.status,
            "responseHeaders": headers,
            "body": base64::engine::general_purpose::STANDARD.encode(&response.body),
        }),
    )
}

fn string_field(value: &Value, name: &str) -> Result<String, Error> {
    value
        .get(name)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| Error::Protocol(format!("reply has no {name}")))
}
