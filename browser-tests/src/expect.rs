// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Neil Blackman
//! Auto-retrying expectations, as Playwright's `expect(locator)`: each matcher re-resolves the
//! locator and re-checks until it holds or the expect timeout (5 s, Playwright's default) runs
//! out. Single-element matchers are strict: more than one match fails at once.
//!
//! ```no_run
//! # async fn example(page: &browser_tests::Page) -> Result<(), browser_tests::Error> {
//! use browser_tests::{expect, regex};
//! expect(&page.locator("#toast")).to_have_text("Saved").await?;
//! expect(&page.locator(".vreact.fav")).to_have_attribute("aria-pressed", "true").await?;
//! expect(&page.locator("#status")).to_have_class(regex("error", "")).await?;
//! expect(&page.locator("#inspector")).not().to_have_attribute("inert", "").await?;
//! # Ok(())
//! # }
//! ```
//!
//! A failure is an [`Error::Assertion`] naming the locator, the expected and the last received
//! value, and the path of a screenshot taken at that moment.

use std::fmt::Debug;
use std::future::Future;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::time::Instant;

use crate::Error;
use crate::connection::lock;
use crate::locator::{Locator, Lookup, Pattern, is_transient};

static EXPECT_TIMEOUT_MS: AtomicU64 = AtomicU64::new(5_000);
static FAILURE_DIR: OnceLock<PathBuf> = OnceLock::new();
static CURRENT_TEST: Mutex<String> = Mutex::new(String::new());
static SHOT: AtomicUsize = AtomicUsize::new(0);
static CURRENT_STEP: Mutex<String> = Mutex::new(String::new());

const EXPECT_CALL: &str = "function(steps, m) { const bt = globalThis[Symbol.for(\"browser-tests\")]; if (!bt) return \"__browser_tests_missing__\"; return bt.expect(steps, m); }";

/// Playwright's polling intervals for `expect`, in milliseconds.
const INTERVALS: [u64; 4] = [100, 250, 500, 1000];

/// The default expect timeout (5 s unless changed).
pub fn expect_timeout() -> Duration {
    Duration::from_millis(EXPECT_TIMEOUT_MS.load(Ordering::Relaxed))
}

/// Change the default expect timeout for every later expectation.
pub fn set_expect_timeout(timeout: Duration) {
    EXPECT_TIMEOUT_MS.store(
        u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX),
        Ordering::Relaxed,
    );
}

/// Where failure screenshots go (once per process; later calls are ignored). Default:
/// `$TMPDIR/e2e-failure-screenshots`.
pub fn set_failure_dir(dir: PathBuf) {
    let _ = FAILURE_DIR.set(dir);
}

/// The test name used in failure screenshot file names.
pub fn set_current_test(name: &str) {
    *lock(&CURRENT_TEST) = name.to_owned();
}

/// Record what the test is doing now (an action, expectation, navigation or wait), so a test
/// that times out can say where it was.
pub fn begin_step(description: String) {
    *lock(&CURRENT_STEP) = description;
}

/// The step recorded last by [`begin_step`].
pub fn current_step() -> String {
    lock(&CURRENT_STEP).clone()
}

fn failure_dir() -> PathBuf {
    FAILURE_DIR
        .get()
        .cloned()
        .unwrap_or_else(|| std::env::temp_dir().join("e2e-failure-screenshots"))
}

/// Save a screenshot of `page` for a failure message; returns its path, or why there is none.
pub async fn failure_screenshot(page: &crate::Page) -> String {
    let name: String = lock(&CURRENT_TEST)
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let name = if name.is_empty() {
        "browser-test".to_owned()
    } else {
        name
    };
    let dir = failure_dir();
    let path = dir.join(format!(
        "{}-{}.png",
        &name[..name.len().min(120)],
        SHOT.fetch_add(1, Ordering::Relaxed)
    ));
    match page.screenshot().await {
        Ok(png) => match std::fs::create_dir_all(&dir).and_then(|()| std::fs::write(&path, png)) {
            Ok(()) => path.display().to_string(),
            Err(error) => format!("(not saved: {error})"),
        },
        Err(error) => format!("(no screenshot: {error})"),
    }
}

/// Assertions on a locator: [`expect`].
pub struct LocatorAssertions<'a, 'p> {
    locator: &'a Locator<'p>,
    not: bool,
    timeout: Duration,
}

/// Start an assertion on `locator`.
pub fn expect<'a, 'p>(locator: &'a Locator<'p>) -> LocatorAssertions<'a, 'p> {
    LocatorAssertions {
        locator,
        not: false,
        timeout: expect_timeout(),
    }
}

enum Outcome {
    Checked { matches: bool, received: Value },
    Strict(usize, Vec<String>),
}

fn pattern_json(pattern: &Pattern) -> Value {
    pattern.to_json()
}

impl LocatorAssertions<'_, '_> {
    /// Negate the next matcher (Playwright's `.not`).
    #[allow(clippy::should_implement_trait)]
    pub fn not(mut self) -> Self {
        self.not = !self.not;
        self
    }

    /// Wait at most `timeout` instead of the default expect timeout.
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    async fn evaluate(&self, matcher: &Value) -> Result<Outcome, Error> {
        let locator = self.locator;
        let scope = match locator.scope().await? {
            Lookup::Found(scope) => scope,
            Lookup::Missing(_) => return Ok(missing(matcher)),
            Lookup::Strict(count, previews) => return Ok(Outcome::Strict(count, previews)),
        };
        let result = locator
            .page()
            .call_in(
                &scope.ctx,
                EXPECT_CALL,
                &[locator.steps_json(), matcher.clone()],
                true,
            )
            .await;
        locator.release_scope(&scope).await;
        let result = result?.get("value").cloned().unwrap_or(Value::Null);
        if let Some(count) = result.get("strict").and_then(Value::as_u64) {
            let previews = result["previews"]
                .as_array()
                .map(|list| {
                    list.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default();
            return Ok(Outcome::Strict(
                usize::try_from(count).unwrap_or(usize::MAX),
                previews,
            ));
        }
        if result.get("missing") == Some(&Value::Bool(true)) {
            return Ok(missing(matcher));
        }
        Ok(Outcome::Checked {
            matches: result.get("matches") == Some(&Value::Bool(true)),
            received: result.get("received").cloned().unwrap_or(Value::Null),
        })
    }

    async fn check(&self, mut matcher: Value, name: &str, expected: String) -> Result<(), Error> {
        begin_step(format!(
            "expect({}){}.{name}",
            self.locator.describe(),
            if self.not { ".not" } else { "" }
        ));
        matcher["isNot"] = Value::Bool(self.not);
        let deadline = Instant::now() + self.timeout;
        let mut attempt = 0;
        let mut received = Value::String("<element(s) not found>".into());
        let mut note = String::new();
        loop {
            match self.evaluate(&matcher).await {
                Ok(Outcome::Checked {
                    matches,
                    received: now,
                }) => {
                    if matches != self.not {
                        return Ok(());
                    }
                    received = now;
                    note.clear();
                }
                Ok(Outcome::Strict(count, previews)) => {
                    return Err(self.locator.strict_error(count, &previews));
                }
                Err(error) if is_transient(&error) => note = error.to_string(),
                Err(error) => return Err(error),
            }
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            let interval = Duration::from_millis(INTERVALS[attempt.min(INTERVALS.len() - 1)]);
            tokio::time::sleep(interval.min(deadline - now)).await;
            attempt += 1;
        }
        let screenshot = failure_screenshot(self.locator.page()).await;
        let not = if self.not { ".not" } else { "" };
        let mut message = format!(
            "expect(locator){not}.{name} failed\n\nLocator:    {}\nExpected:   {}{expected}\nReceived:   {}\nTimeout:    {}ms\nScreenshot: {screenshot}",
            self.locator.describe(),
            if self.not { "not " } else { "" },
            render(&received),
            self.timeout.as_millis(),
        );
        if !note.is_empty() {
            message.push_str(&format!("\nLast error: {note}"));
        }
        Err(Error::Assertion(message))
    }

    async fn state(&self, expression: &str, name: &str) -> Result<(), Error> {
        self.check(json!({"expression": expression}), name, expression.into())
            .await
    }

    pub async fn to_be_visible(&self) -> Result<(), Error> {
        self.state("visible", "toBeVisible()").await
    }

    /// No element matches, or the match is not visible.
    pub async fn to_be_hidden(&self) -> Result<(), Error> {
        self.state("hidden", "toBeHidden()").await
    }

    pub async fn to_be_attached(&self) -> Result<(), Error> {
        self.state("attached", "toBeAttached()").await
    }

    pub async fn to_be_enabled(&self) -> Result<(), Error> {
        self.state("enabled", "toBeEnabled()").await
    }

    pub async fn to_be_disabled(&self) -> Result<(), Error> {
        self.state("disabled", "toBeDisabled()").await
    }

    pub async fn to_be_editable(&self) -> Result<(), Error> {
        self.state("editable", "toBeEditable()").await
    }

    pub async fn to_be_checked(&self) -> Result<(), Error> {
        self.state("checked", "toBeChecked()").await
    }

    pub async fn to_be_focused(&self) -> Result<(), Error> {
        self.state("focused", "toBeFocused()").await
    }

    /// Some part of the element intersects the viewport (`IntersectionObserver`).
    pub async fn to_be_in_viewport(&self) -> Result<(), Error> {
        self.state("inViewport", "toBeInViewport()").await
    }

    /// The element's text (whitespace-normalised) is `expected` (whole, case-sensitive) or
    /// matches the regular expression.
    pub async fn to_have_text(&self, expected: impl Into<Pattern>) -> Result<(), Error> {
        let expected = expected.into();
        self.check(
            json!({"expression": "text", "expected": pattern_json(&expected)}),
            "toHaveText()",
            expected.describe(),
        )
        .await
    }

    /// The element's text contains `expected` (case-sensitive) or matches the regular
    /// expression.
    pub async fn to_contain_text(&self, expected: impl Into<Pattern>) -> Result<(), Error> {
        let expected = expected.into();
        self.check(
            json!({"expression": "text", "expected": pattern_json(&expected), "substring": true}),
            "toContainText()",
            expected.describe(),
        )
        .await
    }

    /// The matches' texts are exactly `expected`, in order (Playwright's `toHaveText([...])`).
    pub async fn to_have_texts<P: Into<Pattern> + Clone>(
        &self,
        expected: &[P],
    ) -> Result<(), Error> {
        let patterns: Vec<Pattern> = expected.iter().cloned().map(Into::into).collect();
        self.check(
            json!({"expression": "text.array", "expected": patterns.iter().map(pattern_json).collect::<Vec<_>>()}),
            "toHaveText([])",
            format!("{:?}", patterns.iter().map(Pattern::describe).collect::<Vec<_>>()),
        )
        .await
    }

    /// Each of `expected` is contained in a match, in order (Playwright's
    /// `toContainText([...])`).
    pub async fn to_contain_texts<P: Into<Pattern> + Clone>(
        &self,
        expected: &[P],
    ) -> Result<(), Error> {
        let patterns: Vec<Pattern> = expected.iter().cloned().map(Into::into).collect();
        self.check(
            json!({"expression": "text.array", "substring": true, "expected": patterns.iter().map(pattern_json).collect::<Vec<_>>()}),
            "toContainText([])",
            format!("{:?}", patterns.iter().map(Pattern::describe).collect::<Vec<_>>()),
        )
        .await
    }

    pub async fn to_have_count(&self, count: usize) -> Result<(), Error> {
        self.check(
            json!({"expression": "count", "count": count}),
            "toHaveCount()",
            count.to_string(),
        )
        .await
    }

    /// The attribute is `expected` (whole) or matches the regular expression.
    pub async fn to_have_attribute(
        &self,
        name: &str,
        expected: impl Into<Pattern>,
    ) -> Result<(), Error> {
        let expected = expected.into();
        self.check(
            json!({"expression": "attribute", "name": name, "expected": pattern_json(&expected)}),
            &format!("toHaveAttribute({name:?})"),
            expected.describe(),
        )
        .await
    }

    /// The element has the attribute, whatever its value.
    pub async fn to_have_attribute_present(&self, name: &str) -> Result<(), Error> {
        self.check(
            json!({"expression": "attribute", "name": name}),
            &format!("toHaveAttribute({name:?})"),
            "attribute present".into(),
        )
        .await
    }

    /// The `value` of an `<input>`, `<textarea>` or `<select>`.
    pub async fn to_have_value(&self, expected: impl Into<Pattern>) -> Result<(), Error> {
        let expected = expected.into();
        self.check(
            json!({"expression": "value", "expected": pattern_json(&expected)}),
            "toHaveValue()",
            expected.describe(),
        )
        .await
    }

    /// The whole `class` attribute is `expected`, or matches the regular expression.
    pub async fn to_have_class(&self, expected: impl Into<Pattern>) -> Result<(), Error> {
        let expected = expected.into();
        self.check(
            json!({"expression": "class", "expected": pattern_json(&expected)}),
            "toHaveClass()",
            expected.describe(),
        )
        .await
    }

    /// The computed style property `name`.
    pub async fn to_have_css(&self, name: &str, expected: impl Into<Pattern>) -> Result<(), Error> {
        let expected = expected.into();
        self.check(
            json!({"expression": "css", "name": name, "expected": pattern_json(&expected)}),
            &format!("toHaveCSS({name:?})"),
            expected.describe(),
        )
        .await
    }
}

/// What a matcher concludes when nothing matches, as Playwright's injected `expect`.
fn missing(matcher: &Value) -> Outcome {
    let expression = matcher["expression"].as_str().unwrap_or("");
    let is_not = matcher["isNot"].as_bool().unwrap_or(false);
    let received = Value::String("<element(s) not found>".into());
    let matches = match expression {
        "count" => matcher["count"].as_u64() == Some(0),
        "text.array" => matcher["expected"].as_array().is_some_and(Vec::is_empty),
        "hidden" if !is_not => true,
        "visible" | "attached" | "inViewport" if is_not => false,
        _ => is_not,
    };
    let received = match expression {
        "count" => json!(0),
        "text.array" => json!([]),
        _ => received,
    };
    Outcome::Checked { matches, received }
}

fn render(value: &Value) -> String {
    match value {
        Value::String(text) => format!("{text:?}"),
        other => other.to_string(),
    }
}

/// Poll `probe` until `accept` holds for its value (Playwright's `expect.poll`), with the expect
/// timeout and Playwright's intervals. Returns the accepted value.
pub async fn poll<T, F, Fut>(what: &str, probe: F, accept: impl Fn(&T) -> bool) -> Result<T, Error>
where
    T: Debug,
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, Error>>,
{
    poll_with_timeout(what, expect_timeout(), probe, accept).await
}

/// [`poll`] with its own timeout instead of the expect timeout (`expect.poll(fn, { timeout })`).
pub async fn poll_with_timeout<T, F, Fut>(
    what: &str,
    timeout: Duration,
    mut probe: F,
    accept: impl Fn(&T) -> bool,
) -> Result<T, Error>
where
    T: Debug,
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, Error>>,
{
    begin_step(format!("expect.poll: {what}"));
    let deadline = Instant::now() + timeout;
    let mut attempt = 0;
    loop {
        let value = probe().await?;
        if accept(&value) {
            return Ok(value);
        }
        let now = Instant::now();
        if now >= deadline {
            return Err(Error::Assertion(format!(
                "expect.poll failed: {what}\nLast value: {value:?}\nTimeout:    {}ms",
                timeout.as_millis()
            )));
        }
        let interval = Duration::from_millis(INTERVALS[attempt.min(INTERVALS.len() - 1)]);
        tokio::time::sleep(interval.min(deadline - now)).await;
        attempt += 1;
    }
}
