// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Neil Blackman
//! Evaluation in a frame (Playwright's `frame.evaluate`, `frame.waitForFunction` and
//! `frame.setContent`) through a [`FrameLocator`], and [`Page::wait_for_function`] with its own
//! timeout.

use std::time::Duration;

use serde_json::Value;

use crate::locator::FrameLocator;
use crate::{Error, Page};

/// JavaScript truthiness of a JSON result (`undefined` arrives as `null`).
fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(flag) => *flag,
        Value::Number(number) => number.as_f64().is_some_and(|n| n != 0.0 && !n.is_nan()),
        Value::String(text) => !text.is_empty(),
        _ => true,
    }
}

/// Call `probe` until its value is truthy or `timeout` passes, every 50 ms.
async fn wait_until_truthy<F, Fut>(
    what: String,
    timeout: Duration,
    mut probe: F,
) -> Result<Value, Error>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<Value, Error>>,
{
    crate::expect::begin_step(what.clone());
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let value = probe().await?;
        if truthy(&value) {
            return Ok(value);
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(Error::Timeout {
                what,
                after: timeout,
            });
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

impl Page {
    /// [`Page::wait_for_function`] with its own `timeout` instead of the page's default
    /// (Playwright's `waitForFunction(fn, arg, { timeout })`).
    pub async fn wait_for_function_with_timeout(
        &self,
        function: &str,
        args: &[Value],
        timeout: Duration,
    ) -> Result<Value, Error> {
        wait_until_truthy(format!("waitForFunction {function}"), timeout, || {
            self.call(function, args)
        })
        .await
    }
}

impl FrameLocator<'_> {
    /// Call `function` (source text) as `function(arg)` in the frame's document and return its
    /// awaited result as JSON (Playwright's `frame.evaluate(function, arg)`). Waits, like a
    /// locator, for the frame and its document.
    pub async fn evaluate(&self, function: &str, arg: Value) -> Result<Value, Error> {
        self.locator(":root")
            .evaluate(&format!("(_root, arg) => ({function})(arg)"), arg)
            .await
    }

    /// Call `function` as `function(arg)` in the frame until it returns a truthy value, with the
    /// page's default timeout (Playwright's `frame.waitForFunction(function, arg)`).
    pub async fn wait_for_function(&self, function: &str, arg: Value) -> Result<Value, Error> {
        let timeout = self.owner().page().default_timeout();
        self.wait_for_function_with_timeout(function, arg, timeout)
            .await
    }

    /// [`FrameLocator::wait_for_function`] with its own `timeout`.
    pub async fn wait_for_function_with_timeout(
        &self,
        function: &str,
        arg: Value,
        timeout: Duration,
    ) -> Result<Value, Error> {
        wait_until_truthy(format!("frame waitForFunction {function}"), timeout, || {
            self.evaluate(function, arg.clone())
        })
        .await
    }

    /// Replace the frame's document with `html` (`document.open/write/close`, as Playwright's
    /// `frame.setContent`) and wait for its `load` event. The HTML is a call argument, never
    /// part of script source.
    pub async fn set_content(&self, html: &str) -> Result<(), Error> {
        self.evaluate(
            r#"(html) => {
                document.open();
                document.write(html);
                document.close();
                if (document.readyState === "complete") return;
                return new Promise((resolve) => window.addEventListener("load", () => resolve(), { once: true }));
            }"#,
            Value::from(html),
        )
        .await?;
        Ok(())
    }
}
