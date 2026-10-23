// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Neil Blackman
//! [`Page::expose_function`]: a page function that calls into Rust (Playwright's
//! `exposeFunction`), built on DevTools `Runtime.addBinding` and `Runtime.bindingCalled`.
//!
//! The binding itself only takes one string and returns nothing. A small wrapper script turns it
//! into `window[name](...args)`, which sends `{id, args}` as JSON and returns a promise. A task on
//! the Rust side answers each `Runtime.bindingCalled` event by running the handler and resolving
//! (or rejecting) that promise in the execution context that called it.

use std::sync::{Arc, Weak};
use std::time::Duration;

use serde_json::{Value, json};

use crate::connection::{Connection, Subscription, lock};
use crate::{Error, Page};

/// Prefix of the raw DevTools binding behind an exposed function.
const BINDING_PREFIX: &str = "__browserTestsBinding_";

/// What an exposed function's handler gets (the call's arguments, JSON) and returns (the value
/// the page's promise resolves to, or a message it rejects with).
type Handler = Arc<dyn Fn(Vec<Value>) -> Result<Value, String> + Send + Sync>;

/// The page-side wrapper: defines `globalThis[name]` over the binding, once per global.
fn wrapper(name: &str, binding: &str) -> String {
    let name = Value::from(name).to_string();
    let binding = Value::from(binding).to_string();
    format!(
        r#"(() => {{
  const name = {name}, binding = globalThis[{binding}];
  const deliver = Symbol.for("browser-tests:expose:" + name);
  if (typeof binding !== "function" || globalThis[deliver]) return;
  const pending = new Map();
  let next = 0;
  Object.defineProperty(globalThis, deliver, {{
    configurable: true,
    value: (id, ok, value) => {{
      const call = pending.get(id);
      if (!call) return;
      pending.delete(id);
      if (ok) call.resolve(value); else call.reject(new Error(value));
    }},
  }});
  Object.defineProperty(globalThis, name, {{
    configurable: true,
    writable: true,
    value: (...args) => new Promise((resolve, reject) => {{
      const id = ++next;
      pending.set(id, {{ resolve, reject }});
      binding(JSON.stringify({{ id, args }}));
    }}),
  }});
}})();"#
    )
}

impl Page {
    /// Define `window[name]` in the page: calling it with JSON-serialisable arguments runs
    /// `handler` in Rust with those arguments, and the returned promise resolves to the
    /// handler's value (or rejects with `Error(message)` for `Err(message)`), as Playwright's
    /// `page.exposeFunction` does.
    ///
    /// The function exists in the current documents of the page and its frames, and in every
    /// new document from now on (navigations, `set_content`). Out-of-process iframes that attach
    /// after this call get the wrapper but not the binding behind it, so there the function does
    /// not exist. Arguments go through `JSON.stringify`, so `undefined` becomes `null` and
    /// functions are dropped.
    ///
    /// ```no_run
    /// # async fn example(page: &browser_tests::Page) -> Result<(), browser_tests::Error> {
    /// use std::sync::{Arc, Mutex};
    /// let seen = Arc::new(Mutex::new(Vec::new()));
    /// let sink = Arc::clone(&seen);
    /// page.expose_function("capture", move |args| {
    ///     sink.lock().unwrap().push(args);
    ///     Ok(serde_json::Value::Null)
    /// })
    /// .await?;
    /// page.evaluate("capture({ type: 'ready' })").await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn expose_function(
        &self,
        name: &str,
        handler: impl Fn(Vec<Value>) -> Result<Value, String> + Send + Sync + 'static,
    ) -> Result<(), Error> {
        let binding = format!("{BINDING_PREFIX}{name}");
        // Subscribe before the binding exists, so no call is missed.
        let calls = self.subscribe_all("Runtime.bindingCalled");
        for session in self.state.frame_sessions() {
            let result = self
                .session_command(&session, "Runtime.addBinding", json!({"name": binding}))
                .await;
            if session == self.session {
                result?;
            }
        }
        let source = wrapper(name, &binding);
        self.add_init_script(&source).await?;
        let contexts: Vec<(String, i64)> = lock(&self.state.contexts)
            .iter()
            .filter(|context| context.is_default)
            .map(|context| (context.session.clone(), context.id))
            .collect();
        for (session, context) in contexts {
            // A context that is going away is not an error.
            let _ = self
                .session_command(
                    &session,
                    "Runtime.evaluate",
                    json!({"expression": source, "contextId": context}),
                )
                .await;
        }
        tokio::spawn(answer_calls(
            Arc::downgrade(&self.connection),
            calls,
            binding,
            name.to_owned(),
            Arc::new(handler),
            self.timeout,
        ));
        Ok(())
    }
}

/// Answer the binding's calls until the page (its event hub) or the browser goes away.
async fn answer_calls(
    connection: Weak<Connection>,
    mut calls: Subscription,
    binding: String,
    name: String,
    handler: Handler,
    timeout: Duration,
) {
    let deliver = format!(
        "globalThis[Symbol.for({})]",
        Value::from(format!("browser-tests:expose:{name}"))
    );
    loop {
        let event = match calls.next(Duration::from_secs(3600)).await {
            Ok(event) => event,
            Err(Error::Timeout { .. }) => continue,
            Err(_) => return,
        };
        let params = &event["params"];
        if params.get("name").and_then(Value::as_str) != Some(binding.as_str()) {
            continue;
        }
        let Some(session) = event.get("sessionId").and_then(Value::as_str) else {
            continue;
        };
        let payload: Value = params
            .get("payload")
            .and_then(Value::as_str)
            .and_then(|payload| serde_json::from_str(payload).ok())
            .unwrap_or(Value::Null);
        let id = payload.get("id").cloned().unwrap_or(Value::Null);
        let args = match payload.get("args") {
            Some(Value::Array(args)) => args.clone(),
            _ => Vec::new(),
        };
        let (ok, value) = match handler(args) {
            Ok(value) => (true, value),
            Err(message) => (false, Value::from(message)),
        };
        let Some(live) = connection.upgrade() else {
            return;
        };
        let expression = format!("{deliver}({id}, {ok}, {value})");
        let _ = live
            .call(
                Some(session),
                "Runtime.evaluate",
                json!({
                    "expression": expression,
                    "contextId": params.get("executionContextId").cloned().unwrap_or(Value::Null),
                }),
                timeout,
            )
            .await;
    }
}
