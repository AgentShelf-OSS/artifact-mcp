// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Neil Blackman
//! A test browser: one `chrome-headless-shell` process group with a fresh profile.

use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use artifact_cdp::{DEFAULT_MAX_MESSAGE_BYTES, LaunchOptions, Network};
use serde_json::{Value, json};
use tokio::task::JoinHandle;

use crate::Error;
use crate::connection::{Connection, EventHub, EventSink, Subscription};
use crate::page::Page;

/// Prefix of the per-browser temporary directories under `$TMPDIR`.
pub const TEMP_DIR_PREFIX: &str = "browser-tests-";

/// How to launch a test browser.
#[derive(Debug, Clone)]
pub struct BrowserOptions {
    /// The `chrome-headless-shell` executable.
    pub executable: PathBuf,
    /// [`Network::LockedDown`] by default: pages reach nothing but [`Page::route`] handlers.
    /// [`Network::LoopbackOnly`] lets them reach servers on this machine.
    pub network: Network,
    /// Extra Chromium switches.
    pub extra_args: Vec<OsString>,
    /// Show the browser's own stderr (crash and GPU diagnostics).
    pub inherit_stderr: bool,
    /// Default timeout for commands, navigation and evaluation.
    pub timeout: Duration,
}

impl BrowserOptions {
    pub fn new(executable: impl Into<PathBuf>) -> Self {
        Self {
            executable: executable.into(),
            network: Network::LockedDown,
            extra_args: Vec::new(),
            inherit_stderr: std::env::var_os("BROWSER_TESTS_CHROME_STDERR").is_some(),
            timeout: Duration::from_secs(30),
        }
    }

    pub fn network(mut self, network: Network) -> Self {
        self.network = network;
        self
    }
}

#[derive(Default)]
struct BrowserEvents {
    hub: EventHub,
}

impl EventSink for BrowserEvents {
    fn on_event(&self, method: &str, params: &Value) {
        self.hub.publish(method, params);
    }
}

/// A running browser. Dropping it kills the whole process group; [`Browser::close`] also waits
/// for the group to exit and removes the profile.
pub struct Browser {
    process: Option<artifact_cdp::Browser>,
    connection: Arc<Connection>,
    reader: JoinHandle<()>,
    events: Arc<BrowserEvents>,
    timeout: Duration,
}

impl Browser {
    /// Start a browser with a fresh profile and check that it answers. Call this from a tokio
    /// runtime with I/O and time enabled. The browser dies with the thread that launched it.
    pub async fn launch(options: &BrowserOptions) -> Result<Self, Error> {
        let launch = LaunchOptions {
            network: options.network,
            temp_dir_prefix: TEMP_DIR_PREFIX,
            log_label: "[browser-tests]",
            max_message_bytes: DEFAULT_MAX_MESSAGE_BYTES,
            extra_args: options.extra_args.clone(),
            inherit_stderr: options.inherit_stderr,
        };
        let (process, pipe) = artifact_cdp::Browser::launch(&options.executable, &launch)
            .map_err(|error| Error::Launch(format!("{}: {error}", options.executable.display())))?;
        let events = Arc::new(BrowserEvents::default());
        let (connection, reader) =
            Connection::start(pipe, Arc::clone(&events) as Arc<dyn EventSink>);
        let browser = Self {
            process: Some(process),
            connection,
            reader,
            events,
            timeout: options.timeout,
        };
        if let Err(error) = browser.version().await {
            browser.close().await;
            return Err(Error::Launch(format!(
                "{} did not answer: {error}",
                options.executable.display()
            )));
        }
        Ok(browser)
    }

    /// The browser's product string, for example `HeadlessChrome/156.0.8078.47`.
    pub async fn version(&self) -> Result<String, Error> {
        let version = self.command("Browser.getVersion", json!({})).await?;
        Ok(version
            .get("product")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned())
    }

    /// A new tab in a new browser context (no shared cookies, storage or cache), with a
    /// 1280×720 viewport.
    pub async fn new_page(&self) -> Result<Page, Error> {
        Page::open(Arc::clone(&self.connection), self.timeout).await
    }

    /// Send a raw DevTools command to the browser target.
    pub async fn command(&self, method: &str, params: Value) -> Result<Value, Error> {
        self.connection
            .call(None, method, params, self.timeout)
            .await
    }

    /// Browser-level events (no session), for example `Target.targetCreated`.
    pub fn subscribe(&self, method: &str) -> Subscription {
        self.events.hub.subscribe(method)
    }

    /// The browser's process group id.
    pub fn pgid(&self) -> u32 {
        self.process.as_ref().map_or(0, artifact_cdp::Browser::pgid)
    }

    /// Kill the process group, wait for it to exit and remove the profile directory.
    pub async fn close(mut self) {
        self.reader.abort();
        if let Some(process) = self.process.take() {
            process.shutdown().await;
        }
    }
}

impl Drop for Browser {
    fn drop(&mut self) {
        // `artifact_cdp::Browser`'s own `Drop` kills the group and removes the profile.
        self.reader.abort();
    }
}
