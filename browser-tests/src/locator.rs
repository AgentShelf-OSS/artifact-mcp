// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Neil Blackman
//! Locators: lazy, re-resolved element queries with Playwright's meaning, and the actions that
//! auto-wait on them.
//!
//! A [`Locator`] is a chain of steps (CSS, role, text, label, filters, `nth`) evaluated in a
//! frame by the in-page engine (`injected.js`) every time it is used, so it follows the page as it
//! re-renders. [`Page::frame_locator`] / [`Locator::content_frame`] cross into an iframe, out of
//! process or not. Actions resolve to exactly one element (Playwright's strict mode: more than one
//! match is an immediate [`Error::Strict`]) and retry until the page's default timeout:
//!
//! * pointer actions ([`Locator::click`], [`Locator::dblclick`], [`Locator::hover`],
//!   [`Locator::tap`]) wait for the element to be attached, visible, enabled (not for hover),
//!   stable for two animation frames and the hit target at the action point (also in every
//!   parent frame), scroll it into view, then dispatch trusted input at its centre;
//! * [`Locator::fill`] waits for visible, enabled and editable, selects the text and inserts the
//!   value as input-method text (an empty value presses Delete);
//! * [`Locator::select_option`] waits for visible and enabled and for the options to exist;
//! * [`Locator::press`] focuses the element and presses the key.

use std::time::Duration;

use serde_json::{Value, json};
use tokio::time::Instant;

use crate::Error;
use crate::input::MouseButton;
use crate::page::Page;

const INJECTED: &str = include_str!("injected.js");
const MISSING: &str = "__browser_tests_missing__";

/// A text pattern for locators and expectations.
///
/// In locators (`get_by_text`, `get_by_label`, role names, `filter_has_text`) a plain string is
/// Playwright's default: a case-insensitive substring after whitespace normalisation; [`exact`]
/// is case-sensitive and whole; [`regex`] is a JavaScript regular expression. In expectations
/// (`to_have_text`, `to_contain_text`, `to_have_value`, ...) a string is matched as Playwright
/// does there: whole (or contained, for `to_contain_*`) and case-sensitive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Pattern {
    Substring(String),
    Exact(String),
    /// A JavaScript `RegExp` source and flags, for example `regex("Open Thread", "i")`.
    Regex {
        source: String,
        flags: String,
    },
}

/// A case-sensitive, whole-text [`Pattern`] (Playwright's `exact: true`).
pub fn exact(text: impl Into<String>) -> Pattern {
    Pattern::Exact(text.into())
}

/// A JavaScript regular expression [`Pattern`]: `regex("Saved feedback", "")` is
/// `/Saved feedback/`.
pub fn regex(source: impl Into<String>, flags: &str) -> Pattern {
    Pattern::Regex {
        source: source.into(),
        flags: flags.to_owned(),
    }
}

impl From<&str> for Pattern {
    fn from(text: &str) -> Self {
        Self::Substring(text.to_owned())
    }
}

impl From<String> for Pattern {
    fn from(text: String) -> Self {
        Self::Substring(text)
    }
}

impl From<&String> for Pattern {
    fn from(text: &String) -> Self {
        Self::Substring(text.clone())
    }
}

impl Pattern {
    pub(crate) fn to_json(&self) -> Value {
        match self {
            Self::Substring(text) => json!({"kind": "substring", "text": text}),
            Self::Exact(text) => json!({"kind": "exact", "text": text}),
            Self::Regex { source, flags } => {
                json!({"kind": "regex", "text": source, "flags": flags})
            }
        }
    }

    /// Playwright-style rendering: `'text'`, `'text'` with `exact`, `/re/i`.
    pub fn describe(&self) -> String {
        match self {
            Self::Substring(text) | Self::Exact(text) => format!("{text:?}"),
            Self::Regex { source, flags } => format!("/{source}/{flags}"),
        }
    }
}

/// A role query for [`Page::get_by_role`]: the ARIA role plus Playwright's options. A plain
/// `&str` is the role alone.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Role {
    role: String,
    name: Option<Pattern>,
    include_hidden: bool,
    checked: Option<bool>,
    pressed: Option<bool>,
    selected: Option<bool>,
    expanded: Option<bool>,
    disabled: Option<bool>,
    level: Option<u32>,
}

impl Role {
    pub fn new(role: &str) -> Self {
        Self {
            role: role.to_owned(),
            ..Self::default()
        }
    }

    /// The accessible name: a substring (case-insensitive), [`exact`] or [`regex`].
    pub fn name(mut self, name: impl Into<Pattern>) -> Self {
        self.name = Some(name.into());
        self
    }

    /// The exact accessible name (Playwright's `{ name, exact: true }`).
    pub fn exact_name(mut self, name: &str) -> Self {
        self.name = Some(exact(name));
        self
    }

    /// Also match elements hidden from the accessibility tree.
    pub fn include_hidden(mut self) -> Self {
        self.include_hidden = true;
        self
    }

    pub fn checked(mut self, checked: bool) -> Self {
        self.checked = Some(checked);
        self
    }

    pub fn pressed(mut self, pressed: bool) -> Self {
        self.pressed = Some(pressed);
        self
    }

    pub fn selected(mut self, selected: bool) -> Self {
        self.selected = Some(selected);
        self
    }

    pub fn expanded(mut self, expanded: bool) -> Self {
        self.expanded = Some(expanded);
        self
    }

    pub fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = Some(disabled);
        self
    }

    pub fn level(mut self, level: u32) -> Self {
        self.level = Some(level);
        self
    }

    fn to_json(&self) -> Value {
        let mut step = json!({"role": self.role, "includeHidden": self.include_hidden});
        if let Some(name) = &self.name {
            step["name"] = name.to_json();
        }
        for (key, value) in [
            ("checked", self.checked),
            ("pressed", self.pressed),
            ("selected", self.selected),
            ("expanded", self.expanded),
            ("disabled", self.disabled),
        ] {
            if let Some(value) = value {
                step[key] = Value::Bool(value);
            }
        }
        if let Some(level) = self.level {
            step["level"] = json!(level);
        }
        step
    }

    fn describe(&self) -> String {
        let mut options = Vec::new();
        if let Some(name) = &self.name {
            options.push(format!("name: {}", name.describe()));
            if matches!(name, Pattern::Exact(_)) {
                options.push("exact: true".into());
            }
        }
        if self.include_hidden {
            options.push("includeHidden: true".into());
        }
        for (key, value) in [
            ("checked", self.checked),
            ("pressed", self.pressed),
            ("selected", self.selected),
            ("expanded", self.expanded),
            ("disabled", self.disabled),
        ] {
            if let Some(value) = value {
                options.push(format!("{key}: {value}"));
            }
        }
        if let Some(level) = self.level {
            options.push(format!("level: {level}"));
        }
        if options.is_empty() {
            format!("getByRole({:?})", self.role)
        } else {
            format!("getByRole({:?}, {{ {} }})", self.role, options.join(", "))
        }
    }
}

impl From<&str> for Role {
    fn from(role: &str) -> Self {
        Self::new(role)
    }
}

#[derive(Debug, Clone, PartialEq)]
enum Step {
    Css(String),
    Role(Role),
    Text(Pattern),
    Label(Pattern),
    Attribute { name: String, value: Pattern },
    HasText(Pattern),
    HasNotText(Pattern),
    Has(Vec<Step>),
    HasNot(Vec<Step>),
    Visible(bool),
    Nth(i64),
}

impl Step {
    fn to_json(&self) -> Value {
        match self {
            Self::Css(selector) => json!({"css": selector}),
            Self::Role(role) => role.to_json(),
            Self::Text(pattern) => json!({"text": pattern.to_json()}),
            Self::Label(pattern) => json!({"label": pattern.to_json()}),
            Self::Attribute { name, value } => json!({"attribute": name, "value": value.to_json()}),
            Self::HasText(pattern) => json!({"hasText": pattern.to_json()}),
            Self::HasNotText(pattern) => json!({"hasNotText": pattern.to_json()}),
            Self::Has(steps) => json!({"has": steps_json(steps)}),
            Self::HasNot(steps) => json!({"hasNot": steps_json(steps)}),
            Self::Visible(visible) => json!({"visible": visible}),
            Self::Nth(index) => json!({"nth": index}),
        }
    }
}

fn steps_json(steps: &[Step]) -> Value {
    Value::Array(steps.iter().map(Step::to_json).collect())
}

/// CSS steps for `selector`. Standard CSS is evaluated by the browser (open shadow roots are
/// pierced); Playwright's `:visible` pseudo-class is supported at the end of a compound selector
/// followed by a descendant or child combinator, or at the end of the selector
/// (`#grid > .card:visible .title`). Other Playwright selector extensions (`:has-text()`, `>>`,
/// `text=`) are not: use the locator methods instead.
fn css_steps(selector: &str) -> Vec<Step> {
    let mut steps = Vec::new();
    let mut rest = selector.trim().to_owned();
    loop {
        let Some(at) = top_level_find(&rest, ":visible") else {
            if !rest.trim().is_empty() {
                steps.push(Step::Css(rest.trim().to_owned()));
            }
            return steps;
        };
        let (head, tail) = rest.split_at(at);
        let tail = &tail[":visible".len()..];
        assert!(
            !head.trim().is_empty(),
            "browser-tests: `:visible` needs a selector in front of it: {selector:?}"
        );
        steps.push(Step::Css(head.trim().to_owned()));
        steps.push(Step::Visible(true));
        let trimmed = tail.trim_start();
        if trimmed.is_empty() {
            return steps;
        }
        let combinator_ok = tail.starts_with(char::is_whitespace) || trimmed.starts_with('>');
        assert!(
            combinator_ok && !trimmed.starts_with('+') && !trimmed.starts_with('~'),
            "browser-tests: `:visible` must end a compound selector before a descendant or child \
             combinator: {selector:?}"
        );
        rest = if trimmed.starts_with('>') {
            format!(":scope {trimmed}")
        } else {
            trimmed.to_owned()
        };
    }
}

/// The byte offset of `needle` outside quotes, brackets and parentheses, followed by a
/// non-identifier character.
fn top_level_find(text: &str, needle: &str) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut depth = 0i32;
    let mut quote: Option<u8> = None;
    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        if let Some(open) = quote {
            if byte == b'\\' {
                index += 2;
                continue;
            }
            if byte == open {
                quote = None;
            }
        } else {
            match byte {
                b'\\' => {
                    index += 2;
                    continue;
                }
                b'"' | b'\'' => quote = Some(byte),
                b'(' | b'[' => depth += 1,
                b')' | b']' => depth -= 1,
                _ if depth == 0 && text[index..].starts_with(needle) => {
                    let next = bytes.get(index + needle.len());
                    if !next.is_some_and(|next| {
                        next.is_ascii_alphanumeric() || *next == b'-' || *next == b'_'
                    }) {
                        return Some(index);
                    }
                }
                _ => {}
            }
        }
        index += 1;
    }
    None
}

/// Retry bookkeeping for actions: the deadline, the attempt number and the last reason.
struct Retrier {
    deadline: Instant,
    timeout: Duration,
    attempt: u32,
}

impl Retrier {
    fn new(timeout: Duration) -> Self {
        Self {
            deadline: Instant::now() + timeout,
            timeout,
            attempt: 0,
        }
    }

    /// After a failed attempt: an error once the time is up, else wait Playwright's back-off.
    async fn wait(&mut self, what: &str, locator: &str, reason: String) -> Result<(), Error> {
        let now = Instant::now();
        if now >= self.deadline {
            return Err(Error::Timeout {
                what: format!("{what} {locator}: {reason}"),
                after: self.timeout,
            });
        }
        tokio::time::sleep(backoff(self.attempt).min(self.deadline - now)).await;
        self.attempt += 1;
        Ok(())
    }
}

/// Run `$attempt` (an `Attempt` future; `$n` is the attempt number) until it is done, fails with
/// a lasting error, or the time is up. Transient errors and `Attempt::Retry` retry.
macro_rules! retry {
    ($locator:expr, $what:expr, $timeout:expr, |$n:ident| $attempt:expr) => {{
        crate::expect::begin_step(format!("{} {}", $what, $locator.description));
        let mut retrier = Retrier::new($timeout);
        loop {
            let $n = retrier.attempt;
            let reason = match $attempt.await {
                Ok(Attempt::Done(value)) => break Ok(value),
                Ok(Attempt::Retry(reason)) => reason,
                Err(error) if is_transient(&error) => error.to_string(),
                Err(error) => break Err(error),
            };
            if let Err(error) = retrier.wait($what, &$locator.description, reason).await {
                break Err(error);
            }
        }
    }};
}

/// A JavaScript context the locator runs in.
#[derive(Debug, Clone)]
pub(crate) struct Ctx {
    pub(crate) session: String,
    pub(crate) context: i64,
}

/// An iframe element crossed on the way to the locator's frame.
#[derive(Debug, Clone)]
struct Hop {
    /// The context of the document that contains the iframe element.
    ctx: Ctx,
    iframe: String,
}

/// Where a locator's last steps run.
#[derive(Debug, Clone)]
pub(crate) struct Scope {
    pub(crate) ctx: Ctx,
    hops: Vec<Hop>,
}

pub(crate) enum Lookup<T> {
    Found(T),
    /// Nothing yet; the reason.
    Missing(String),
    /// More than one element: how many, and previews of the first ones.
    Strict(usize, Vec<String>),
}

pub(crate) enum Attempt<T> {
    Done(T),
    Retry(String),
}

/// A box in CSS pixels of the main frame's viewport.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BoundingBox {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

/// Options for [`Locator::click_with`].
#[derive(Debug, Clone, Default)]
pub struct ClickOptions {
    pub button: MouseButton,
    /// 0 means 1.
    pub click_count: u32,
    /// Skip the actionability checks (visible, enabled, stable, hit target).
    pub force: bool,
    /// Click here, relative to the top left of the element's border box, instead of its centre.
    pub position: Option<(f64, f64)>,
    /// Instead of the page's default timeout.
    pub timeout: Option<Duration>,
}

#[derive(Debug, Clone, Copy)]
enum Pointer {
    Click { button: MouseButton, count: u32 },
    Hover,
    Tap,
}

impl Pointer {
    fn name(self) -> &'static str {
        match self {
            Self::Click { count: 2, .. } => "dblclick",
            Self::Click { .. } => "click",
            Self::Hover => "hover",
            Self::Tap => "tap",
        }
    }
}

/// States for [`Locator::wait_for`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitState {
    Attached,
    Detached,
    Visible,
    Hidden,
}

/// Errors worth retrying: the context, node or child session went away (navigation, re-render).
pub(crate) fn is_transient(error: &Error) -> bool {
    match error {
        Error::Command { .. } | Error::Closed(_) => true,
        Error::Evaluation(text) => {
            text.contains("Execution context was destroyed")
                || text.contains("Cannot find context")
                || text.contains("Inspected target navigated or closed")
                || text.contains("Cannot find object")
        }
        _ => false,
    }
}

/// Playwright's retry back-off for actions, in milliseconds.
fn backoff(attempt: u32) -> Duration {
    Duration::from_millis(match attempt {
        0 => 0,
        1 => 20,
        2 | 3 => 100,
        _ => 500,
    })
}

fn with_bt(params: &str, body: &str) -> String {
    format!(
        "function({params}) {{ const bt = globalThis[Symbol.for(\"browser-tests\")]; if (!bt) return \"{MISSING}\"; {body} }}"
    )
}

fn is_missing_helper(result: &Value) -> bool {
    result.get("value").and_then(Value::as_str) == Some(MISSING)
}

impl Page {
    pub(crate) fn frame_context(&self, frame_id: &str) -> Option<Ctx> {
        self.state.default_context(frame_id).map(|context| Ctx {
            session: context.session,
            context: context.id,
        })
    }

    pub(crate) fn main_context(&self) -> Option<Ctx> {
        self.frame_context(&self.main_frame)
    }

    async fn install(&self, ctx: &Ctx) -> Result<(), Error> {
        let result = self
            .session_command(
                &ctx.session,
                "Runtime.evaluate",
                json!({"expression": INJECTED, "contextId": ctx.context, "returnByValue": true}),
            )
            .await?;
        if let Some(details) = result.get("exceptionDetails") {
            return Err(Error::Evaluation(format!(
                "installing the locator engine: {}",
                crate::state::exception_text(details)
            )));
        }
        Ok(())
    }

    fn check_call(result: Value) -> Result<Value, Error> {
        if let Some(details) = result.get("exceptionDetails") {
            return Err(Error::Evaluation(crate::state::exception_text(details)));
        }
        Ok(result.get("result").cloned().unwrap_or(Value::Null))
    }

    /// Call `declaration` in `ctx` with JSON `args`. Returns the `RemoteObject` (its `value` when
    /// `by_value`).
    pub(crate) async fn call_in(
        &self,
        ctx: &Ctx,
        declaration: &str,
        args: &[Value],
        by_value: bool,
    ) -> Result<Value, Error> {
        let params = json!({
            "functionDeclaration": declaration,
            "executionContextId": ctx.context,
            "arguments": args.iter().map(|value| json!({"value": value})).collect::<Vec<_>>(),
            "returnByValue": by_value,
            "awaitPromise": true,
            "userGesture": self.user_gesture,
        });
        let mut result = Self::check_call(
            self.session_command(&ctx.session, "Runtime.callFunctionOn", params.clone())
                .await?,
        )?;
        if is_missing_helper(&result) {
            self.install(ctx).await?;
            result = Self::check_call(
                self.session_command(&ctx.session, "Runtime.callFunctionOn", params)
                    .await?,
            )?;
        }
        Ok(result)
    }

    /// Call `declaration` with `this` bound to `object` (in `ctx`).
    pub(crate) async fn call_on(
        &self,
        ctx: &Ctx,
        object: &str,
        declaration: &str,
        args: &[Value],
        by_value: bool,
    ) -> Result<Value, Error> {
        let params = json!({
            "functionDeclaration": declaration,
            "objectId": object,
            "arguments": args.iter().map(|value| json!({"value": value})).collect::<Vec<_>>(),
            "returnByValue": by_value,
            "awaitPromise": true,
            "userGesture": self.user_gesture,
        });
        let mut result = Self::check_call(
            self.session_command(&ctx.session, "Runtime.callFunctionOn", params.clone())
                .await?,
        )?;
        if is_missing_helper(&result) {
            self.install(ctx).await?;
            result = Self::check_call(
                self.session_command(&ctx.session, "Runtime.callFunctionOn", params)
                    .await?,
            )?;
        }
        Ok(result)
    }

    async fn single(&self, ctx: &Ctx, steps: &[Step]) -> Result<Lookup<String>, Error> {
        let result = self
            .call_in(
                ctx,
                &with_bt("steps", "return bt.single(steps);"),
                &[steps_json(steps)],
                false,
            )
            .await?;
        if let Some(object) = result.get("objectId").and_then(Value::as_str) {
            return Ok(Lookup::Found(object.to_owned()));
        }
        let count = result.get("value").and_then(Value::as_u64).unwrap_or(0);
        if count == 0 {
            return Ok(Lookup::Missing("no element matches yet".into()));
        }
        let previews = self
            .call_in(
                ctx,
                &with_bt("steps", "return bt.previews(steps, 10);"),
                &[steps_json(steps)],
                true,
            )
            .await?;
        let previews = previews
            .get("value")
            .and_then(Value::as_array)
            .map(|list| {
                list.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        Ok(Lookup::Strict(
            usize::try_from(count).unwrap_or(usize::MAX),
            previews,
        ))
    }

    async fn release(&self, objects: Vec<(String, String)>) {
        for (session, object) in objects {
            let _ = self
                .session_command(
                    &session,
                    "Runtime.releaseObject",
                    json!({"objectId": object}),
                )
                .await;
        }
    }

    /// Where the top left of `scope`'s viewport is, in the main frame's viewport.
    async fn frame_origin(&self, scope: &Scope) -> Result<(f64, f64), Error> {
        let mut origin = (0.0, 0.0);
        for hop in &scope.hops {
            let offset = self
                .call_on(
                    &hop.ctx,
                    &hop.iframe,
                    &with_bt("", "return bt.frameOffset(this);"),
                    &[],
                    true,
                )
                .await?;
            origin.0 += number(&offset, "/value/x");
            origin.1 += number(&offset, "/value/y");
        }
        Ok(origin)
    }

    async fn viewport(&self) -> Result<(f64, f64), Error> {
        let size = self
            .evaluate("({ width: innerWidth, height: innerHeight })")
            .await?;
        Ok((number(&size, "/width"), number(&size, "/height")))
    }

    /// A locator for the CSS `selector` in the main frame.
    pub fn locator(&self, selector: &str) -> Locator<'_> {
        Locator::root(self).locator(selector)
    }

    /// Elements by ARIA role and accessible name (Playwright's `getByRole`).
    pub fn get_by_role(&self, role: impl Into<Role>) -> Locator<'_> {
        Locator::root(self).get_by_role(role)
    }

    /// The smallest elements whose text matches (Playwright's `getByText`).
    pub fn get_by_text(&self, text: impl Into<Pattern>) -> Locator<'_> {
        Locator::root(self).get_by_text(text)
    }

    /// Form controls by their label, `aria-labelledby` or `aria-label` (Playwright's
    /// `getByLabel`).
    pub fn get_by_label(&self, text: impl Into<Pattern>) -> Locator<'_> {
        Locator::root(self).get_by_label(text)
    }

    pub fn get_by_placeholder(&self, text: impl Into<Pattern>) -> Locator<'_> {
        Locator::root(self).get_by_placeholder(text)
    }

    pub fn get_by_alt_text(&self, text: impl Into<Pattern>) -> Locator<'_> {
        Locator::root(self).get_by_alt_text(text)
    }

    pub fn get_by_title(&self, text: impl Into<Pattern>) -> Locator<'_> {
        Locator::root(self).get_by_title(text)
    }

    pub fn get_by_test_id(&self, id: &str) -> Locator<'_> {
        Locator::root(self).get_by_test_id(id)
    }

    /// The document inside the iframe `selector` selects (Playwright's `frameLocator`).
    pub fn frame_locator(&self, selector: &str) -> FrameLocator<'_> {
        self.locator(selector).content_frame()
    }
}

fn number(value: &Value, pointer: &str) -> f64 {
    value
        .pointer(pointer)
        .and_then(Value::as_f64)
        .unwrap_or(0.0)
}

/// A lazy element query; see the module documentation.
#[derive(Clone)]
pub struct Locator<'p> {
    page: &'p Page,
    /// For each iframe crossed, the steps that select it in its parent document.
    frames: Vec<Vec<Step>>,
    steps: Vec<Step>,
    description: String,
}

/// The document inside an iframe: [`Page::frame_locator`], [`Locator::content_frame`].
#[derive(Clone)]
pub struct FrameLocator<'p> {
    page: &'p Page,
    frames: Vec<Vec<Step>>,
    description: String,
}

impl<'p> FrameLocator<'p> {
    fn root(&self) -> Locator<'p> {
        Locator {
            page: self.page,
            frames: self.frames.clone(),
            steps: Vec::new(),
            description: self.description.clone(),
        }
    }

    pub fn locator(&self, selector: &str) -> Locator<'p> {
        self.root().locator(selector)
    }

    pub fn get_by_role(&self, role: impl Into<Role>) -> Locator<'p> {
        self.root().get_by_role(role)
    }

    pub fn get_by_text(&self, text: impl Into<Pattern>) -> Locator<'p> {
        self.root().get_by_text(text)
    }

    pub fn get_by_label(&self, text: impl Into<Pattern>) -> Locator<'p> {
        self.root().get_by_label(text)
    }

    pub fn get_by_placeholder(&self, text: impl Into<Pattern>) -> Locator<'p> {
        self.root().get_by_placeholder(text)
    }

    pub fn get_by_test_id(&self, id: &str) -> Locator<'p> {
        self.root().get_by_test_id(id)
    }

    /// A nested iframe inside this one.
    pub fn frame_locator(&self, selector: &str) -> FrameLocator<'p> {
        self.locator(selector).content_frame()
    }

    /// The iframe element itself, in its parent document.
    pub fn owner(&self) -> Locator<'p> {
        let mut frames = self.frames.clone();
        let steps = frames.pop().unwrap_or_default();
        Locator {
            page: self.page,
            frames,
            steps,
            description: format!("{}.owner()", self.description),
        }
    }
}

impl<'p> Locator<'p> {
    fn root(page: &'p Page) -> Self {
        Self {
            page,
            frames: Vec::new(),
            steps: Vec::new(),
            description: String::new(),
        }
    }

    fn then(&self, steps: Vec<Step>, description: String) -> Self {
        let mut next = self.clone();
        next.steps.extend(steps);
        next.description = if self.description.is_empty() {
            description
        } else {
            format!("{}.{description}", self.description)
        };
        next
    }

    /// The page this locator belongs to.
    pub fn page(&self) -> &'p Page {
        self.page
    }

    /// Playwright-style description, for messages.
    pub fn describe(&self) -> &str {
        &self.description
    }

    /// Descendants that match the CSS `selector` (see [`css_steps`] for `:visible`).
    pub fn locator(&self, selector: &str) -> Self {
        self.then(css_steps(selector), format!("locator({selector:?})"))
    }

    /// Like [`Locator::locator`] with Playwright's `{ hasText }` option.
    pub fn locator_has_text(&self, selector: &str, text: impl Into<Pattern>) -> Self {
        let text = text.into();
        let description = format!("locator({selector:?}, {{ hasText: {} }})", text.describe());
        let mut steps = css_steps(selector);
        steps.push(Step::HasText(text));
        self.then(steps, description)
    }

    pub fn get_by_role(&self, role: impl Into<Role>) -> Self {
        let role = role.into();
        let description = role.describe();
        self.then(vec![Step::Role(role)], description)
    }

    pub fn get_by_text(&self, text: impl Into<Pattern>) -> Self {
        let text = text.into();
        let description = format!("getByText({})", describe_with_exact(&text));
        self.then(vec![Step::Text(text)], description)
    }

    pub fn get_by_label(&self, text: impl Into<Pattern>) -> Self {
        let text = text.into();
        let description = format!("getByLabel({})", describe_with_exact(&text));
        self.then(vec![Step::Label(text)], description)
    }

    pub fn get_by_placeholder(&self, text: impl Into<Pattern>) -> Self {
        self.attribute("placeholder", "getByPlaceholder", text.into())
    }

    pub fn get_by_alt_text(&self, text: impl Into<Pattern>) -> Self {
        self.attribute("alt", "getByAltText", text.into())
    }

    pub fn get_by_title(&self, text: impl Into<Pattern>) -> Self {
        self.attribute("title", "getByTitle", text.into())
    }

    pub fn get_by_test_id(&self, id: &str) -> Self {
        self.attribute("data-testid", "getByTestId", exact(id))
    }

    fn attribute(&self, name: &str, method: &str, value: Pattern) -> Self {
        let description = format!("{method}({})", describe_with_exact(&value));
        self.then(
            vec![Step::Attribute {
                name: name.to_owned(),
                value,
            }],
            description,
        )
    }

    /// The `index`th match (from 0; negative counts from the end).
    pub fn nth(&self, index: i64) -> Self {
        self.then(vec![Step::Nth(index)], format!("nth({index})"))
    }

    pub fn first(&self) -> Self {
        self.then(vec![Step::Nth(0)], "first()".into())
    }

    pub fn last(&self) -> Self {
        self.then(vec![Step::Nth(-1)], "last()".into())
    }

    /// Keep matches whose text contains `text` (Playwright's `filter({ hasText })`).
    pub fn filter_has_text(&self, text: impl Into<Pattern>) -> Self {
        let text = text.into();
        let description = format!("filter({{ hasText: {} }})", text.describe());
        self.then(vec![Step::HasText(text)], description)
    }

    /// Keep matches whose text does not contain `text`.
    pub fn filter_has_not_text(&self, text: impl Into<Pattern>) -> Self {
        let text = text.into();
        let description = format!("filter({{ hasNotText: {} }})", text.describe());
        self.then(vec![Step::HasNotText(text)], description)
    }

    /// Keep matches that contain an element `inner` selects. `inner` must be built from the
    /// page (not a frame) and is evaluated inside each match.
    pub fn filter_has(&self, inner: &Locator<'_>) -> Self {
        assert!(
            inner.frames.is_empty(),
            "browser-tests: filter_has takes a same-frame locator"
        );
        let description = format!("filter({{ has: {} }})", inner.description);
        self.then(vec![Step::Has(inner.steps.clone())], description)
    }

    /// Keep matches that contain no element `inner` selects.
    pub fn filter_has_not(&self, inner: &Locator<'_>) -> Self {
        assert!(
            inner.frames.is_empty(),
            "browser-tests: filter_has_not takes a same-frame locator"
        );
        let description = format!("filter({{ hasNot: {} }})", inner.description);
        self.then(vec![Step::HasNot(inner.steps.clone())], description)
    }

    /// Keep visible (or hidden) matches (Playwright's `filter({ visible })`).
    pub fn filter_visible(&self, visible: bool) -> Self {
        self.then(
            vec![Step::Visible(visible)],
            format!("filter({{ visible: {visible} }})"),
        )
    }

    /// The document inside the iframe this locator selects.
    pub fn content_frame(&self) -> FrameLocator<'p> {
        let mut frames = self.frames.clone();
        frames.push(self.steps.clone());
        FrameLocator {
            page: self.page,
            frames,
            description: format!("{}.contentFrame()", self.description),
        }
    }

    /// The iframe this locator selects, entered (Playwright's `frameLocator(selector)` on a
    /// locator).
    pub fn frame_locator(&self, selector: &str) -> FrameLocator<'p> {
        self.locator(selector).content_frame()
    }

    pub(crate) fn steps_json(&self) -> Value {
        steps_json(&self.steps)
    }

    pub(crate) fn strict_error(&self, count: usize, previews: &[String]) -> Error {
        let mut message = format!("{} resolved to {count} elements:", self.description);
        for (index, preview) in previews.iter().enumerate() {
            message.push_str(&format!("\n    {}) {preview}", index + 1));
        }
        Error::Strict(message)
    }

    /// Resolve the frames: the context the last steps run in.
    pub(crate) async fn scope(&self) -> Result<Lookup<Scope>, Error> {
        let page = self.page;
        let Some(mut ctx) = page.main_context() else {
            return Ok(Lookup::Missing("the main frame has no document yet".into()));
        };
        let mut hops = Vec::new();
        for frame_steps in &self.frames {
            let iframe = match page.single(&ctx, frame_steps).await? {
                Lookup::Found(iframe) => iframe,
                Lookup::Missing(reason) => {
                    return Ok(Lookup::Missing(format!("no frame element yet: {reason}")));
                }
                Lookup::Strict(count, previews) => return Ok(Lookup::Strict(count, previews)),
            };
            let node = page
                .session_command(
                    &ctx.session,
                    "DOM.describeNode",
                    json!({"objectId": iframe}),
                )
                .await?;
            let Some(frame_id) = node.pointer("/node/frameId").and_then(Value::as_str) else {
                return Ok(Lookup::Missing(
                    "the frame element has no content frame".into(),
                ));
            };
            let Some(child) = page.frame_context(frame_id) else {
                return Ok(Lookup::Missing(format!(
                    "frame {frame_id} has no document yet"
                )));
            };
            hops.push(Hop {
                ctx: ctx.clone(),
                iframe,
            });
            ctx = child;
        }
        Ok(Lookup::Found(Scope { ctx, hops }))
    }

    pub(crate) async fn release_scope(&self, scope: &Scope) {
        self.page
            .release(
                scope
                    .hops
                    .iter()
                    .map(|hop| (hop.ctx.session.clone(), hop.iframe.clone()))
                    .collect(),
            )
            .await;
    }

    /// The single element, or why not.
    async fn element(&self) -> Result<Lookup<(Scope, String)>, Error> {
        let scope = match self.scope().await? {
            Lookup::Found(scope) => scope,
            Lookup::Missing(reason) => return Ok(Lookup::Missing(reason)),
            Lookup::Strict(count, previews) => return Ok(Lookup::Strict(count, previews)),
        };
        match self.page.single(&scope.ctx, &self.steps).await? {
            Lookup::Found(element) => Ok(Lookup::Found((scope, element))),
            Lookup::Missing(reason) => {
                self.release_scope(&scope).await;
                Ok(Lookup::Missing(reason))
            }
            Lookup::Strict(count, previews) => {
                self.release_scope(&scope).await;
                Ok(Lookup::Strict(count, previews))
            }
        }
    }

    /// The single element; `Err(reason)` while there is none yet. Several are an
    /// [`Error::Strict`].
    async fn resolved(&self) -> Result<Result<(Scope, String), String>, Error> {
        match self.element().await? {
            Lookup::Found(found) => Ok(Ok(found)),
            Lookup::Missing(reason) => Ok(Err(reason)),
            Lookup::Strict(count, previews) => Err(self.strict_error(count, &previews)),
        }
    }

    async fn release_element(&self, scope: &Scope, element: String) {
        let mut objects: Vec<(String, String)> = scope
            .hops
            .iter()
            .map(|hop| (hop.ctx.session.clone(), hop.iframe.clone()))
            .collect();
        objects.push((scope.ctx.session.clone(), element));
        self.page.release(objects).await;
    }

    async fn call_element(
        &self,
        scope: &Scope,
        element: &str,
        params: &str,
        body: &str,
        args: &[Value],
    ) -> Result<Value, Error> {
        let result = self
            .page
            .call_on(&scope.ctx, element, &with_bt(params, body), args, true)
            .await?;
        Ok(result.get("value").cloned().unwrap_or(Value::Null))
    }

    /// `Some(state)` when the element is not yet in all of `states`.
    async fn waiting_for(
        &self,
        scope: &Scope,
        element: &str,
        states: &[&str],
    ) -> Result<Option<String>, Error> {
        let waiting = self
            .call_element(
                scope,
                element,
                "states",
                "return bt.waitingFor(this, states);",
                &[json!(states)],
            )
            .await?;
        Ok((!waiting.is_null()).then(|| {
            format!(
                "element is not {} ({})",
                waiting["state"].as_str().unwrap_or(""),
                waiting["received"].as_str().unwrap_or("")
            )
        }))
    }

    async fn pointer(&self, action: Pointer, options: &ClickOptions) -> Result<(), Error> {
        let timeout = options.timeout.unwrap_or(self.page.timeout);
        retry!(self, action.name(), timeout, |attempt| self
            .try_pointer(action, options, attempt))
    }

    async fn try_pointer(
        &self,
        action: Pointer,
        options: &ClickOptions,
        attempt: u32,
    ) -> Result<Attempt<()>, Error> {
        let (scope, element) = match self.resolved().await? {
            Ok(found) => found,
            Err(reason) => return Ok(Attempt::Retry(reason)),
        };
        let result = self
            .pointer_on(&scope, &element, action, options, attempt)
            .await;
        self.release_element(&scope, element).await;
        result
    }

    async fn pointer_on(
        &self,
        scope: &Scope,
        element: &str,
        action: Pointer,
        options: &ClickOptions,
        attempt: u32,
    ) -> Result<Attempt<()>, Error> {
        let page = self.page;
        if !options.force {
            let states: &[&str] = match action {
                Pointer::Hover => &["visible"],
                _ => &["visible", "enabled"],
            };
            if let Some(reason) = self.waiting_for(scope, element, states).await? {
                return Ok(Attempt::Retry(reason));
            }
        }
        // Scroll: first as DevTools does it (across processes), then with alternating alignments,
        // as Playwright retries.
        let scrolled = if attempt.is_multiple_of(4) {
            page.session_command(
                &scope.ctx.session,
                "DOM.scrollIntoViewIfNeeded",
                json!({"objectId": element}),
            )
            .await
            .is_ok()
        } else {
            false
        };
        if !scrolled {
            let alignment = ["end", "center", "start"][(attempt % 3) as usize];
            self.call_element(
                scope,
                element,
                "alignment",
                "bt.scroll(this, alignment);",
                &[json!(alignment)],
            )
            .await?;
        }
        if !options.force {
            let stable = self
                .call_element(scope, element, "", "return bt.stable(this);", &[])
                .await?;
            if stable != Value::Bool(true) {
                return Ok(Attempt::Retry("element is not stable".into()));
            }
        }
        let origin = page.frame_origin(scope).await?;
        let point = match options.position {
            Some((x, y)) => {
                let bounds = self
                    .call_element(scope, element, "", "return bt.box(this);", &[])
                    .await?;
                if bounds.is_null() {
                    return Ok(Attempt::Retry("element has no layout box".into()));
                }
                (
                    origin.0 + number(&bounds, "/x") + x,
                    origin.1 + number(&bounds, "/y") + y,
                )
            }
            None => {
                let rects = self
                    .call_element(scope, element, "", "return bt.rects(this);", &[])
                    .await?;
                let viewport = page.viewport().await?;
                match clickable_point(&rects, origin, viewport) {
                    Some(point) => point,
                    None => {
                        return Ok(Attempt::Retry("element is outside of the viewport".into()));
                    }
                }
            }
        };
        if !options.force {
            let local = [json!(point.0 - origin.0), json!(point.1 - origin.1)];
            let hit = self
                .call_element(scope, element, "x, y", "return bt.hit(this, x, y);", &local)
                .await?;
            if hit.as_str() != Some("done") {
                return Ok(Attempt::Retry(format!(
                    "{} intercepts pointer events",
                    hit.as_str().unwrap_or("another element")
                )));
            }
            // The iframes on the way must receive the point too.
            let mut parent = (0.0, 0.0);
            for hop in &scope.hops {
                let local = [json!(point.0 - parent.0), json!(point.1 - parent.1)];
                let hit = page
                    .call_on(
                        &hop.ctx,
                        &hop.iframe,
                        &with_bt("x, y", "return bt.hit(this, x, y, 'none');"),
                        &local,
                        true,
                    )
                    .await?;
                let hit = hit.get("value").cloned().unwrap_or(Value::Null);
                if hit.as_str() != Some("done") {
                    return Ok(Attempt::Retry(format!(
                        "{} intercepts pointer events before the frame",
                        hit.as_str().unwrap_or("another element")
                    )));
                }
                let offset = page
                    .call_on(
                        &hop.ctx,
                        &hop.iframe,
                        &with_bt("", "return bt.frameOffset(this);"),
                        &[],
                        true,
                    )
                    .await?;
                parent.0 += number(&offset, "/value/x");
                parent.1 += number(&offset, "/value/y");
            }
        }
        if !options.force {
            let kind = match action {
                Pointer::Click { .. } => "mouse",
                Pointer::Hover => "hover",
                Pointer::Tap => "tap",
            };
            self.call_element(
                scope,
                element,
                "kind",
                "bt.intercept(this, kind);",
                &[json!(kind)],
            )
            .await?;
        }
        match action {
            Pointer::Click { button, count } => {
                page.mouse()
                    .click_with(point.0, point.1, button, count)
                    .await?;
            }
            Pointer::Hover => page.mouse().move_to(point.0, point.1, 1).await?,
            Pointer::Tap => page.touchscreen().tap(point.0, point.1).await?,
        }
        if !options.force {
            // A navigation may have replaced the document: that counts as a hit, as in
            // Playwright.
            let intercepted = page
                .call_in(
                    &scope.ctx,
                    &with_bt("", "return bt.stopIntercept();"),
                    &[],
                    true,
                )
                .await
                .ok()
                .and_then(|result| {
                    result
                        .get("value")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                })
                .unwrap_or_else(|| "done".to_owned());
            if intercepted != "done" {
                return Ok(Attempt::Retry(format!(
                    "{intercepted} intercepts pointer events (the input was blocked)"
                )));
            }
        }
        Ok(Attempt::Done(()))
    }

    /// Click the element's centre after the actionability checks.
    pub async fn click(&self) -> Result<(), Error> {
        self.click_with(ClickOptions::default()).await
    }

    pub async fn click_with(&self, options: ClickOptions) -> Result<(), Error> {
        let action = Pointer::Click {
            button: options.button,
            count: options.click_count.max(1),
        };
        self.pointer(action, &options).await
    }

    pub async fn dblclick(&self) -> Result<(), Error> {
        self.click_with(ClickOptions {
            click_count: 2,
            ..ClickOptions::default()
        })
        .await
    }

    /// Move the mouse over the element (visible, stable, hit target; enabled is not required).
    pub async fn hover(&self) -> Result<(), Error> {
        self.pointer(Pointer::Hover, &ClickOptions::default()).await
    }

    /// Tap the element's centre (needs [`Page::set_has_touch`]).
    pub async fn tap(&self) -> Result<(), Error> {
        self.pointer(Pointer::Tap, &ClickOptions::default()).await
    }

    /// Replace the value of an `<input>`, `<textarea>` or `[contenteditable]` (Playwright's
    /// `fill`).
    pub async fn fill(&self, value: &str) -> Result<(), Error> {
        retry!(self, "fill", self.page.timeout, |_attempt| self
            .try_fill(value))
    }

    async fn try_fill(&self, value: &str) -> Result<Attempt<()>, Error> {
        let (scope, element) = match self.resolved().await? {
            Ok(found) => found,
            Err(reason) => return Ok(Attempt::Retry(reason)),
        };
        let result = self.fill_on(&scope, &element, value).await;
        self.release_element(&scope, element).await;
        result
    }

    async fn fill_on(
        &self,
        scope: &Scope,
        element: &str,
        value: &str,
    ) -> Result<Attempt<()>, Error> {
        if let Some(reason) = self
            .waiting_for(scope, element, &["visible", "enabled", "editable"])
            .await?
        {
            return Ok(Attempt::Retry(reason));
        }
        let outcome = self
            .call_element(
                scope,
                element,
                "value",
                "return bt.fill(this, value);",
                &[json!(value)],
            )
            .await?;
        match outcome.as_str() {
            Some("needsinput") if value.is_empty() => {
                self.page.keyboard().press("Delete").await?;
            }
            Some("needsinput") => self.page.keyboard().insert_text(value).await?,
            Some("done") => {}
            other => {
                return Ok(Attempt::Retry(other.unwrap_or("not connected").to_owned()));
            }
        }
        Ok(Attempt::Done(()))
    }

    /// Empty the field (`fill("")`).
    pub async fn clear(&self) -> Result<(), Error> {
        self.fill("").await
    }

    /// Focus the element and press `key` (a key name or combination, see
    /// [`crate::Keyboard::press`]).
    pub async fn press(&self, key: &str) -> Result<(), Error> {
        self.focus_element("press").await?;
        self.page.keyboard().press(key).await
    }

    /// Focus the element and type `text` key by key (Playwright's `pressSequentially`).
    pub async fn press_sequentially(&self, text: &str) -> Result<(), Error> {
        self.focus_element("type").await?;
        self.page.keyboard().type_text(text).await
    }

    /// Run `body` (JavaScript with `bt` and `this`, the element) on the single element once it
    /// exists, and return its JSON result.
    async fn run_on_element(&self, what: &str, body: &str, args: &[Value]) -> Result<Value, Error> {
        retry!(self, what, self.page.timeout, |_attempt| self
            .try_run_on_element(body, args))
    }

    async fn try_run_on_element(
        &self,
        body: &str,
        args: &[Value],
    ) -> Result<Attempt<Value>, Error> {
        let (scope, element) = match self.resolved().await? {
            Ok(found) => found,
            Err(reason) => return Ok(Attempt::Retry(reason)),
        };
        let result = self.call_element(&scope, &element, "arg", body, args).await;
        self.release_element(&scope, element).await;
        result.map(Attempt::Done)
    }

    async fn focus_element(&self, what: &str) -> Result<(), Error> {
        self.run_on_element(what, "bt.focus(this, true);", &[])
            .await?;
        Ok(())
    }

    /// Focus the element.
    pub async fn focus(&self) -> Result<(), Error> {
        self.run_on_element("focus", "bt.focus(this, false);", &[])
            .await?;
        Ok(())
    }

    /// Remove focus from the element.
    pub async fn blur(&self) -> Result<(), Error> {
        self.evaluate("(element) => element.blur()", Value::Null)
            .await?;
        Ok(())
    }

    /// Select options of a `<select>` by value or label; returns the selected values
    /// (Playwright's `selectOption`).
    pub async fn select_option(&self, values: &[&str]) -> Result<Vec<String>, Error> {
        retry!(self, "selectOption", self.page.timeout, |_attempt| self
            .try_select(values))
    }

    async fn try_select(&self, values: &[&str]) -> Result<Attempt<Vec<String>>, Error> {
        let (scope, element) = match self.resolved().await? {
            Ok(found) => found,
            Err(reason) => return Ok(Attempt::Retry(reason)),
        };
        let result = self.select_on(&scope, &element, values).await;
        self.release_element(&scope, element).await;
        result
    }

    async fn select_on(
        &self,
        scope: &Scope,
        element: &str,
        values: &[&str],
    ) -> Result<Attempt<Vec<String>>, Error> {
        if let Some(reason) = self
            .waiting_for(scope, element, &["visible", "enabled"])
            .await?
        {
            return Ok(Attempt::Retry(reason));
        }
        let selected = self
            .call_element(
                scope,
                element,
                "values",
                "return bt.selectOptions(this, values);",
                &[json!(values)],
            )
            .await?;
        Ok(match selected {
            Value::Array(values) => Attempt::Done(
                values
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect(),
            ),
            other => Attempt::Retry(format!(
                "{} (wanted {values:?})",
                other.as_str().unwrap_or("options not found")
            )),
        })
    }

    async fn checked_state(&self) -> Result<bool, Error> {
        match self
            .run_on_element("isChecked", "return bt.checked(this);", &[])
            .await?
        {
            Value::Bool(checked) => Ok(checked),
            _ => Err(Error::Evaluation("Not a checkbox or radio button".into())),
        }
    }

    /// Check a checkbox or radio button (click it unless it is checked already).
    pub async fn check(&self) -> Result<(), Error> {
        self.set_checked(true).await
    }

    pub async fn uncheck(&self) -> Result<(), Error> {
        self.set_checked(false).await
    }

    pub async fn set_checked(&self, checked: bool) -> Result<(), Error> {
        if self.checked_state().await? == checked {
            return Ok(());
        }
        self.click().await?;
        if self.checked_state().await? != checked {
            return Err(Error::Evaluation(
                "Clicking the checkbox did not change its state".into(),
            ));
        }
        Ok(())
    }

    /// Dispatch a DOM event of `kind` (bubbling, composed) on the element.
    pub async fn dispatch_event(&self, kind: &str) -> Result<(), Error> {
        self.evaluate(
            "(element, kind) => element.dispatchEvent(new Event(kind, { bubbles: true, cancelable: true, composed: true }))",
            json!(kind),
        )
        .await?;
        Ok(())
    }

    /// Scroll the element into view if it is not.
    pub async fn scroll_into_view_if_needed(&self) -> Result<(), Error> {
        self.run_on_element(
            "scrollIntoViewIfNeeded",
            "this.scrollIntoViewIfNeeded ? this.scrollIntoViewIfNeeded(true) : bt.scroll(this, 'center');",
            &[],
        )
        .await?;
        Ok(())
    }

    /// Call the JavaScript function `function` (source text) as `function(element, arg)` in the
    /// element's frame and return its awaited result as JSON (Playwright's `locator.evaluate`).
    pub async fn evaluate(&self, function: &str, arg: Value) -> Result<Value, Error> {
        self.run_on_element(
            "evaluate",
            &format!("return ({function})(this, arg);"),
            &[arg],
        )
        .await
    }

    /// `element.textContent`.
    pub async fn text_content(&self) -> Result<Option<String>, Error> {
        let text = self
            .evaluate("(element) => element.textContent", Value::Null)
            .await?;
        Ok(text.as_str().map(str::to_owned))
    }

    /// `element.innerText`.
    pub async fn inner_text(&self) -> Result<String, Error> {
        let text = self
            .evaluate("(element) => element.innerText", Value::Null)
            .await?;
        Ok(text.as_str().unwrap_or("").to_owned())
    }

    /// The value of an `<input>`, `<textarea>` or `<select>`.
    pub async fn input_value(&self) -> Result<String, Error> {
        let value = self
            .evaluate(
                "(element) => { if (!['INPUT', 'TEXTAREA', 'SELECT'].includes(element.nodeName)) throw new Error('Not an input element'); return element.value; }",
                Value::Null,
            )
            .await?;
        Ok(value.as_str().unwrap_or("").to_owned())
    }

    pub async fn get_attribute(&self, name: &str) -> Result<Option<String>, Error> {
        let value = self
            .evaluate("(element, name) => element.getAttribute(name)", json!(name))
            .await?;
        Ok(value.as_str().map(str::to_owned))
    }

    /// The element's border box in the main frame's viewport, or `None` when it is not rendered
    /// (Playwright's `boundingBox`).
    pub async fn bounding_box(&self) -> Result<Option<BoundingBox>, Error> {
        retry!(self, "boundingBox", self.page.timeout, |_attempt| self
            .try_bounding_box())
    }

    async fn try_bounding_box(&self) -> Result<Attempt<Option<BoundingBox>>, Error> {
        let (scope, element) = match self.resolved().await? {
            Ok(found) => found,
            Err(reason) => return Ok(Attempt::Retry(reason)),
        };
        let bounds = self
            .call_element(&scope, &element, "", "return bt.box(this);", &[])
            .await;
        let origin = self.page.frame_origin(&scope).await;
        self.release_element(&scope, element).await;
        let (bounds, origin) = (bounds?, origin?);
        if bounds.is_null() {
            return Ok(Attempt::Done(None));
        }
        Ok(Attempt::Done(Some(BoundingBox {
            x: origin.0 + number(&bounds, "/x"),
            y: origin.1 + number(&bounds, "/y"),
            width: number(&bounds, "/width"),
            height: number(&bounds, "/height"),
        })))
    }

    /// The accessible name the role engine computes (for debugging role queries).
    pub async fn accessible_name(&self) -> Result<String, Error> {
        let name = self
            .run_on_element(
                "accessibleName",
                "return bt.accessibleName(this, false);",
                &[],
            )
            .await?;
        Ok(name.as_str().unwrap_or("").to_owned())
    }

    /// How many elements match now (no waiting).
    pub async fn count(&self) -> Result<usize, Error> {
        let scope = match self.scope().await? {
            Lookup::Found(scope) => scope,
            Lookup::Missing(_) => return Ok(0),
            Lookup::Strict(count, previews) => return Err(self.strict_error(count, &previews)),
        };
        let count = self
            .page
            .call_in(
                &scope.ctx,
                &with_bt("steps", "return bt.count(steps);"),
                &[self.steps_json()],
                true,
            )
            .await;
        self.release_scope(&scope).await;
        Ok(count?
            .get("value")
            .and_then(Value::as_u64)
            .and_then(|count| usize::try_from(count).ok())
            .unwrap_or(0))
    }

    /// `textContent` of every match, now (no waiting).
    pub async fn all_text_contents(&self) -> Result<Vec<String>, Error> {
        let scope = match self.scope().await? {
            Lookup::Found(scope) => scope,
            Lookup::Missing(_) => return Ok(Vec::new()),
            Lookup::Strict(count, previews) => return Err(self.strict_error(count, &previews)),
        };
        let texts = self
            .page
            .call_in(
                &scope.ctx,
                &with_bt(
                    "steps",
                    "return bt.query(steps).map((element) => element.textContent || '');",
                ),
                &[self.steps_json()],
                true,
            )
            .await;
        self.release_scope(&scope).await;
        Ok(texts?
            .get("value")
            .and_then(Value::as_array)
            .map(|list| {
                list.iter()
                    .map(|text| text.as_str().unwrap_or("").to_owned())
                    .collect()
            })
            .unwrap_or_default())
    }

    /// Call the JavaScript function `function` (source text) as `function(elements, arg)` with
    /// every element that matches now, in document order, and return its awaited result as JSON
    /// (Playwright's `locator.evaluateAll`). It waits for the locator's frame, not for elements:
    /// no match calls `function([], arg)`.
    pub async fn evaluate_all(&self, function: &str, arg: Value) -> Result<Value, Error> {
        retry!(self, "evaluateAll", self.page.timeout, |_attempt| self
            .try_evaluate_all(function, &arg))
    }

    async fn try_evaluate_all(&self, function: &str, arg: &Value) -> Result<Attempt<Value>, Error> {
        let scope = match self.scope().await? {
            Lookup::Found(scope) => scope,
            Lookup::Missing(reason) => return Ok(Attempt::Retry(reason)),
            Lookup::Strict(count, previews) => return Err(self.strict_error(count, &previews)),
        };
        let result = self
            .page
            .call_in(
                &scope.ctx,
                &with_bt(
                    "steps, arg",
                    &format!("return ({function})(bt.query(steps), arg);"),
                ),
                &[self.steps_json(), arg.clone()],
                true,
            )
            .await;
        self.release_scope(&scope).await;
        let result = result?;
        if is_missing_helper(&result) {
            return Ok(Attempt::Retry(
                "the locator engine is not installed yet".into(),
            ));
        }
        Ok(Attempt::Done(
            result.get("value").cloned().unwrap_or(Value::Null),
        ))
    }

    async fn state_now(&self, state: &str) -> Result<bool, Error> {
        match self.element().await? {
            Lookup::Missing(_) => Ok(state == "hidden"),
            Lookup::Strict(count, previews) => Err(self.strict_error(count, &previews)),
            Lookup::Found((scope, element)) => {
                let result = self
                    .call_element(
                        &scope,
                        &element,
                        "state",
                        "return bt.state(this, state).matches;",
                        &[json!(state)],
                    )
                    .await;
                self.release_element(&scope, element).await;
                Ok(result? == Value::Bool(true))
            }
        }
    }

    /// Whether the single match is visible now (no waiting; no match is `false`).
    pub async fn is_visible(&self) -> Result<bool, Error> {
        self.state_now("visible").await
    }

    pub async fn is_hidden(&self) -> Result<bool, Error> {
        self.state_now("hidden").await
    }

    pub async fn is_enabled(&self) -> Result<bool, Error> {
        self.state_now("enabled").await
    }

    pub async fn is_checked(&self) -> Result<bool, Error> {
        self.state_now("checked").await
    }

    /// Wait until the locator is attached, detached, visible or hidden.
    pub async fn wait_for(&self, state: WaitState) -> Result<(), Error> {
        retry!(self, "waitFor", self.page.timeout, |_attempt| self
            .try_wait_for(state))
    }

    async fn try_wait_for(&self, state: WaitState) -> Result<Attempt<()>, Error> {
        let count = self.count().await?;
        let done = match state {
            WaitState::Attached => count > 0,
            WaitState::Detached => count == 0,
            WaitState::Visible => count > 0 && self.first().is_visible().await?,
            WaitState::Hidden => count == 0 || !self.first().is_visible().await?,
        };
        Ok(if done {
            Attempt::Done(())
        } else {
            Attempt::Retry(format!("waiting for {state:?}"))
        })
    }
}

fn describe_with_exact(pattern: &Pattern) -> String {
    match pattern {
        Pattern::Exact(_) => format!("{}, {{ exact: true }}", pattern.describe()),
        other => other.describe(),
    }
}

/// The centre of the first client rectangle that is in view, clipped by the element's frame and
/// by the main viewport, in main-viewport coordinates.
fn clickable_point(rects: &Value, origin: (f64, f64), viewport: (f64, f64)) -> Option<(f64, f64)> {
    let frame_width = number(rects, "/viewport/width");
    let frame_height = number(rects, "/viewport/height");
    for rect in rects.get("rects")?.as_array()? {
        let (x, y) = (number(rect, "/x"), number(rect, "/y"));
        let (width, height) = (number(rect, "/width"), number(rect, "/height"));
        // Clip to the frame's viewport, then to the main viewport.
        let left = x.max(0.0) + origin.0;
        let top = y.max(0.0) + origin.1;
        let right = (x + width).min(frame_width) + origin.0;
        let bottom = (y + height).min(frame_height) + origin.1;
        let left = left.max(0.0);
        let top = top.max(0.0);
        let right = right.min(viewport.0);
        let bottom = bottom.min(viewport.1);
        if (right - left) * (bottom - top) > 0.99 && right > left && bottom > top {
            return Some(((left + right) / 2.0, (top + bottom) / 2.0));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn visible_pseudo_class_becomes_a_filter() {
        assert_eq!(
            css_steps("#artifact-grid > .card:visible .card-title"),
            vec![
                Step::Css("#artifact-grid > .card".into()),
                Step::Visible(true),
                Step::Css(".card-title".into()),
            ]
        );
        assert_eq!(
            css_steps(".a:visible > .b"),
            vec![
                Step::Css(".a".into()),
                Step::Visible(true),
                Step::Css(":scope > .b".into()),
            ]
        );
        assert_eq!(
            css_steps("button:visible"),
            vec![Step::Css("button".into()), Step::Visible(true)]
        );
        assert_eq!(
            css_steps("[title=':visible'] .x"),
            vec![Step::Css("[title=':visible'] .x".into())]
        );
        assert_eq!(
            css_steps(".visible-thing"),
            vec![Step::Css(".visible-thing".into())]
        );
    }

    #[test]
    fn clickable_point_clips_to_frame_and_viewport() {
        let rects = json!({"rects": [{"x": -10, "y": 10, "width": 40, "height": 20}], "viewport": {"width": 100, "height": 100}});
        assert_eq!(
            clickable_point(&rects, (0.0, 0.0), (800.0, 600.0)),
            Some((15.0, 20.0))
        );
        assert_eq!(
            clickable_point(&rects, (10.0, 5.0), (800.0, 600.0)),
            Some((25.0, 25.0))
        );
        let outside = json!({"rects": [{"x": 0, "y": 700, "width": 40, "height": 20}], "viewport": {"width": 800, "height": 600}});
        assert_eq!(clickable_point(&outside, (0.0, 0.0), (800.0, 600.0)), None);
    }

    #[test]
    fn descriptions_read_like_playwright() {
        let role = Role::new("button").exact_name("Move");
        assert_eq!(
            role.describe(),
            "getByRole(\"button\", { name: \"Move\", exact: true })"
        );
        assert_eq!(regex("Open Thread", "i").describe(), "/Open Thread/i");
    }
}
