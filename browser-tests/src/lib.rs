// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Neil Blackman
//! `browser-tests`: drive real `chrome-headless-shell` from Rust tests, with no Node.
//!
//! The harness launches one browser per test (or per test file) with a fresh profile, using the
//! same DevTools pipe transport and process-group launcher as the preview renderer
//! (`artifact-cdp`). The network is locked down by default; [`Page::route`] answers requests
//! inside the browser, and [`Network::LoopbackOnly`] reaches a server on this machine.
//!
//! ```no_run
//! use browser_tests::{Browser, BrowserOptions, chrome_from_env};
//! use serde_json::json;
//!
//! # async fn example() -> Result<(), browser_tests::Error> {
//! let Some(chrome) = chrome_from_env() else { return Ok(()) };
//! let browser = Browser::launch(&BrowserOptions::new(chrome)).await?;
//! let page = browser.new_page().await?;
//! page.set_content("<p id=greeting>hello</p>").await?;
//! let text = page.evaluate("document.getElementById('greeting').textContent").await?;
//! assert_eq!(text, json!("hello"));
//! let sum = page.call("(a, b) => a + b", &[json!(2), json!(3)]).await?;
//! assert_eq!(sum, json!(5));
//! assert!(page.log().iter().all(|entry| !entry.is_failure()));
//! browser.close().await;
//! # Ok(())
//! # }
//! ```
//!
//! The JavaScript unit tests in `js/` run through [`suite`]; see `README.md`.
#![forbid(unsafe_code)]

mod browser;
mod connection;
mod page;
mod route;
pub mod suite;

use std::fmt;
use std::path::PathBuf;
use std::time::Duration;

pub use artifact_cdp::Network;
pub use browser::{Browser, BrowserOptions, TEMP_DIR_PREFIX};
pub use connection::Subscription;
pub use page::{LogEntry, Page, ScriptError};
pub use route::{Request, Response, StaticFiles, content_type};

/// Everything that can go wrong while driving the browser.
#[derive(Debug)]
pub enum Error {
    /// The browser could not be started.
    Launch(String),
    /// A DevTools command returned an error.
    Command { method: String, message: String },
    /// The browser, the page or the pipe went away.
    Closed(String),
    /// Nothing happened in time.
    Timeout { what: String, after: Duration },
    /// The evaluated JavaScript threw or rejected. The text includes the stack when there is one.
    Evaluation(String),
    /// Navigation failed (for example `net::ERR_PROXY_CONNECTION_FAILED`).
    Navigation(String),
    /// A reply did not have the expected shape.
    Protocol(String),
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Launch(detail) => write!(formatter, "browser launch failed: {detail}"),
            Self::Command { method, message } => write!(formatter, "{method} failed: {message}"),
            Self::Closed(detail) => write!(formatter, "closed: {detail}"),
            Self::Timeout { what, after } => write!(formatter, "timed out after {after:?}: {what}"),
            Self::Evaluation(detail) => write!(formatter, "JavaScript error: {detail}"),
            Self::Navigation(detail) => write!(formatter, "navigation failed: {detail}"),
            Self::Protocol(detail) => write!(formatter, "unexpected DevTools reply: {detail}"),
        }
    }
}

impl std::error::Error for Error {}

/// The browser from `CHROME_HEADLESS_SHELL`, or `None` (after printing why) when browser tests
/// are skipped. When `CI` is set a missing browser panics instead: a CI job must never pass by
/// skipping.
pub fn chrome_from_env() -> Option<PathBuf> {
    match std::env::var_os("CHROME_HEADLESS_SHELL") {
        Some(path) if !path.is_empty() => Some(PathBuf::from(path)),
        _ => {
            assert!(
                std::env::var_os("CI").is_none(),
                "CHROME_HEADLESS_SHELL must be set in CI; browser tests may not be skipped there"
            );
            eprintln!("skipping browser test: CHROME_HEADLESS_SHELL is not set");
            None
        }
    }
}

/// The repository root (the parent of this crate).
pub fn repo_root() -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest.parent().map(PathBuf::from).unwrap_or(manifest)
}
