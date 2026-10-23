// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Neil Blackman
//! One page (a tab in its own browser context) and what tests do with it.
//!
//! A page auto-attaches its out-of-process iframes (`Target.setAutoAttach` with flat sessions):
//! their console output, exceptions, dialogs, network traffic, routes, extra headers and init
//! scripts belong to the page, as in Playwright. Locators reach into them through
//! [`Page::frame_locator`].

use std::fmt;
use std::future::{Future, IntoFuture};
use std::pin::Pin;
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use base64::Engine;
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::Error;
use crate::browser::BrowserEvents;
use crate::connection::{Connection, EventSink, Subscription, lock};
use crate::input::InputState;
use crate::network::{NetworkRequest, NetworkResponse, Waiter};
use crate::route::{
    self, Matcher, Request, Response, Route, RouteEntry, RouteHandler, fulfil_params,
};
use crate::state::{Active, PageState, exception_text};

/// Something the page reported on its own: console output, an uncaught exception, a dialog or a
/// crash. [`LogEntry::is_failure`] says whether a test should fail because of it.
#[derive(Debug, Clone, PartialEq)]
pub enum LogEntry {
    /// A `console.*` call. `level` is the DevTools type: `log`, `debug`, `info`, `warning`,
    /// `error`, `assert`, `trace` and so on. With [`Page::enable_browser_log`], browser messages
    /// (`Failed to load resource: ...`, CSP reports) are recorded here too, with the `Log` level
    /// (`error`, `warning`, `info`, `verbose`), as Playwright's `console` event reports them.
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
    /// A JavaScript dialog (`alert`, `confirm`, `prompt`, `beforeunload`) that no
    /// [`Page::on_dialog`] handler took. The harness dismissed it.
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

/// A JavaScript dialog the page opened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dialog {
    /// `alert`, `confirm`, `prompt` or `beforeunload`.
    pub kind: String,
    pub message: String,
    pub default_prompt: String,
}

/// What a [`Page::on_dialog`] handler does with a dialog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DialogAction {
    /// Press OK; for `prompt`, answer with the text (or the default prompt when `None`).
    Accept(Option<String>),
    /// Press Cancel.
    Dismiss,
}

/// One frame of the page (a snapshot).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub id: String,
    pub parent_id: Option<String>,
    /// The current URL, with its fragment.
    pub url: String,
    pub name: String,
}

/// One tab in its own browser context: fresh cookies, storage and cache. Created by
/// [`crate::Browser::new_page`].
pub struct Page {
    pub(crate) connection: Arc<Connection>,
    pub(crate) session: String,
    target: String,
    /// The browser context this page owns and disposes of on close (`None` for popups).
    context: Option<String>,
    pub(crate) main_frame: String,
    pub(crate) state: Arc<PageState>,
    browser_events: Arc<BrowserEvents>,
    events: JoinHandle<()>,
    pub(crate) timeout: Duration,
    base_url: Option<String>,
    pub(crate) user_gesture: bool,
    pub(crate) input: Mutex<InputState>,
}

fn auto_attach() -> Value {
    json!({"autoAttach": true, "waitForDebuggerOnStart": true, "flatten": true})
}

impl Page {
    pub(crate) async fn open(
        connection: Arc<Connection>,
        browser_events: Arc<BrowserEvents>,
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
        Self::attach(connection, browser_events, target, Some(context), timeout).await
    }

    /// Attach to an existing page target (a new tab, or a popup).
    pub(crate) async fn attach(
        connection: Arc<Connection>,
        browser_events: Arc<BrowserEvents>,
        target: String,
        context: Option<String>,
        timeout: Duration,
    ) -> Result<Self, Error> {
        let session = string_field(
            &connection
                .call(
                    None,
                    "Target.attachToTarget",
                    json!({"targetId": target, "flatten": true}),
                    timeout,
                )
                .await?,
            "sessionId",
        )?;
        let (active, active_events) = mpsc::unbounded_channel();
        let state = Arc::new(PageState::new(session.clone(), active));
        connection.register_session(&session, Arc::clone(&state) as Arc<dyn EventSink>);
        let events = tokio::spawn(answer_events(
            Arc::downgrade(&connection),
            Arc::downgrade(&state),
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
            browser_events,
            events,
            timeout,
            base_url: None,
            user_gesture: false,
            input: Mutex::new(InputState::default()),
        };
        for (method, params) in [
            ("Page.enable", json!({})),
            ("Page.setLifecycleEventsEnabled", json!({"enabled": true})),
            ("Runtime.enable", json!({})),
            ("Network.enable", json!({})),
            // Playwright's default: the page behaves as if its window had focus.
            (
                "Emulation.setFocusEmulationEnabled",
                json!({"enabled": true}),
            ),
            ("Target.setAutoAttach", auto_attach()),
        ] {
            page.command(method, params).await?;
        }
        let tree = page.command("Page.getFrameTree", json!({})).await?;
        let frame = &tree["frameTree"]["frame"];
        page.main_frame = frame
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(|| Error::Protocol("frame tree has no main frame".into()))?
            .to_owned();
        lock(&page.state.frames)
            .entry(page.main_frame.clone())
            .or_default()
            .url = frame
            .get("url")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        // A popup may already be waiting for the debugger; an ordinary tab ignores this.
        let _ = page
            .command("Runtime.runIfWaitingForDebugger", json!({}))
            .await;
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

    /// Send a command to one session of the page (the page or a child target).
    pub(crate) async fn session_command(
        &self,
        session: &str,
        method: &str,
        params: Value,
    ) -> Result<Value, Error> {
        self.connection
            .call(Some(session), method, params, self.timeout)
            .await
    }

    /// Send a command to every frame session (the page and its out-of-process iframes). Errors
    /// from child sessions (which may be going away) are ignored.
    async fn command_all_frames(&self, method: &str, params: Value) -> Result<(), Error> {
        for session in self.state.frame_sessions() {
            let result = self.session_command(&session, method, params.clone()).await;
            if session == self.session {
                result?;
            }
        }
        Ok(())
    }

    /// The default timeout for commands, navigation, evaluation, actions and waiters on this
    /// page.
    pub fn set_default_timeout(&mut self, timeout: Duration) {
        self.timeout = timeout;
    }

    pub fn default_timeout(&self) -> Duration {
        self.timeout
    }

    /// Resolve relative URLs in [`Page::goto`] against `url` (Playwright's `baseURL`).
    pub fn set_base_url(&mut self, url: &str) {
        self.base_url = Some(url.trim_end_matches('/').to_owned());
    }

    /// Run [`Page::evaluate`], [`Page::call`] and locator evaluations with a user gesture
    /// (transient user activation), as Playwright's `evaluate` does. Off by default.
    pub fn set_evaluate_user_gesture(&mut self, enabled: bool) {
        self.user_gesture = enabled;
    }

    fn resolve_url(&self, url: &str) -> String {
        match &self.base_url {
            Some(base) if url.starts_with('/') => format!("{base}{url}"),
            _ => url.to_owned(),
        }
    }

    /// Events of `method` on this page's own session from now on. Subscribe before triggering
    /// the event.
    pub fn subscribe(&self, method: &str) -> Subscription {
        self.state.hub.subscribe(method)
    }

    /// Events of `method` from every session of the page (the page and its out-of-process
    /// iframes), as `{"sessionId": ..., "params": ...}`.
    pub fn subscribe_all(&self, method: &str) -> Subscription {
        self.state.all.subscribe(method)
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

    /// Send these headers with every request of the page and its iframes (Playwright's
    /// `extraHTTPHeaders`). Replaces the previous set.
    pub async fn set_extra_http_headers(&self, headers: &[(&str, &str)]) -> Result<(), Error> {
        let object: serde_json::Map<String, Value> = headers
            .iter()
            .map(|(name, value)| ((*name).to_owned(), Value::from(*value)))
            .collect();
        let object = Value::Object(object);
        lock(&self.state.settings).extra_headers = Some(object.clone());
        self.command_all_frames("Network.setExtraHTTPHeaders", json!({"headers": object}))
            .await
    }

    /// Run `source` in every new document of the page and its iframes before the document's
    /// own scripts (Playwright's `addInitScript`). Affects later navigations.
    pub async fn add_init_script(&self, source: &str) -> Result<(), Error> {
        lock(&self.state.settings)
            .init_scripts
            .push(source.to_owned());
        self.command_all_frames(
            "Page.addScriptToEvaluateOnNewDocument",
            json!({"source": source}),
        )
        .await
    }

    /// Record the browser's own messages (`Failed to load resource: ...`, CSP violations,
    /// interventions) as [`LogEntry::Console`], as Playwright's `console` event does.
    pub async fn enable_browser_log(&self) -> Result<(), Error> {
        lock(&self.state.settings).browser_log = true;
        self.command_all_frames("Log.enable", json!({})).await
    }

    /// Navigate the main frame and wait for its `load` event. A path starting with `/` is
    /// resolved against [`Page::set_base_url`]. Returns the main document's response (`None` for
    /// same-document navigations and URLs without one, such as `about:blank`).
    pub async fn goto(&self, url: &str) -> Result<Option<NetworkResponse>, Error> {
        let url = self.resolve_url(url);
        crate::expect::begin_step(format!("goto {url}"));
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
            return Ok(None);
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
        Ok(lock(&self.state.network).response(loader))
    }

    /// Wait for the next document of the main frame to commit and load (or, with
    /// `same_document`, for a same-document navigation of the main frame).
    async fn wait_for_next_load(
        &self,
        lifecycle: &mut Subscription,
        same_document: Option<&mut Subscription>,
        what: &str,
    ) -> Result<Option<NetworkResponse>, Error> {
        let deadline = tokio::time::Instant::now() + self.timeout;
        let main = self.main_frame.clone();
        let is_main = |event: &Value| event.get("frameId").and_then(Value::as_str) == Some(&main);
        let is_init = |event: &Value| {
            is_main(event) && event.get("name").and_then(Value::as_str) == Some("init")
        };
        let rename = |error: Error| match error {
            Error::Timeout { after, .. } => Error::Timeout {
                what: what.to_owned(),
                after,
            },
            other => other,
        };
        let init = match same_document {
            Some(same_document) => {
                tokio::select! {
                    event = lifecycle.next_matching(self.timeout, is_init) => event.map_err(rename)?,
                    event = same_document.next_matching(self.timeout, is_main) => {
                        event.map_err(rename)?;
                        return Ok(None);
                    }
                }
            }
            None => lifecycle
                .next_matching(self.timeout, is_init)
                .await
                .map_err(rename)?,
        };
        let loader = init
            .get("loaderId")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        lifecycle
            .next_matching(remaining, |event| {
                event.get("name").and_then(Value::as_str) == Some("load")
                    && event.get("loaderId").and_then(Value::as_str) == Some(loader.as_str())
            })
            .await
            .map_err(rename)?;
        Ok(lock(&self.state.network).response(&loader))
    }

    /// Reload and wait for the `load` event.
    pub async fn reload(&self) -> Result<Option<NetworkResponse>, Error> {
        crate::expect::begin_step("reload".into());
        let mut lifecycle = self.subscribe("Page.lifecycleEvent");
        self.command("Page.reload", json!({})).await?;
        self.wait_for_next_load(&mut lifecycle, None, "load event after reload")
            .await
    }

    /// Go back in history and wait for the previous page to load (or, for a same-document
    /// entry, for the URL change). `Ok(None)` when there is nothing to go back to.
    pub async fn go_back(&self) -> Result<Option<NetworkResponse>, Error> {
        self.go_history(-1).await
    }

    /// Go forward in history; see [`Page::go_back`].
    pub async fn go_forward(&self) -> Result<Option<NetworkResponse>, Error> {
        self.go_history(1).await
    }

    async fn go_history(&self, delta: i64) -> Result<Option<NetworkResponse>, Error> {
        crate::expect::begin_step(format!("history navigation by {delta}"));
        let history = self.command("Page.getNavigationHistory", json!({})).await?;
        let current = history
            .get("currentIndex")
            .and_then(Value::as_i64)
            .unwrap_or(0);
        let Some(entry) = history
            .get("entries")
            .and_then(Value::as_array)
            .and_then(|entries| entries.get(usize::try_from(current + delta).ok()?))
        else {
            return Ok(None);
        };
        let mut lifecycle = self.subscribe("Page.lifecycleEvent");
        let mut same_document = self.subscribe("Page.navigatedWithinDocument");
        self.command(
            "Page.navigateToHistoryEntry",
            json!({"entryId": entry.get("id").cloned().unwrap_or(Value::Null)}),
        )
        .await?;
        self.wait_for_next_load(
            &mut lifecycle,
            Some(&mut same_document),
            "history navigation",
        )
        .await
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

    /// The main frame's URL.
    pub fn url(&self) -> String {
        self.frame_url(&self.main_frame).unwrap_or_default()
    }

    /// The main frame's id.
    pub fn main_frame_id(&self) -> &str {
        &self.main_frame
    }

    /// Every frame of the page, the main frame first (Playwright's `page.frames()`), including
    /// out-of-process iframes.
    pub fn frames(&self) -> Vec<Frame> {
        let frames = lock(&self.state.frames);
        let mut list: Vec<Frame> = frames
            .iter()
            .map(|(id, frame)| Frame {
                id: id.clone(),
                parent_id: frame.parent.clone(),
                url: frame.url.clone(),
                name: frame.name.clone(),
            })
            .collect();
        list.sort_by_key(|frame| frame.id != self.main_frame);
        list
    }

    /// A frame's current URL (with its fragment), or `None` once it is gone.
    pub fn frame_url(&self, frame_id: &str) -> Option<String> {
        lock(&self.state.frames)
            .get(frame_id)
            .map(|frame| frame.url.clone())
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
                json!({
                    "expression": expression,
                    "awaitPromise": true,
                    "returnByValue": true,
                    "userGesture": self.user_gesture,
                }),
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

    /// Call `function` with `args` until it returns a truthy value (Playwright's
    /// `waitForFunction`), and return that value.
    pub async fn wait_for_function(&self, function: &str, args: &[Value]) -> Result<Value, Error> {
        crate::expect::begin_step(format!("waitForFunction {function}"));
        let deadline = tokio::time::Instant::now() + self.timeout;
        loop {
            let value = self.call(function, args).await?;
            let truthy = match &value {
                Value::Null => false,
                Value::Bool(flag) => *flag,
                Value::Number(number) => number.as_f64().is_some_and(|n| n != 0.0 && !n.is_nan()),
                Value::String(text) => !text.is_empty(),
                _ => true,
            };
            if truthy {
                return Ok(value);
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(Error::Timeout {
                    what: format!("waiting for function {function}"),
                    after: self.timeout,
                });
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
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

    async fn update_fetch(&self) -> Result<(), Error> {
        let patterns = route::fetch_patterns(&lock(&self.state.routes));
        lock(&self.state.settings).fetch_patterns = patterns.clone();
        match patterns {
            Some(patterns) => {
                self.command_all_frames("Fetch.enable", json!({"patterns": patterns}))
                    .await
            }
            None => self.command_all_frames("Fetch.disable", json!({})).await,
        }
    }

    /// Answer every request whose URL starts with `url_prefix` from `handler`, inside the
    /// browser (DevTools `Fetch` interception): nothing goes to the network, so this works under
    /// [`artifact_cdp::Network::LockedDown`]. Use an origin such as `https://harness.test` to give
    /// pages a secure, same-origin home. Later prefix routes do not replace earlier ones; the
    /// first matching prefix wins.
    pub async fn route(
        &self,
        url_prefix: &str,
        handler: impl Fn(&Request) -> Response + Send + Sync + 'static,
    ) -> Result<(), Error> {
        lock(&self.state.routes).push(RouteEntry::prefix(url_prefix, handler));
        self.update_fetch().await
    }

    /// Route requests whose URL matches the Playwright glob `glob` (for example
    /// `"**/gallery/preferences*"`) to `handler`, which answers through the [`Route`] it gets
    /// (Playwright's `page.route`). The newest matching glob route runs first; a handler that
    /// calls [`Route::fallback`] passes the request to the next one.
    ///
    /// ```no_run
    /// # async fn example(page: &browser_tests::Page) -> Result<(), browser_tests::Error> {
    /// use browser_tests::Response;
    /// use serde_json::json;
    /// page.route_glob("**/collections", |route| async move {
    ///     if route.request().method == "POST" {
    ///         route.fulfill(Response::json(503, &json!({"message": "unavailable"}))).await
    ///     } else {
    ///         route.continue_request().await
    ///     }
    /// })
    /// .await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn route_glob<F, Fut>(&self, glob: &str, handler: F) -> Result<(), Error>
    where
        F: Fn(Route) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<(), Error>> + Send + 'static,
    {
        self.add_route(
            Matcher::Glob(glob.to_owned()),
            Some(glob.to_owned()),
            handler,
        )
        .await
    }

    /// Like [`Page::route_glob`], for URLs that satisfy `predicate`.
    pub async fn route_where<P, F, Fut>(&self, predicate: P, handler: F) -> Result<(), Error>
    where
        P: Fn(&str) -> bool + Send + Sync + 'static,
        F: Fn(Route) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<(), Error>> + Send + 'static,
    {
        self.add_route(Matcher::Predicate(Arc::new(predicate)), None, handler)
            .await
    }

    async fn add_route<F, Fut>(
        &self,
        matcher: Matcher,
        key: Option<String>,
        handler: F,
    ) -> Result<(), Error>
    where
        F: Fn(Route) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<(), Error>> + Send + 'static,
    {
        let handler: route::AsyncHandler = Arc::new(move |route| {
            let future = handler(route);
            Box::pin(async move {
                if let Err(error) = future.await {
                    eprintln!("[browser-tests] route handler failed: {error}");
                }
            })
        });
        lock(&self.state.routes).push(RouteEntry {
            matcher,
            handler: RouteHandler::Async(handler),
            key,
        });
        self.update_fetch().await
    }

    /// Remove the [`Page::route_glob`] routes registered with exactly `glob`.
    pub async fn unroute(&self, glob: &str) -> Result<(), Error> {
        lock(&self.state.routes).retain(|route| route.key.as_deref() != Some(glob));
        self.update_fetch().await
    }

    /// Remove every route.
    pub async fn unroute_all(&self) -> Result<(), Error> {
        lock(&self.state.routes).clear();
        self.update_fetch().await
    }

    /// Decide what happens to the page's dialogs (`alert`, `confirm`, `prompt`,
    /// `beforeunload`), in the page and its iframes. Without a handler they are dismissed and
    /// logged as [`LogEntry::Dialog`] failures. Every dialog is listed in [`Page::dialogs`].
    pub fn on_dialog(&self, handler: impl Fn(&Dialog) -> DialogAction + Send + Sync + 'static) {
        *lock(&self.state.dialog_handler) = Some(Arc::new(handler));
    }

    /// Every dialog the page opened so far.
    pub fn dialogs(&self) -> Vec<Dialog> {
        lock(&self.state.dialogs).clone()
    }

    /// Wait for the next request that satisfies `predicate`, from any frame of the page. Create
    /// the waiter before the action, then `.await` it.
    pub fn wait_for_request(
        &self,
        predicate: impl Fn(&NetworkRequest) -> bool + Send + 'static,
    ) -> Waiter<NetworkRequest> {
        Waiter::new(
            self.state.requests.subscribe(),
            Box::new(predicate),
            self.timeout,
            "request",
        )
    }

    /// Wait for the next response that satisfies `predicate`, from any frame of the page
    /// (Playwright's `waitForResponse`). Create the waiter before the action, then `.await` it.
    pub fn wait_for_response(
        &self,
        predicate: impl Fn(&NetworkResponse) -> bool + Send + 'static,
    ) -> Waiter<NetworkResponse> {
        Waiter::new(
            self.state.responses.subscribe(),
            Box::new(predicate),
            self.timeout,
            "response",
        )
    }

    /// Every request the page and its frames sent so far, in order (what a Playwright
    /// `page.on("request")` listener attached at page creation collects).
    pub fn requests(&self) -> Vec<NetworkRequest> {
        lock(&self.state.network).history()
    }

    /// The response body, once it has finished loading.
    pub async fn response_body(&self, response: &NetworkResponse) -> Result<Vec<u8>, Error> {
        let deadline = tokio::time::Instant::now() + self.timeout;
        loop {
            let finished = lock(&self.state.network).finished(&response.id);
            match finished {
                Some(Ok(())) => break,
                Some(Err(error)) => {
                    return Err(Error::Navigation(format!("{}: {error}", response.url)));
                }
                None => {}
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(Error::Timeout {
                    what: format!("body of {}", response.url),
                    after: self.timeout,
                });
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let body = self
            .session_command(
                &response.session,
                "Network.getResponseBody",
                json!({"requestId": response.id}),
            )
            .await?;
        let text = body.get("body").and_then(Value::as_str).unwrap_or("");
        if body.get("base64Encoded").and_then(Value::as_bool) == Some(true) {
            base64::engine::general_purpose::STANDARD
                .decode(text)
                .map_err(|error| Error::Protocol(format!("response body: {error}")))
        } else {
            Ok(text.as_bytes().to_vec())
        }
    }

    /// The response body as UTF-8 text (Playwright's `response.text()`).
    pub async fn response_text(&self, response: &NetworkResponse) -> Result<String, Error> {
        let body = self.response_body(response).await?;
        String::from_utf8(body).map_err(|error| Error::Protocol(format!("response body: {error}")))
    }

    /// Every header that went on the wire for `request` (Playwright's `allHeaders()`), names in
    /// lower case: [`NetworkRequest::headers`] plus what the network stack added. Waits up to a
    /// second for the browser to report them.
    pub async fn all_request_headers(&self, request: &NetworkRequest) -> Vec<(String, String)> {
        for _ in 0..50 {
            let headers = lock(&self.state.network).extra_headers(&request.id);
            if let Some(headers) = headers {
                return headers;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        request.headers.clone()
    }

    /// Wait for a popup (a page this page opens with `window.open` or a link with a target).
    /// Create the waiter before the action, then `.await` it. The popup's first request may go out
    /// before the harness attaches, so extra headers and routes apply from its second request on.
    pub fn wait_for_popup(&self) -> PopupWaiter {
        PopupWaiter {
            created: self.browser_events.hub.subscribe("Target.targetCreated"),
            opener: self.target.clone(),
            connection: Arc::clone(&self.connection),
            browser_events: Arc::clone(&self.browser_events),
            timeout: self.timeout,
        }
    }

    /// A PNG screenshot of the viewport.
    pub async fn screenshot(&self) -> Result<Vec<u8>, Error> {
        let shot = self
            .command("Page.captureScreenshot", json!({"format": "png"}))
            .await?;
        base64::engine::general_purpose::STANDARD
            .decode(shot.get("data").and_then(Value::as_str).unwrap_or(""))
            .map_err(|error| Error::Protocol(format!("screenshot: {error}")))
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

    /// Uncaught exceptions so far (Playwright's `pageerror` events), as text.
    pub fn page_errors(&self) -> Vec<String> {
        self.log()
            .into_iter()
            .filter_map(|entry| match entry {
                LogEntry::Exception { text, .. } => Some(text),
                _ => None,
            })
            .collect()
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
        if let Some(context) = &self.context {
            let _ = self
                .connection
                .call(
                    None,
                    "Target.disposeBrowserContext",
                    json!({"browserContextId": context}),
                    self.timeout,
                )
                .await;
        }
        self.connection.unregister_session(&self.session);
    }
}

impl Drop for Page {
    fn drop(&mut self) {
        self.events.abort();
    }
}

/// Waits for a popup; see [`Page::wait_for_popup`].
pub struct PopupWaiter {
    created: Subscription,
    opener: String,
    connection: Arc<Connection>,
    browser_events: Arc<BrowserEvents>,
    timeout: Duration,
}

impl PopupWaiter {
    pub async fn wait(mut self) -> Result<Page, Error> {
        let opener = self.opener.clone();
        let event = self
            .created
            .next_matching(self.timeout, |event| {
                event
                    .pointer("/targetInfo/openerId")
                    .and_then(Value::as_str)
                    == Some(opener.as_str())
                    && event.pointer("/targetInfo/type").and_then(Value::as_str) == Some("page")
            })
            .await?;
        let target = event
            .pointer("/targetInfo/targetId")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        Page::attach(
            self.connection,
            self.browser_events,
            target,
            None,
            self.timeout,
        )
        .await
    }
}

impl IntoFuture for PopupWaiter {
    type Output = Result<Page, Error>;
    type IntoFuture = Pin<Box<dyn Future<Output = Self::Output> + Send>>;

    fn into_future(self) -> Self::IntoFuture {
        Box::pin(self.wait())
    }
}

/// Answer the events that need a reply: serve routed requests, handle dialogs and set up child
/// targets. Route handlers and child setup run on their own tasks, so a held route never blocks
/// the others.
async fn answer_events(
    connection: Weak<Connection>,
    state: Weak<PageState>,
    mut events: mpsc::UnboundedReceiver<Active>,
    timeout: Duration,
) {
    while let Some(event) = events.recv().await {
        let (Some(live_connection), Some(live_state)) = (connection.upgrade(), state.upgrade())
        else {
            return;
        };
        match event {
            Active::Paused { session, params } => {
                tokio::spawn(on_paused(
                    connection.clone(),
                    state.clone(),
                    session,
                    params,
                    timeout,
                ));
            }
            Active::Dialog { session, params } => {
                let text = |name: &str| params[name].as_str().unwrap_or("").to_owned();
                let dialog = Dialog {
                    kind: text("type"),
                    message: text("message"),
                    default_prompt: text("defaultPrompt"),
                };
                let handler = lock(&live_state.dialog_handler).clone();
                let reply = match handler.map(|handler| handler(&dialog)) {
                    Some(DialogAction::Accept(text)) => json!({
                        "accept": true,
                        "promptText": text.unwrap_or(dialog.default_prompt),
                    }),
                    Some(DialogAction::Dismiss) | None => json!({"accept": false}),
                };
                let _ = live_connection
                    .call(
                        Some(&session),
                        "Page.handleJavaScriptDialog",
                        reply,
                        timeout,
                    )
                    .await;
            }
            Active::Attached { session, kind } => {
                tokio::spawn(set_up_child(
                    live_connection,
                    live_state,
                    session,
                    kind,
                    timeout,
                ));
            }
        }
    }
}

/// Give an auto-attached child the page's settings, then let it run.
async fn set_up_child(
    connection: Arc<Connection>,
    state: Arc<PageState>,
    session: String,
    kind: String,
    timeout: Duration,
) {
    let mut commands = Vec::new();
    if kind == "iframe" || kind == "page" {
        let settings = lock(&state.settings).clone();
        commands.extend([
            ("Page.enable", json!({})),
            ("Page.setLifecycleEventsEnabled", json!({"enabled": true})),
            ("Runtime.enable", json!({})),
            ("Network.enable", json!({})),
        ]);
        if let Some(headers) = settings.extra_headers {
            commands.push(("Network.setExtraHTTPHeaders", json!({"headers": headers})));
        }
        if settings.browser_log {
            commands.push(("Log.enable", json!({})));
        }
        for source in settings.init_scripts {
            commands.push((
                "Page.addScriptToEvaluateOnNewDocument",
                json!({"source": source}),
            ));
        }
        if let Some(patterns) = settings.fetch_patterns {
            commands.push(("Fetch.enable", json!({"patterns": patterns})));
        }
        commands.push(("Target.setAutoAttach", auto_attach()));
    }
    drop(state);
    commands.push(("Runtime.runIfWaitingForDebugger", json!({})));
    for (method, params) in commands {
        // The child may be gone already; nothing to do then.
        let _ = connection
            .call(Some(&session), method, params, timeout)
            .await;
    }
}

async fn on_paused(
    connection: Weak<Connection>,
    state: Weak<PageState>,
    session: String,
    params: Value,
    timeout: Duration,
) {
    let Some(live_connection) = connection.upgrade() else {
        return;
    };
    let request_id = params
        .get("requestId")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    let send = |method: &'static str, reply: Value| {
        let connection = Arc::clone(&live_connection);
        let session = session.clone();
        async move {
            // The request may already be cancelled (navigation away); nothing to do then.
            let _ = connection
                .call(Some(&session), method, reply, timeout)
                .await;
        }
    };
    let (waiter, candidates, request) = {
        let Some(live_state) = state.upgrade() else {
            return;
        };
        // The response stage of a request a route continued with `Route::fetch`.
        if params.get("responseStatusCode").is_some() || params.get("responseErrorReason").is_some()
        {
            (
                Some(lock(&live_state.response_stage).remove(&request_id)),
                Vec::new(),
                None,
            )
        } else {
            let request = Request::from_paused(&params);
            let candidates = route::candidates(&lock(&live_state.routes), &request.url);
            (None, candidates, Some(request))
        }
    };
    match (waiter, request) {
        (Some(Some(waiter)), _) => {
            let _ = waiter.send((session.clone(), params));
            return;
        }
        (Some(None), _) | (_, None) => {
            send("Fetch.continueRequest", json!({"requestId": request_id})).await;
            return;
        }
        (None, Some(request)) => {
            for handler in candidates {
                match handler {
                    RouteHandler::Fulfil(handler) => {
                        let response = handler(&request);
                        send(
                            "Fetch.fulfillRequest",
                            fulfil_params(&request_id, &response),
                        )
                        .await;
                        return;
                    }
                    RouteHandler::Async(handler) => {
                        let (route, outcome) = Route::new(
                            request.clone(),
                            request_id.clone(),
                            session.clone(),
                            connection.clone(),
                            state.clone(),
                            timeout,
                        );
                        handler(route).await;
                        let outcome = *lock(&outcome);
                        match outcome {
                            route::Outcome::Handled => return,
                            route::Outcome::Fallback => continue,
                            route::Outcome::Pending => break,
                        }
                    }
                }
            }
        }
    }
    send("Fetch.continueRequest", json!({"requestId": request_id})).await;
}

fn string_field(value: &Value, name: &str) -> Result<String, Error> {
    value
        .get(name)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| Error::Protocol(format!("reply has no {name}")))
}
