// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Neil Blackman
//! One render: launch a fresh browser, load hostile HTML into a locked-down page, take a PNG
//! screenshot, then kill the browser's whole process group.
//!
//! The DevTools sequence mirrors what Playwright 1.60 sent for the Node renderer's
//! `browser.newContext({viewport, deviceScaleFactor: 2, acceptDownloads: false,
//! serviceWorkers: "block", offline: true})`, `context.route("**/*", …)`, `page.setContent(html,
//! {waitUntil: "load"})` and `page.screenshot({animations: "disabled", caret: "hide", clip})`.
//! Differences are deliberate and stricter:
//!
//! * Request interception is enabled on the browser target as well as on the page, so requests
//!   from popups, workers and out-of-process frames are also paused and failed.
//! * Popups are closed while still paused before their first navigation (Playwright let them
//!   start, routed their requests, then closed them).
//! * Workers, service workers and shared workers stay paused before their first statement and
//!   never run (Playwright resumed dedicated workers).
//! * Launch flags route all browser traffic to an unreachable proxy and disable DNS
//!   (see [`crate::browser`] and `artifact_cdp::LOCKDOWN_SWITCHES`).

use std::collections::HashMap;
use std::fmt;
use std::io;
use std::path::PathBuf;
use std::time::Duration;

use base64::Engine;
use serde_json::{Value, json};

use artifact_cdp::{CdpError, DEFAULT_MAX_MESSAGE_BYTES, Incoming, Pipe, encode_command};

use crate::browser::{Browser, Hardening};
use crate::config::DEVICE_SCALE_FACTOR;
use crate::image_info::PNG_SIGNATURE;

/// Name of the isolated JavaScript world the renderer evaluates its own scripts in. Page scripts
/// run in the main world and cannot see or patch the objects in this one.
const UTILITY_WORLD: &str = "__artifact_preview_utility__";

/// Playwright's `frame.setContent`: replace the document with the HTML, passed as a protocol
/// argument and never spliced into script source.
const SET_CONTENT: &str =
    "function (html) { document.open(); document.write(html); document.close(); }";

/// Playwright's `serviceWorkers: "block"` init script, hardened: the replacement is installed on
/// the prototype as a non-writable, non-configurable property, so the page cannot restore the
/// original through `ServiceWorkerContainer.prototype`. Like Playwright's, it resolves to
/// `undefined`. Registration would also fail on its own: the page has an opaque origin, every
/// request is blocked, and service worker targets are never resumed.
const BLOCK_SERVICE_WORKERS: &str = r#"(() => {
  if (typeof ServiceWorkerContainer === "undefined") return;
  Object.defineProperty(ServiceWorkerContainer.prototype, "register", {
    value: async () => { console.warn("Service Worker registration blocked by artifact-preview"); },
    writable: false,
    configurable: false,
  });
})();"#;

/// Playwright's `inPagePrepareForScreenshots(undefined, hideCaret = true, disableAnimations =
/// true, syncAnimations = false)`, without the cleanup hook (the browser is killed instead).
/// Finite animations jump to their end state, infinite ones are cancelled, and the caret of every
/// editable element, including inside shadow roots, becomes transparent. A blanket
/// `animation: none` stylesheet would differ from the Node renderer: an element that fades in
/// with `animation-fill-mode: forwards` would stay invisible.
const PREPARE_FOR_SCREENSHOT: &str = r#"function () {
  const collectRoots = (root, roots = []) => {
    roots.push(root);
    const walker = document.createTreeWalker(root, NodeFilter.SHOW_ELEMENT);
    do {
      const node = walker.currentNode;
      const shadowRoot = node instanceof Element ? node.shadowRoot : null;
      if (shadowRoot) collectRoots(shadowRoot, roots);
    } while (walker.nextNode());
    return roots;
  };
  const roots = collectRoots(document);
  for (const root of roots) {
    root.querySelectorAll("input,textarea,[contenteditable]").forEach((element) => {
      element.style.setProperty("caret-color", "transparent", "important");
    });
  }
  const cancelled = new Set();
  const handleAnimations = (root) => {
    for (const animation of root.getAnimations()) {
      if (!animation.effect || animation.playbackRate === 0 || cancelled.has(animation)) continue;
      const endTime = animation.effect.getComputedTiming().endTime;
      if (Number.isFinite(endTime)) {
        try { animation.finish(); } catch (e) {}
      } else {
        try { animation.cancel(); cancelled.add(animation); } catch (e) {}
      }
    }
  };
  for (const root of roots) {
    const handleRootAnimations = handleAnimations.bind(null, root);
    handleRootAnimations();
    root.addEventListener("transitionrun", handleRootAnimations);
    root.addEventListener("animationstart", handleRootAnimations);
  }
}"#;

/// Playwright waits for `document.fonts.ready` in the utility world before every screenshot.
const FONTS_READY: &str = "function () { return document.fonts.ready.then(() => undefined); }";

/// Resolves after the next animation frame.
const NEXT_FRAME: &str =
    "function () { return new Promise((resolve) => requestAnimationFrame(() => resolve())); }";

/// Extra `Page.captureScreenshot` attempts after a protocol error.
const CAPTURE_RETRIES: u32 = 2;

#[derive(Debug)]
pub enum RenderError {
    /// The temporary directory or the browser process could not be created.
    Launch(io::Error),
    /// The wall-clock deadline passed.
    TimedOut,
    Protocol(String),
    /// The screenshot was not a PNG.
    InvalidImage,
}

impl fmt::Display for RenderError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Launch(error) => write!(formatter, "browser launch failed: {error}"),
            Self::TimedOut => formatter.write_str("render timed out"),
            Self::Protocol(detail) => write!(formatter, "render failed: {detail}"),
            Self::InvalidImage => formatter.write_str("screenshot is not a PNG"),
        }
    }
}

impl std::error::Error for RenderError {}

/// Renders HTML with a fresh browser per call. One `Renderer` serves the whole process; the HTTP
/// layer guarantees at most one render at a time.
#[derive(Debug, Clone)]
pub struct Renderer {
    executable: PathBuf,
    timeout: Duration,
    hardening: Hardening,
    max_message_bytes: usize,
    /// Extra time to keep the loaded page alive before the screenshot. Always zero in production;
    /// tests raise it so slow network attempts get the chance to reach a canary.
    settle: Duration,
    /// `CHROME_SANDBOX`: launch without `--no-sandbox`.
    chrome_sandbox: bool,
}

impl Renderer {
    /// A renderer with Chromium's own sandbox off, as before `CHROME_SANDBOX` existed.
    pub fn new(executable: PathBuf, timeout: Duration) -> Self {
        Self {
            executable,
            timeout,
            hardening: Hardening::FULL,
            max_message_bytes: DEFAULT_MAX_MESSAGE_BYTES,
            settle: Duration::ZERO,
            chrome_sandbox: false,
        }
    }

    /// Keep Chromium's own sandbox (`CHROME_SANDBOX`): every launch leaves out `--no-sandbox`.
    #[must_use]
    pub fn with_chrome_sandbox(mut self, enabled: bool) -> Self {
        self.chrome_sandbox = enabled;
        self
    }

    #[cfg(test)]
    pub(crate) fn with_hardening(mut self, hardening: Hardening) -> Self {
        self.hardening = hardening;
        self
    }

    #[cfg(test)]
    pub(crate) fn with_settle(mut self, settle: Duration) -> Self {
        self.settle = settle;
        self
    }

    /// Render `html` at `width`×`height` CSS pixels. The PNG is `2·width`×`2·height` pixels.
    ///
    /// The deadline covers launch, load and screenshot. Whatever the outcome, the browser's whole
    /// process group is killed and its temporary directory removed before this returns.
    pub async fn render(
        &self,
        html: &str,
        width: u32,
        height: u32,
    ) -> Result<Vec<u8>, RenderError> {
        self.render_inspecting(html, width, height, |_| {}).await
    }

    /// [`Renderer::render`], handing the live browser to `inspect` just before the shutdown.
    /// Tests use it to look at the process group.
    pub(crate) async fn render_inspecting(
        &self,
        html: &str,
        width: u32,
        height: u32,
        inspect: impl FnOnce(&Browser),
    ) -> Result<Vec<u8>, RenderError> {
        let started = tokio::time::Instant::now();
        let (browser, mut pipe) = Browser::launch(
            &self.executable,
            &self
                .hardening
                .launch_options(self.max_message_bytes, self.chrome_sandbox),
        )
        .map_err(RenderError::Launch)?;
        let remaining = self.timeout.saturating_sub(started.elapsed());
        let outcome = tokio::time::timeout(
            remaining,
            drive(&mut pipe, self.hardening, self.settle, html, width, height),
        )
        .await;
        inspect(&browser);
        browser.shutdown().await;
        // The pipe outlives the kill, as it did when the browser owned it.
        drop(pipe);
        match outcome {
            Err(_) => Err(RenderError::TimedOut),
            Ok(Err(DriveError::Cdp(error))) => Err(RenderError::Protocol(error.to_string())),
            Ok(Err(DriveError::InvalidImage)) => Err(RenderError::InvalidImage),
            Ok(Ok(png)) => Ok(png),
        }
    }
}

#[derive(Debug)]
enum DriveError {
    Cdp(CdpError),
    InvalidImage,
}

impl From<CdpError> for DriveError {
    fn from(error: CdpError) -> Self {
        Self::Cdp(error)
    }
}

fn fetch_patterns() -> Value {
    json!({"patterns": [{"urlPattern": "*", "requestStage": "Request"}]})
}

fn offline_conditions() -> Value {
    json!({"offline": true, "latency": 0, "downloadThroughput": -1, "uploadThroughput": -1})
}

fn auto_attach() -> Value {
    json!({"autoAttach": true, "waitForDebuggerOnStart": true, "flatten": true})
}

async fn drive(
    pipe: &mut Pipe,
    hardening: Hardening,
    settle: Duration,
    html: &str,
    width: u32,
    height: u32,
) -> Result<Vec<u8>, DriveError> {
    let mut driver = Driver::new(pipe, hardening);

    // Browser target. Auto-attach pauses every new page before it runs, so popups can be closed
    // before their first request. Interception here catches requests from any target.
    driver
        .call(None, "Target.setAutoAttach", auto_attach())
        .await?;
    if hardening.intercept_requests {
        driver.call(None, "Fetch.enable", fetch_patterns()).await?;
    }
    let context = driver
        .call(
            None,
            "Target.createBrowserContext",
            json!({"disposeOnDetach": true}),
        )
        .await?;
    let context_id = string_field(&context, "browserContextId")?;
    driver
        .call(
            None,
            "Browser.setDownloadBehavior",
            json!({"behavior": "deny", "browserContextId": context_id}),
        )
        .await?;
    let target = driver
        .call(
            None,
            "Target.createTarget",
            json!({"url": "about:blank", "browserContextId": context_id}),
        )
        .await?;
    driver
        .claim_main_target(string_field(&target, "targetId")?)
        .await?;
    driver
        .wait_until(|driver| driver.main_session.is_some())
        .await?;
    let session = driver.main_session.clone().expect("checked by wait_until");
    let page = Some(session.as_str());

    // Page target, in the order of Playwright's FrameSession._initialize.
    driver.call(page, "Page.enable", json!({})).await?;
    let tree = driver.call(page, "Page.getFrameTree", json!({})).await?;
    let main_frame = tree
        .pointer("/frameTree/frame/id")
        .and_then(Value::as_str)
        .ok_or_else(|| CdpError::Malformed("frame tree has no main frame".into()))?
        .to_owned();
    driver.main_frame = Some(main_frame.clone());
    driver
        .call(
            page,
            "Page.setLifecycleEventsEnabled",
            json!({"enabled": true}),
        )
        .await?;
    let world = driver.isolated_world(page, &main_frame).await?;
    driver
        .call(
            page,
            "Page.addScriptToEvaluateOnNewDocument",
            json!({"source": BLOCK_SERVICE_WORKERS, "runImmediately": true}),
        )
        .await?;
    if hardening.intercept_requests {
        driver.call(page, "Fetch.enable", fetch_patterns()).await?;
    }
    if hardening.offline {
        // Playwright enables the Network domain on every session. With it enabled, offline
        // emulation also fails WebSocket handshakes and WebRTC STUN, not only HTTP requests.
        driver.call(page, "Network.enable", json!({})).await?;
        driver
            .call(
                page,
                "Network.emulateNetworkConditions",
                offline_conditions(),
            )
            .await?;
    }
    driver
        .call(page, "Target.setAutoAttach", auto_attach())
        .await?;
    // Playwright sizes the window to the viewport when the target has one.
    match driver
        .call(page, "Browser.getWindowForTarget", json!({}))
        .await
    {
        Ok(window) => {
            if let Some(window_id) = window.get("windowId") {
                let bounds =
                    json!({"windowId": window_id, "bounds": {"width": width, "height": height}});
                driver
                    .call_ignoring_command_errors(page, "Browser.setWindowBounds", bounds)
                    .await?;
            }
        }
        Err(CdpError::Command { .. }) => {}
        Err(error) => return Err(error.into()),
    }
    driver
        .call(
            page,
            "Emulation.setFocusEmulationEnabled",
            json!({"enabled": true}),
        )
        .await?;
    driver
        .call(
            page,
            "Emulation.setDeviceMetricsOverride",
            json!({
                "mobile": false,
                "width": width,
                "height": height,
                "screenWidth": width,
                "screenHeight": height,
                "deviceScaleFactor": DEVICE_SCALE_FACTOR,
                "screenOrientation": {"angle": 0, "type": "landscapePrimary"},
            }),
        )
        .await?;
    // Playwright's default Linux font families for headless Chromium.
    driver
        .call(
            page,
            "Page.setFontFamilies",
            json!({"fontFamilies": {
                "standard": "Times New Roman",
                "fixed": "Monospace",
                "serif": "Times New Roman",
                "sansSerif": "Arial",
                "cursive": "Comic Sans MS",
                "fantasy": "Impact",
            }}),
        )
        .await?;
    // Playwright's default media emulation for a new context.
    driver
        .call(
            page,
            "Emulation.setEmulatedMedia",
            json!({"media": "", "features": [
                {"name": "prefers-color-scheme", "value": "light"},
                {"name": "prefers-reduced-motion", "value": "no-preference"},
                {"name": "forced-colors", "value": "none"},
                {"name": "prefers-contrast", "value": "no-preference"},
            ]}),
        )
        .await?;
    driver
        .call(page, "Runtime.runIfWaitingForDebugger", json!({}))
        .await?;

    // The initial about:blank must have loaded, or its late `load` could be mistaken for the
    // content's. Lifecycle events already reached are replayed when they are enabled.
    driver.wait_until(|driver| driver.main_loaded).await?;

    // page.setContent(html, {waitUntil: "load"}).
    driver.main_loaded = false;
    let written = driver
        .call(
            page,
            "Runtime.callFunctionOn",
            json!({
                "functionDeclaration": SET_CONTENT,
                "executionContextId": world,
                "arguments": [{"value": html}],
                "returnByValue": true,
                "awaitPromise": false,
            }),
        )
        .await?;
    if let Some(details) = written.get("exceptionDetails") {
        return Err(CdpError::Command {
            method: "setContent".into(),
            message: details.to_string(),
        }
        .into());
    }
    driver.wait_until(|driver| driver.main_loaded).await?;
    if !settle.is_zero() {
        // Keep applying the event policy while the page idles.
        if let Ok(result) = tokio::time::timeout(settle, driver.wait_until(|_| false)).await {
            result?;
        }
    }

    // page.screenshot({animations: "disabled", caret: "hide"}): prepare every frame, then wait
    // for fonts. Failures in individual frames are ignored, as in Playwright.
    let tree = driver.call(page, "Page.getFrameTree", json!({})).await?;
    let mut frames = Vec::new();
    collect_frame_ids(tree.get("frameTree"), &mut frames);
    for frame in frames {
        let context = if frame == main_frame {
            Some(world)
        } else {
            match driver.isolated_world(page, &frame).await {
                Ok(context) => Some(context),
                Err(CdpError::Command { .. } | CdpError::Malformed(_)) => None,
                Err(error) => return Err(error.into()),
            }
        };
        if let Some(context) = context {
            let prepare = json!({
                "functionDeclaration": PREPARE_FOR_SCREENSHOT,
                "executionContextId": context,
                "returnByValue": true,
            });
            driver
                .call_ignoring_command_errors(page, "Runtime.callFunctionOn", prepare)
                .await?;
        }
    }
    let fonts = json!({
        "functionDeclaration": FONTS_READY,
        "executionContextId": world,
        "awaitPromise": true,
        "returnByValue": true,
    });
    driver
        .call_ignoring_command_errors(page, "Runtime.callFunctionOn", fonts)
        .await?;

    // Playwright's CRPage.takeScreenshot for a viewport clip of {0, 0, width, height}.
    let metrics = driver
        .call(page, "Page.getLayoutMetrics", json!({}))
        .await?;
    let clip = screenshot_clip(&metrics, width, height);
    let capture = json!({"format": "png", "clip": clip, "captureBeyondViewport": false});
    let mut retries = 0;
    let shot = loop {
        match driver
            .call(page, "Page.captureScreenshot", capture.clone())
            .await
        {
            Ok(shot) => break shot,
            // Chromium occasionally answers "Unable to capture screenshot" when no compositor
            // frame is ready yet (about 1 in 100 renders with Chrome 148). Wait for the next
            // frame and try again, within the same deadline.
            Err(error @ CdpError::Command { .. }) if retries < CAPTURE_RETRIES => {
                retries += 1;
                eprintln!("[artifact-preview] retrying screenshot: {error}");
                let next_frame = json!({
                    "functionDeclaration": NEXT_FRAME,
                    "executionContextId": world,
                    "awaitPromise": true,
                    "returnByValue": true,
                });
                driver
                    .call_ignoring_command_errors(page, "Runtime.callFunctionOn", next_frame)
                    .await?;
            }
            Err(error) => return Err(error.into()),
        }
    };
    let data = shot
        .get("data")
        .and_then(Value::as_str)
        .ok_or(DriveError::InvalidImage)?;
    let png = base64::engine::general_purpose::STANDARD
        .decode(data)
        .map_err(|_| DriveError::InvalidImage)?;
    if !png.starts_with(&PNG_SIGNATURE) {
        return Err(DriveError::InvalidImage);
    }
    Ok(png)
}

/// The clip Playwright computes from `Page.getLayoutMetrics`: the visible viewport at the current
/// scroll position, at the visual viewport's scale.
fn screenshot_clip(metrics: &Value, width: u32, height: u32) -> Value {
    let viewport = metrics
        .get("visualViewport")
        .or_else(|| metrics.get("cssVisualViewport"));
    let field = |name: &str| {
        viewport
            .and_then(|viewport| viewport.get(name))
            .and_then(Value::as_f64)
    };
    let page_x = field("pageX").unwrap_or(0.0);
    let page_y = field("pageY").unwrap_or(0.0);
    let scale = field("scale")
        .filter(|scale| scale.is_finite() && *scale > 0.0)
        .unwrap_or(1.0);
    // helper.enclosingIntSize
    let clip_width = (f64::from(width) / scale + 1e-3).floor();
    let clip_height = (f64::from(height) / scale + 1e-3).floor();
    json!({"x": page_x, "y": page_y, "width": clip_width, "height": clip_height, "scale": scale})
}

fn collect_frame_ids(tree: Option<&Value>, frames: &mut Vec<String>) {
    let Some(tree) = tree else { return };
    if let Some(id) = tree.pointer("/frame/id").and_then(Value::as_str) {
        frames.push(id.to_owned());
    }
    if let Some(children) = tree.get("childFrames").and_then(Value::as_array) {
        for child in children {
            collect_frame_ids(Some(child), frames);
        }
    }
}

fn string_field(value: &Value, name: &str) -> Result<String, CdpError> {
    value
        .get(name)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| CdpError::Malformed(format!("reply has no {name}")))
}

/// What to do with a paused child target once request interception is active on it.
enum AfterAck {
    /// Let it run (an out-of-process iframe).
    Resume { session: String },
    /// Let it run, then close it (a popup).
    ResumeAndClose { session: String, target: String },
}

/// Protocol state for one render. Every incoming message passes through [`Driver::pump`], which
/// applies the security policy to events immediately, whatever command is in flight: a paused
/// request or an open `alert()` would otherwise block the very command being awaited.
struct Driver<'p> {
    pipe: &'p mut Pipe,
    hardening: Hardening,
    next_id: u64,
    main_target: Option<String>,
    main_session: Option<String>,
    main_frame: Option<String>,
    /// Page targets attached before `Target.createTarget` returned: (target id, session id).
    unclaimed_pages: Vec<(String, String)>,
    /// Set by the main frame's `load` lifecycle event.
    main_loaded: bool,
    /// The page crashed or detached; every further wait fails.
    main_gone: bool,
    /// Work to do when a command is acknowledged: command id → follow-up for a child target.
    after_ack: HashMap<u64, AfterAck>,
}

impl<'p> Driver<'p> {
    fn new(pipe: &'p mut Pipe, hardening: Hardening) -> Self {
        Self {
            pipe,
            hardening,
            next_id: 0,
            main_target: None,
            main_session: None,
            main_frame: None,
            unclaimed_pages: Vec::new(),
            main_loaded: false,
            main_gone: false,
            after_ack: HashMap::new(),
        }
    }

    async fn send(
        &mut self,
        session: Option<&str>,
        method: &str,
        params: Value,
    ) -> Result<u64, CdpError> {
        self.next_id += 1;
        let id = self.next_id;
        self.pipe
            .send(&encode_command(id, session, method, params))
            .await?;
        Ok(id)
    }

    async fn call(
        &mut self,
        session: Option<&str>,
        method: &str,
        params: Value,
    ) -> Result<Value, CdpError> {
        let id = self.send(session, method, params).await?;
        loop {
            if let Some((reply_id, result)) = self.pump().await?
                && reply_id == id
            {
                return result.map_err(|message| CdpError::Command {
                    method: method.to_owned(),
                    message,
                });
            }
        }
    }

    async fn call_ignoring_command_errors(
        &mut self,
        session: Option<&str>,
        method: &str,
        params: Value,
    ) -> Result<(), CdpError> {
        match self.call(session, method, params).await {
            Ok(_) | Err(CdpError::Command { .. }) => Ok(()),
            Err(error) => Err(error),
        }
    }

    async fn isolated_world(
        &mut self,
        session: Option<&str>,
        frame: &str,
    ) -> Result<i64, CdpError> {
        let world = self
            .call(
                session,
                "Page.createIsolatedWorld",
                json!({"frameId": frame, "worldName": UTILITY_WORLD, "grantUniveralAccess": true}),
            )
            .await?;
        world
            .get("executionContextId")
            .and_then(Value::as_i64)
            .ok_or_else(|| CdpError::Malformed("isolated world has no execution context".into()))
    }

    async fn wait_until(&mut self, condition: impl Fn(&Self) -> bool) -> Result<(), CdpError> {
        while !condition(self) {
            self.pump().await?;
        }
        Ok(())
    }

    /// Read one message. Events are handled here; a reply is returned unless it was internal.
    async fn pump(&mut self) -> Result<Option<(u64, Result<Value, String>)>, CdpError> {
        let reply = match self.pipe.recv().await? {
            Incoming::Response { id, result } => match self.after_ack.remove(&id) {
                Some(follow_up) => {
                    // Interception is active on the child target (or could not be enabled, in
                    // which case it is not resumed); only now may it run.
                    if result.is_ok() {
                        self.resume(follow_up).await?;
                    }
                    None
                }
                None => Some((id, result)),
            },
            Incoming::Event {
                method,
                params,
                session_id,
            } => {
                self.on_event(&method, &params, session_id.as_deref())
                    .await?;
                None
            }
        };
        if self.main_gone {
            return Err(CdpError::TargetGone);
        }
        Ok(reply)
    }

    async fn on_event(
        &mut self,
        method: &str,
        params: &Value,
        session: Option<&str>,
    ) -> Result<(), CdpError> {
        match method {
            "Fetch.requestPaused" => {
                let Some(request_id) = params.get("requestId").and_then(Value::as_str) else {
                    return Ok(());
                };
                let url = params
                    .pointer("/request/url")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let navigation =
                    params.get("resourceType").and_then(Value::as_str) == Some("Document");
                if url.starts_with("data:") && !navigation {
                    self.send(
                        session,
                        "Fetch.continueRequest",
                        json!({"requestId": request_id}),
                    )
                    .await?;
                } else {
                    let fail = json!({"requestId": request_id, "errorReason": "BlockedByClient"});
                    self.send(session, "Fetch.failRequest", fail).await?;
                }
            }
            "Page.javascriptDialogOpening" => {
                self.send(
                    session,
                    "Page.handleJavaScriptDialog",
                    json!({"accept": false}),
                )
                .await?;
            }
            "Target.attachedToTarget" => self.on_attached(params).await?,
            "Target.detachedFromTarget" => {
                let detached = params.get("sessionId").and_then(Value::as_str);
                if detached.is_some() && detached == self.main_session.as_deref() {
                    self.main_gone = true;
                }
            }
            "Inspector.targetCrashed" | "Inspector.detached" => {
                if session.is_some() && session == self.main_session.as_deref() {
                    self.main_gone = true;
                }
            }
            "Page.lifecycleEvent" => {
                let frame = params.get("frameId").and_then(Value::as_str);
                if session.is_some()
                    && session == self.main_session.as_deref()
                    && frame.is_some()
                    && frame == self.main_frame.as_deref()
                    && params.get("name").and_then(Value::as_str) == Some("load")
                {
                    self.main_loaded = true;
                }
            }
            _ => {}
        }
        Ok(())
    }

    async fn on_attached(&mut self, params: &Value) -> Result<(), CdpError> {
        let (Some(session), Some(target), Some(kind)) = (
            params.get("sessionId").and_then(Value::as_str),
            params
                .pointer("/targetInfo/targetId")
                .and_then(Value::as_str),
            params.pointer("/targetInfo/type").and_then(Value::as_str),
        ) else {
            return Ok(());
        };
        match kind {
            "page" => match self.main_target.as_deref() {
                Some(main) if main == target => {
                    if self.main_session.is_none() {
                        self.main_session = Some(session.to_owned());
                    }
                }
                // A popup (window.open, target=_blank, form target) or any other new page. A
                // popup can share the opener's renderer process, whose main thread stays blocked
                // while the popup waits for the debugger, so closing it while paused would hang
                // the opener. Protect it like the page, let it run, then close it.
                Some(_) => {
                    let follow_up = AfterAck::ResumeAndClose {
                        session: session.to_owned(),
                        target: target.to_owned(),
                    };
                    self.protect_then(session, follow_up).await?;
                }
                None => self
                    .unclaimed_pages
                    .push((target.to_owned(), session.to_owned())),
            },
            // An out-of-process iframe. Give it the page's protections, then let it run once
            // interception is confirmed active.
            "iframe" => {
                self.send(Some(session), "Target.setAutoAttach", auto_attach())
                    .await?;
                let follow_up = AfterAck::Resume {
                    session: session.to_owned(),
                };
                self.protect_then(session, follow_up).await?;
            }
            // Dedicated, shared and service workers and worklets stay paused before their first
            // statement until the browser is killed. They run on their own threads, so a paused
            // worker blocks nothing else.
            _ => {}
        }
        Ok(())
    }

    /// Enable dialog handling, offline emulation and request interception on a paused child
    /// target, then run `follow_up` once interception is acknowledged.
    async fn protect_then(&mut self, session: &str, follow_up: AfterAck) -> Result<(), CdpError> {
        let child = Some(session);
        self.send(child, "Page.enable", json!({})).await?;
        if self.hardening.offline {
            self.send(child, "Network.enable", json!({})).await?;
            self.send(
                child,
                "Network.emulateNetworkConditions",
                offline_conditions(),
            )
            .await?;
        }
        if self.hardening.intercept_requests {
            let id = self.send(child, "Fetch.enable", fetch_patterns()).await?;
            self.after_ack.insert(id, follow_up);
            Ok(())
        } else {
            self.resume(follow_up).await
        }
    }

    async fn resume(&mut self, follow_up: AfterAck) -> Result<(), CdpError> {
        match follow_up {
            AfterAck::Resume { session } => {
                self.send(Some(&session), "Runtime.runIfWaitingForDebugger", json!({}))
                    .await?;
            }
            AfterAck::ResumeAndClose { session, target } => {
                self.send(Some(&session), "Runtime.runIfWaitingForDebugger", json!({}))
                    .await?;
                self.send(None, "Target.closeTarget", json!({"targetId": target}))
                    .await?;
            }
        }
        Ok(())
    }

    async fn claim_main_target(&mut self, target: String) -> Result<(), CdpError> {
        for (other, session) in std::mem::take(&mut self.unclaimed_pages) {
            if other == target {
                self.main_session = Some(session);
            } else {
                let follow_up = AfterAck::ResumeAndClose {
                    session: session.clone(),
                    target: other,
                };
                self.protect_then(&session, follow_up).await?;
            }
        }
        self.main_target = Some(target);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clip_matches_playwright_for_an_unscrolled_page() {
        let metrics = json!({"visualViewport": {"pageX": 0, "pageY": 0, "scale": 1}});
        assert_eq!(
            screenshot_clip(&metrics, 1200, 630),
            json!({"x": 0.0, "y": 0.0, "width": 1200.0, "height": 630.0, "scale": 1.0})
        );
    }

    #[test]
    fn clip_follows_scroll_and_zoom() {
        let metrics = json!({"visualViewport": {"pageX": 10, "pageY": 400, "scale": 2}});
        assert_eq!(
            screenshot_clip(&metrics, 1201, 631),
            json!({"x": 10.0, "y": 400.0, "width": 600.0, "height": 315.0, "scale": 2.0})
        );
        let fallback = json!({"cssVisualViewport": {"pageX": 0, "pageY": 0, "scale": 0}});
        assert_eq!(screenshot_clip(&fallback, 10, 10)["scale"], json!(1.0));
    }

    #[test]
    fn frame_ids_are_collected_depth_first() {
        let tree = json!({"frame": {"id": "A"}, "childFrames": [
            {"frame": {"id": "B"}, "childFrames": [{"frame": {"id": "C"}}]},
            {"frame": {"id": "D"}},
        ]});
        let mut frames = Vec::new();
        collect_frame_ids(Some(&tree), &mut frames);
        assert_eq!(frames, ["A", "B", "C", "D"]);
    }
}
