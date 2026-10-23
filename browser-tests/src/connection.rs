// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Neil Blackman
//! One DevTools connection, shared by the browser and all its pages.
//!
//! A reader task owns the read end of the pipe. It hands each reply to the caller waiting for that
//! command id, and gives each event to the [`EventHub`] of its session (or of the browser, for
//! events without a session) *synchronously*, in arrival order. So when a caller sees the reply to
//! a command, every event the browser sent before that reply is already recorded. Test code relies
//! on this to attribute console messages and exceptions to the test case that caused them.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use artifact_cdp::{CdpError, Incoming, Pipe, PipeSender, encode_command};
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::Error;

type Reply = Result<Value, Error>;

/// Receives the events of one session (or of the browser target).
pub(crate) trait EventSink: Send + Sync {
    /// Called on the reader task, in arrival order. Must not block. `session` is the session the
    /// event came from (`None` for the browser target).
    fn on_event(&self, session: Option<&str>, method: &str, params: &Value);

    /// Whether targets auto-attached to one of this sink's sessions (out-of-process iframes and
    /// workers) send their events here too. The child session is registered on the reader task,
    /// before its first event is read, so nothing it sends is lost.
    fn adopts_children(&self) -> bool {
        false
    }
}

/// Event subscriptions by method name.
#[derive(Default)]
pub(crate) struct EventHub {
    subscribers: Mutex<Vec<(String, mpsc::UnboundedSender<Value>)>>,
}

impl EventHub {
    pub(crate) fn subscribe(&self, method: &str) -> Subscription {
        let (sender, receiver) = mpsc::unbounded_channel();
        lock(&self.subscribers).push((method.to_owned(), sender));
        Subscription {
            method: method.to_owned(),
            receiver,
        }
    }

    /// Whether anyone listens for `method`, so callers can skip building an event nobody reads.
    pub(crate) fn has_subscribers(&self, method: &str) -> bool {
        lock(&self.subscribers)
            .iter()
            .any(|(wanted, sender)| wanted == method && !sender.is_closed())
    }

    pub(crate) fn publish(&self, method: &str, params: &Value) {
        lock(&self.subscribers)
            .retain(|(wanted, sender)| wanted != method || sender.send(params.clone()).is_ok());
    }
}

/// Events of one method, from the moment of subscription. Subscribe before the action that
/// triggers the event, then wait.
pub struct Subscription {
    method: String,
    receiver: mpsc::UnboundedReceiver<Value>,
}

impl Subscription {
    /// The next event's `params`.
    pub async fn next(&mut self, timeout: Duration) -> Result<Value, Error> {
        match tokio::time::timeout(timeout, self.receiver.recv()).await {
            Ok(Some(params)) => Ok(params),
            Ok(None) => Err(Error::Closed(format!(
                "no more {} events: the page or browser is gone",
                self.method
            ))),
            Err(_) => Err(Error::Timeout {
                what: format!("waiting for {}", self.method),
                after: timeout,
            }),
        }
    }

    /// The first event whose `params` satisfy `predicate`.
    pub async fn next_matching(
        &mut self,
        timeout: Duration,
        predicate: impl Fn(&Value) -> bool,
    ) -> Result<Value, Error> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            let params = self.next(remaining).await.map_err(|error| match error {
                Error::Timeout { what, .. } => Error::Timeout {
                    what,
                    after: timeout,
                },
                other => other,
            })?;
            if predicate(&params) {
                return Ok(params);
            }
        }
    }
}

struct Pending {
    session: Option<String>,
    method: String,
    reply: oneshot::Sender<Reply>,
}

#[derive(Default)]
struct Shared {
    pending: HashMap<u64, Pending>,
    sessions: HashMap<String, Arc<dyn EventSink>>,
    /// Why the connection ended, once it has.
    closed: Option<String>,
}

pub(crate) struct Connection {
    writer: tokio::sync::Mutex<PipeSender>,
    next_id: AtomicU64,
    shared: Mutex<Shared>,
    browser_events: Arc<dyn EventSink>,
}

impl Connection {
    /// Split `pipe` and start the reader task. `browser_events` receives events that carry no
    /// session id.
    pub(crate) fn start(
        pipe: Pipe,
        browser_events: Arc<dyn EventSink>,
    ) -> (Arc<Self>, JoinHandle<()>) {
        let (writer, mut reader) = pipe.into_split();
        let connection = Arc::new(Self {
            writer: tokio::sync::Mutex::new(writer),
            next_id: AtomicU64::new(0),
            shared: Mutex::new(Shared::default()),
            browser_events,
        });
        let weak = Arc::downgrade(&connection);
        let task = tokio::spawn(async move {
            loop {
                let incoming = reader.recv().await;
                let Some(connection) = weak.upgrade() else {
                    return;
                };
                match incoming {
                    Ok(Incoming::Response { id, result }) => connection.on_reply(id, result),
                    Ok(Incoming::Event {
                        method,
                        params,
                        session_id,
                    }) => connection.on_event(&method, &params, session_id.as_deref()),
                    Err(error) => {
                        connection.close(&describe_transport_error(&error));
                        return;
                    }
                }
            }
        });
        (connection, task)
    }

    fn on_reply(&self, id: u64, result: Result<Value, String>) {
        let pending = lock(&self.shared).pending.remove(&id);
        // A reply nobody waits for belongs to a command that timed out or was sent without
        // waiting.
        if let Some(pending) = pending {
            let reply = result.map_err(|message| Error::Command {
                method: pending.method,
                message,
            });
            let _ = pending.reply.send(reply);
        }
    }

    fn on_event(&self, method: &str, params: &Value, session: Option<&str>) {
        let sink = match session {
            Some(session) => lock(&self.shared).sessions.get(session).cloned(),
            None => Some(Arc::clone(&self.browser_events)),
        };
        if method == "Target.attachedToTarget"
            && session.is_some()
            && let Some(sink) = &sink
            && sink.adopts_children()
            && let Some(child) = params.get("sessionId").and_then(Value::as_str)
        {
            self.register_session(child, Arc::clone(sink));
        }
        if let Some(sink) = sink {
            sink.on_event(session, method, params);
        }
        match method {
            "Target.detachedFromTarget" => {
                if let Some(detached) = params.get("sessionId").and_then(Value::as_str) {
                    // Dropping the sink ends the page's event task and its subscriptions.
                    lock(&self.shared).sessions.remove(detached);
                    self.fail_session(detached, "the page was detached");
                }
            }
            "Inspector.targetCrashed" | "Inspector.detached" => {
                if let Some(session) = session {
                    self.fail_session(session, &format!("{method}: the page went away"));
                }
            }
            _ => {}
        }
    }

    /// Fail every command still waiting on `session`.
    fn fail_session(&self, session: &str, reason: &str) {
        let failed: Vec<Pending> = {
            let mut shared = lock(&self.shared);
            let ids: Vec<u64> = shared
                .pending
                .iter()
                .filter(|(_, pending)| pending.session.as_deref() == Some(session))
                .map(|(id, _)| *id)
                .collect();
            ids.iter()
                .filter_map(|id| shared.pending.remove(id))
                .collect()
        };
        for pending in failed {
            let _ = pending
                .reply
                .send(Err(Error::Closed(format!("{}: {reason}", pending.method))));
        }
    }

    fn close(&self, reason: &str) {
        let pending: Vec<Pending> = {
            let mut shared = lock(&self.shared);
            shared.closed = Some(reason.to_owned());
            // Dropping the sinks ends every page's event task and subscription.
            shared.sessions.clear();
            shared.pending.drain().map(|(_, pending)| pending).collect()
        };
        for pending in pending {
            let _ = pending
                .reply
                .send(Err(Error::Closed(format!("{}: {reason}", pending.method))));
        }
    }

    pub(crate) fn register_session(&self, session: &str, sink: Arc<dyn EventSink>) {
        lock(&self.shared).sessions.insert(session.to_owned(), sink);
    }

    pub(crate) fn unregister_session(&self, session: &str) {
        lock(&self.shared).sessions.remove(session);
        self.fail_session(session, "the page was closed");
    }

    /// Send a command and wait up to `timeout` for its reply.
    pub(crate) async fn call(
        &self,
        session: Option<&str>,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, Error> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed) + 1;
        let (sender, receiver) = oneshot::channel();
        {
            let mut shared = lock(&self.shared);
            if let Some(reason) = &shared.closed {
                return Err(Error::Closed(format!("{method}: {reason}")));
            }
            shared.pending.insert(
                id,
                Pending {
                    session: session.map(str::to_owned),
                    method: method.to_owned(),
                    reply: sender,
                },
            );
        }
        let message = encode_command(id, session, method, params);
        if let Err(error) = self.writer.lock().await.send(&message).await {
            lock(&self.shared).pending.remove(&id);
            return Err(Error::Closed(format!(
                "{method}: {}",
                describe_transport_error(&error)
            )));
        }
        match tokio::time::timeout(timeout, receiver).await {
            Ok(Ok(reply)) => reply,
            Ok(Err(_)) => Err(Error::Closed(format!("{method}: the connection ended"))),
            Err(_) => {
                lock(&self.shared).pending.remove(&id);
                Err(Error::Timeout {
                    what: method.to_owned(),
                    after: timeout,
                })
            }
        }
    }
}

fn describe_transport_error(error: &CdpError) -> String {
    match error {
        CdpError::Closed => "the browser exited or closed the DevTools pipe".to_owned(),
        other => other.to_string(),
    }
}

/// Lock a mutex, ignoring poisoning: the state stays usable after a panicking test thread.
pub(crate) fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}
