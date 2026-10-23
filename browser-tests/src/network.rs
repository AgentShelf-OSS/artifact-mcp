// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Neil Blackman
//! The page's network traffic, from DevTools `Network` events of every session of the page
//! (the page and its out-of-process iframes), and waiters for requests and responses.

use std::collections::HashMap;
use std::future::{Future, IntoFuture};
use std::pin::Pin;
use std::sync::Mutex;
use std::time::Duration;

use base64::Engine;
use serde_json::Value;
use tokio::sync::mpsc;

use crate::Error;
use crate::connection::lock;

/// Values sent to every current subscriber.
pub(crate) struct Broadcast<T> {
    subscribers: Mutex<Vec<mpsc::UnboundedSender<T>>>,
}

impl<T> Default for Broadcast<T> {
    fn default() -> Self {
        Self {
            subscribers: Mutex::new(Vec::new()),
        }
    }
}

impl<T: Clone> Broadcast<T> {
    pub(crate) fn subscribe(&self) -> mpsc::UnboundedReceiver<T> {
        let (sender, receiver) = mpsc::unbounded_channel();
        lock(&self.subscribers).push(sender);
        receiver
    }

    pub(crate) fn send(&self, value: &T) {
        lock(&self.subscribers).retain(|sender| sender.send(value.clone()).is_ok());
    }
}

/// A request the page made, as `Network.requestWillBeSent` reported it.
#[derive(Debug, Clone, PartialEq)]
pub struct NetworkRequest {
    pub url: String,
    pub method: String,
    /// The headers the page asked for, names in lower case: Playwright's `request.headers()`.
    /// Headers the network stack adds later (cookies, `sec-fetch-*`, sometimes `origin`) are in
    /// [`crate::Page::all_request_headers`].
    pub headers: Vec<(String, String)>,
    /// The body, when it is text the browser reported inline.
    pub post_data: Option<String>,
    /// `Document`, `Fetch`, `XHR`, `Script`, `Stylesheet`, ...
    pub resource_type: String,
    pub frame_id: Option<String>,
    /// Whether this request loads a document into a frame.
    pub is_navigation: bool,
    pub(crate) id: String,
    pub(crate) session: String,
}

impl NetworkRequest {
    /// A header by case-insensitive name.
    pub fn header(&self, name: &str) -> Option<&str> {
        find_header(&self.headers, name)
    }

    /// The body parsed as JSON (Playwright's `postDataJSON()`).
    pub fn post_data_json(&self) -> Option<Value> {
        self.post_data
            .as_deref()
            .and_then(|body| serde_json::from_str(body).ok())
    }
}

/// A response the page received (`Network.responseReceived`, or a redirect).
#[derive(Debug, Clone, PartialEq)]
pub struct NetworkResponse {
    pub url: String,
    pub status: u16,
    pub status_text: String,
    /// Response headers, names in lower case.
    pub headers: Vec<(String, String)>,
    pub request: NetworkRequest,
    pub(crate) id: String,
    pub(crate) session: String,
}

impl NetworkResponse {
    pub fn header(&self, name: &str) -> Option<&str> {
        find_header(&self.headers, name)
    }

    /// Status 200–299.
    pub fn ok(&self) -> bool {
        (200..300).contains(&self.status)
    }
}

pub(crate) fn find_header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
}

fn header_list(headers: Option<&Value>) -> Vec<(String, String)> {
    headers
        .and_then(Value::as_object)
        .map(|object| {
            object
                .iter()
                .map(|(name, value)| {
                    (
                        name.to_ascii_lowercase(),
                        value.as_str().unwrap_or("").to_owned(),
                    )
                })
                .collect()
        })
        .unwrap_or_default()
}

/// New requests and responses from one event.
#[derive(Default)]
pub(crate) struct NetworkEvents {
    pub(crate) requests: Vec<NetworkRequest>,
    pub(crate) responses: Vec<NetworkResponse>,
}

#[derive(Default)]
pub(crate) struct NetworkLog {
    requests: HashMap<String, NetworkRequest>,
    responses: HashMap<String, NetworkResponse>,
    /// `requestWillBeSentExtraInfo` headers: what actually went on the wire.
    extra_headers: HashMap<String, Vec<(String, String)>>,
    /// `Ok` after `loadingFinished`, the error text after `loadingFailed`.
    finished: HashMap<String, Result<(), String>>,
    /// Every request in the order it was sent (each redirect hop is its own entry).
    history: Vec<NetworkRequest>,
}

impl NetworkLog {
    pub(crate) fn on_event(
        &mut self,
        session: &str,
        method: &str,
        params: &Value,
    ) -> NetworkEvents {
        let mut events = NetworkEvents::default();
        let id = params
            .get("requestId")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        match method {
            "Network.requestWillBeSent" => {
                // A redirect ends the previous request with its redirect response.
                if let (Some(redirect), Some(previous)) =
                    (params.get("redirectResponse"), self.requests.get(&id))
                {
                    let response = response_from(redirect, previous.clone(), &id, session);
                    self.responses.insert(id.clone(), response.clone());
                    events.responses.push(response);
                }
                let request = &params["request"];
                let text = |pointer: &str| {
                    request
                        .pointer(pointer)
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                };
                let post_data = text("/postData").or_else(|| {
                    let entries = request.get("postDataEntries")?.as_array()?;
                    let mut body = Vec::new();
                    for entry in entries {
                        let bytes = entry.get("bytes").and_then(Value::as_str).unwrap_or("");
                        body.extend(
                            base64::engine::general_purpose::STANDARD
                                .decode(bytes)
                                .ok()?,
                        );
                    }
                    String::from_utf8(body).ok()
                });
                let resource_type = params
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned();
                let loader = params.get("loaderId").and_then(Value::as_str);
                let request = NetworkRequest {
                    url: format!(
                        "{}{}",
                        text("/url").unwrap_or_default(),
                        text("/urlFragment").unwrap_or_default()
                    ),
                    method: text("/method").unwrap_or_else(|| "GET".into()),
                    headers: header_list(request.get("headers")),
                    post_data,
                    is_navigation: resource_type == "Document" && loader == Some(id.as_str()),
                    resource_type,
                    frame_id: params
                        .get("frameId")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                    id: id.clone(),
                    session: session.to_owned(),
                };
                self.finished.remove(&id);
                self.requests.insert(id, request.clone());
                self.history.push(request.clone());
                events.requests.push(request);
            }
            "Network.requestWillBeSentExtraInfo" => {
                self.extra_headers
                    .insert(id, header_list(params.get("headers")));
            }
            "Network.responseReceived" => {
                if let Some(request) = self.requests.get(&id) {
                    let response =
                        response_from(&params["response"], request.clone(), &id, session);
                    self.responses.insert(id, response.clone());
                    events.responses.push(response);
                }
            }
            "Network.loadingFinished" => {
                self.finished.insert(id, Ok(()));
            }
            "Network.loadingFailed" => {
                let error = params
                    .get("errorText")
                    .and_then(Value::as_str)
                    .unwrap_or("failed")
                    .to_owned();
                self.finished.insert(id, Err(error));
            }
            _ => {}
        }
        events
    }

    pub(crate) fn history(&self) -> Vec<NetworkRequest> {
        self.history.clone()
    }

    pub(crate) fn response(&self, id: &str) -> Option<NetworkResponse> {
        self.responses.get(id).cloned()
    }

    pub(crate) fn extra_headers(&self, id: &str) -> Option<Vec<(String, String)>> {
        self.extra_headers.get(id).cloned()
    }

    pub(crate) fn finished(&self, id: &str) -> Option<Result<(), String>> {
        self.finished.get(id).cloned()
    }
}

fn response_from(
    response: &Value,
    request: NetworkRequest,
    id: &str,
    session: &str,
) -> NetworkResponse {
    NetworkResponse {
        url: response
            .get("url")
            .and_then(Value::as_str)
            .unwrap_or(&request.url)
            .to_owned(),
        status: response
            .get("status")
            .and_then(Value::as_u64)
            .and_then(|status| u16::try_from(status).ok())
            .unwrap_or(0),
        status_text: response
            .get("statusText")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned(),
        headers: header_list(response.get("headers")),
        request,
        id: id.to_owned(),
        session: session.to_owned(),
    }
}

type Predicate<T> = Box<dyn Fn(&T) -> bool + Send>;

/// Waits for the first request or response that satisfies a predicate. Created by
/// [`crate::Page::wait_for_request`] and [`crate::Page::wait_for_response`]; it records from the
/// moment it is created, so create it *before* the action that causes the traffic, then `.await`
/// it:
///
/// ```no_run
/// # async fn example(page: &browser_tests::Page, button: browser_tests::Locator<'_>) -> Result<(), browser_tests::Error> {
/// let saved = page.wait_for_response(|response| response.url.ends_with("/react"));
/// button.click().await?;
/// let response = saved.await?;
/// assert_eq!(response.status, 200);
/// # Ok(())
/// # }
/// ```
pub struct Waiter<T> {
    receiver: mpsc::UnboundedReceiver<T>,
    predicate: Predicate<T>,
    timeout: Duration,
    what: &'static str,
}

impl<T> Waiter<T> {
    pub(crate) fn new(
        receiver: mpsc::UnboundedReceiver<T>,
        predicate: Predicate<T>,
        timeout: Duration,
        what: &'static str,
    ) -> Self {
        Self {
            receiver,
            predicate,
            timeout,
            what,
        }
    }

    /// Wait at most `timeout` instead of the page's default timeout.
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    pub async fn wait(mut self) -> Result<T, Error> {
        crate::expect::begin_step(format!("waiting for a matching {}", self.what));
        let deadline = tokio::time::Instant::now() + self.timeout;
        loop {
            match tokio::time::timeout_at(deadline, self.receiver.recv()).await {
                Ok(Some(value)) if (self.predicate)(&value) => return Ok(value),
                Ok(Some(_)) => {}
                Ok(None) => {
                    return Err(Error::Closed(format!(
                        "the page closed while waiting for a {}",
                        self.what
                    )));
                }
                Err(_) => {
                    return Err(Error::Timeout {
                        what: format!("waiting for a matching {}", self.what),
                        after: self.timeout,
                    });
                }
            }
        }
    }
}

impl<T: Send + 'static> IntoFuture for Waiter<T> {
    type Output = Result<T, Error>;
    type IntoFuture = Pin<Box<dyn Future<Output = Self::Output> + Send>>;

    fn into_future(self) -> Self::IntoFuture {
        Box::pin(self.wait())
    }
}
