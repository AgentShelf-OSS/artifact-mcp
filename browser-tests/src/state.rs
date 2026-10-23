// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Neil Blackman
//! What the reader task records for one page: its log, its frames and JavaScript contexts, its
//! network traffic, and the child targets (out-of-process iframes, workers) auto-attached to it.
//!
//! Everything here is updated synchronously on the reader task, in arrival order (see
//! `connection.rs`), so a caller that sees a command's reply also sees every event sent before it.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};

use crate::connection::{EventHub, EventSink, lock};
use crate::network::{Broadcast, NetworkLog, NetworkRequest, NetworkResponse};
use crate::page::{Dialog, DialogAction, LogEntry};
use crate::route::RouteEntry;

/// Events the page's own task must act on.
pub(crate) enum Active {
    /// `Fetch.requestPaused` on `session`.
    Paused { session: String, params: Value },
    /// `Page.javascriptDialogOpening` on `session`.
    Dialog { session: String, params: Value },
    /// A child target attached (`Target.attachedToTarget`); `session` is the child's session.
    Attached { session: String, kind: String },
}

pub(crate) struct Logged {
    pub(crate) entry: LogEntry,
    /// The session and `Runtime.exceptionThrown` id, so `Runtime.exceptionRevoked` can withdraw
    /// the entry.
    pub(crate) exception: Option<(String, i64)>,
    pub(crate) revoked: bool,
}

/// One session of the page: the page itself or an auto-attached child target.
#[derive(Debug, Clone)]
pub(crate) struct SessionInfo {
    /// `page`, `iframe`, `worker`, `service_worker`, ...
    pub(crate) kind: String,
}

/// One JavaScript execution context (`Runtime.executionContextCreated`).
#[derive(Debug, Clone)]
pub(crate) struct ContextInfo {
    pub(crate) session: String,
    pub(crate) id: i64,
    pub(crate) frame_id: String,
    pub(crate) is_default: bool,
}

/// One frame of the page, wherever it runs.
#[derive(Debug, Clone, Default)]
pub(crate) struct FrameRecord {
    pub(crate) parent: Option<String>,
    pub(crate) url: String,
    pub(crate) name: String,
}

/// What every session of the page must get, including children that attach later.
#[derive(Default, Clone)]
pub(crate) struct Settings {
    /// `Network.setExtraHTTPHeaders` `headers` object.
    pub(crate) extra_headers: Option<Value>,
    /// `Page.addScriptToEvaluateOnNewDocument` sources, in order.
    pub(crate) init_scripts: Vec<String>,
    /// Whether `Log.entryAdded` (network errors, CSP reports, interventions) is recorded.
    pub(crate) browser_log: bool,
    /// The current `Fetch.enable` patterns, if interception is on.
    pub(crate) fetch_patterns: Option<Value>,
}

pub(crate) type DialogHandler = Arc<dyn Fn(&Dialog) -> DialogAction + Send + Sync>;

pub(crate) struct PageState {
    pub(crate) main_session: String,
    pub(crate) log: Mutex<Vec<Logged>>,
    /// Events of the page's own session, by method.
    pub(crate) hub: EventHub,
    /// Events of every session of the page, as `{"sessionId": ..., "params": ...}`.
    pub(crate) all: EventHub,
    pub(crate) active: mpsc::UnboundedSender<Active>,
    pub(crate) sessions: Mutex<HashMap<String, SessionInfo>>,
    pub(crate) contexts: Mutex<Vec<ContextInfo>>,
    pub(crate) frames: Mutex<HashMap<String, FrameRecord>>,
    pub(crate) network: Mutex<NetworkLog>,
    pub(crate) requests: Broadcast<NetworkRequest>,
    pub(crate) responses: Broadcast<NetworkResponse>,
    pub(crate) settings: Mutex<Settings>,
    pub(crate) dialog_handler: Mutex<Option<DialogHandler>>,
    pub(crate) dialogs: Mutex<Vec<Dialog>>,
    pub(crate) routes: Mutex<Vec<RouteEntry>>,
    /// Routes waiting for the response stage of a request they continued (`Route::fetch`).
    pub(crate) response_stage: Mutex<HashMap<String, oneshot::Sender<(String, Value)>>>,
}

impl PageState {
    pub(crate) fn new(main_session: String, active: mpsc::UnboundedSender<Active>) -> Self {
        let mut sessions = HashMap::new();
        sessions.insert(
            main_session.clone(),
            SessionInfo {
                kind: "page".into(),
            },
        );
        Self {
            main_session,
            log: Mutex::new(Vec::new()),
            hub: EventHub::default(),
            all: EventHub::default(),
            active,
            sessions: Mutex::new(sessions),
            contexts: Mutex::new(Vec::new()),
            frames: Mutex::new(HashMap::new()),
            network: Mutex::new(NetworkLog::default()),
            requests: Broadcast::default(),
            responses: Broadcast::default(),
            settings: Mutex::new(Settings::default()),
            dialog_handler: Mutex::new(None),
            dialogs: Mutex::new(Vec::new()),
            routes: Mutex::new(Vec::new()),
            response_stage: Mutex::new(HashMap::new()),
        }
    }

    pub(crate) fn push(&self, entry: LogEntry, exception: Option<(String, i64)>) {
        lock(&self.log).push(Logged {
            entry,
            exception,
            revoked: false,
        });
    }

    /// Sessions that run frames (the page and its out-of-process iframes).
    pub(crate) fn frame_sessions(&self) -> Vec<String> {
        lock(&self.sessions)
            .iter()
            .filter(|(_, info)| info.kind == "page" || info.kind == "iframe")
            .map(|(session, _)| session.clone())
            .collect()
    }

    /// The newest default (main world) context of `frame_id`.
    pub(crate) fn default_context(&self, frame_id: &str) -> Option<ContextInfo> {
        lock(&self.contexts)
            .iter()
            .rev()
            .find(|context| context.is_default && context.frame_id == frame_id)
            .cloned()
    }

    fn on_frame_event(&self, method: &str, params: &Value) {
        let text = |pointer: &str| {
            params
                .pointer(pointer)
                .and_then(Value::as_str)
                .map(str::to_owned)
        };
        let mut frames = lock(&self.frames);
        match method {
            "Page.frameAttached" => {
                if let Some(id) = text("/frameId") {
                    let frame = frames.entry(id).or_default();
                    frame.parent = text("/parentFrameId");
                }
            }
            "Page.frameNavigated" => {
                if let Some(id) = text("/frame/id") {
                    let frame = frames.entry(id).or_default();
                    frame.url = format!(
                        "{}{}",
                        text("/frame/url").unwrap_or_default(),
                        text("/frame/urlFragment").unwrap_or_default()
                    );
                    if let Some(parent) = text("/frame/parentId") {
                        frame.parent = Some(parent);
                    }
                    frame.name = text("/frame/name").unwrap_or_default();
                }
            }
            "Page.navigatedWithinDocument" => {
                if let (Some(id), Some(url)) = (text("/frameId"), text("/url")) {
                    frames.entry(id).or_default().url = url;
                }
            }
            "Page.frameDetached" => {
                // A frame that moves to another process is detached with reason `swap` and lives
                // on in the new process.
                if text("/reason").as_deref() != Some("swap")
                    && let Some(id) = text("/frameId")
                {
                    let mut gone = vec![id];
                    while let Some(id) = gone.pop() {
                        frames.remove(&id);
                        gone.extend(
                            frames
                                .iter()
                                .filter(|(_, frame)| frame.parent.as_deref() == Some(id.as_str()))
                                .map(|(child, _)| child.clone()),
                        );
                    }
                }
            }
            _ => {}
        }
    }

    fn on_context_event(&self, session: &str, method: &str, params: &Value) {
        let mut contexts = lock(&self.contexts);
        match method {
            "Runtime.executionContextCreated" => {
                let context = &params["context"];
                if let Some(id) = context.get("id").and_then(Value::as_i64) {
                    contexts.push(ContextInfo {
                        session: session.to_owned(),
                        id,
                        frame_id: context
                            .pointer("/auxData/frameId")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_owned(),
                        is_default: context
                            .pointer("/auxData/isDefault")
                            .and_then(Value::as_bool)
                            .unwrap_or(false),
                    });
                }
            }
            "Runtime.executionContextDestroyed" => {
                let id = params.get("executionContextId").and_then(Value::as_i64);
                contexts.retain(|context| !(context.session == session && Some(context.id) == id));
            }
            "Runtime.executionContextsCleared" => {
                contexts.retain(|context| context.session != session);
            }
            _ => {}
        }
    }
}

impl EventSink for PageState {
    fn adopts_children(&self) -> bool {
        true
    }

    fn on_event(&self, session: Option<&str>, method: &str, params: &Value) {
        let session = session.unwrap_or(&self.main_session);
        let is_main = session == self.main_session;
        match method {
            "Runtime.consoleAPICalled" => self.push(console_entry(params), None),
            "Runtime.exceptionThrown" => {
                let details = params.get("exceptionDetails").unwrap_or(&Value::Null);
                let entry = LogEntry::Exception {
                    text: exception_text(details),
                    location: exception_location(details),
                };
                let id = details.get("exceptionId").and_then(Value::as_i64);
                self.push(entry, id.map(|id| (session.to_owned(), id)));
            }
            "Runtime.exceptionRevoked" => {
                let id = params.get("exceptionId").and_then(Value::as_i64);
                for logged in lock(&self.log).iter_mut() {
                    if let (Some(id), Some((owner, logged_id))) = (id, &logged.exception)
                        && owner == session
                        && *logged_id == id
                    {
                        logged.revoked = true;
                    }
                }
            }
            "Log.entryAdded" => {
                let entry = &params["entry"];
                let text = |name: &str| {
                    entry
                        .get(name)
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_owned()
                };
                let url = text("url");
                let location = (!url.is_empty()).then(|| {
                    let line = entry.get("lineNumber").and_then(Value::as_u64).unwrap_or(0) + 1;
                    format!("{url}:{line}")
                });
                if entry.get("source").and_then(Value::as_str) != Some("worker") {
                    self.push(
                        LogEntry::Console {
                            level: text("level"),
                            text: text("text"),
                            location,
                        },
                        None,
                    );
                }
            }
            "Page.javascriptDialogOpening" => {
                let text = |name| {
                    params
                        .get(name)
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_owned()
                };
                let dialog = Dialog {
                    kind: text("type"),
                    message: text("message"),
                    default_prompt: text("defaultPrompt"),
                };
                if lock(&self.dialog_handler).is_none() {
                    self.push(
                        LogEntry::Dialog {
                            kind: dialog.kind.clone(),
                            message: dialog.message.clone(),
                        },
                        None,
                    );
                }
                lock(&self.dialogs).push(dialog);
                let _ = self.active.send(Active::Dialog {
                    session: session.to_owned(),
                    params: params.clone(),
                });
            }
            "Fetch.requestPaused" => {
                let _ = self.active.send(Active::Paused {
                    session: session.to_owned(),
                    params: params.clone(),
                });
            }
            "Inspector.targetCrashed" | "Inspector.detached" if is_main => self.push(
                LogEntry::Crash {
                    reason: method.to_owned(),
                },
                None,
            ),
            "Target.attachedToTarget" => {
                let info = &params["targetInfo"];
                let field = |name: &str| {
                    info.get(name)
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_owned()
                };
                let kind = field("type");
                if let Some(child) = params.get("sessionId").and_then(Value::as_str) {
                    lock(&self.sessions)
                        .insert(child.to_owned(), SessionInfo { kind: kind.clone() });
                    if kind == "iframe" {
                        // An out-of-process iframe's target id is its frame id.
                        let mut frames = lock(&self.frames);
                        let frame = frames.entry(field("targetId")).or_default();
                        if frame.url.is_empty() {
                            frame.url = field("url");
                        }
                    }
                    let _ = self.active.send(Active::Attached {
                        session: child.to_owned(),
                        kind,
                    });
                }
            }
            "Target.detachedFromTarget" => {
                if let Some(child) = params.get("sessionId").and_then(Value::as_str) {
                    lock(&self.sessions).remove(child);
                    lock(&self.contexts).retain(|context| context.session != child);
                }
            }
            "Runtime.executionContextCreated"
            | "Runtime.executionContextDestroyed"
            | "Runtime.executionContextsCleared" => self.on_context_event(session, method, params),
            "Page.frameAttached"
            | "Page.frameNavigated"
            | "Page.navigatedWithinDocument"
            | "Page.frameDetached" => self.on_frame_event(method, params),
            method if method.starts_with("Network.") => {
                let events = lock(&self.network).on_event(session, method, params);
                for request in &events.requests {
                    self.requests.send(request);
                }
                for response in &events.responses {
                    self.responses.send(response);
                }
            }
            _ => {}
        }
        if is_main {
            self.hub.publish(method, params);
        }
        if self.all.has_subscribers(method) {
            self.all
                .publish(method, &json!({"sessionId": session, "params": params}));
        }
    }
}

pub(crate) fn console_entry(params: &Value) -> LogEntry {
    let level = params
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("log")
        .to_owned();
    let text = params
        .get("args")
        .and_then(Value::as_array)
        .map(|args| {
            args.iter()
                .map(remote_object_text)
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_default();
    let location = params
        .pointer("/stackTrace/callFrames/0")
        .map(|frame| frame_location(frame, "lineNumber", "columnNumber"));
    LogEntry::Console {
        level,
        text,
        location,
    }
}

/// A readable rendering of one `Runtime.RemoteObject`.
fn remote_object_text(object: &Value) -> String {
    if let Some(value) = object.get("value") {
        return match value {
            Value::String(text) => text.clone(),
            other => other.to_string(),
        };
    }
    if let Some(Value::String(text)) = object.get("unserializableValue") {
        return text.clone();
    }
    if let Some(Value::String(text)) = object.get("description") {
        return text.clone();
    }
    object
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("undefined")
        .to_owned()
}

fn frame_location(frame: &Value, line: &str, column: &str) -> String {
    let url = frame.get("url").and_then(Value::as_str).unwrap_or("");
    let line = frame.get(line).and_then(Value::as_u64).unwrap_or(0) + 1;
    let column = frame.get(column).and_then(Value::as_u64).unwrap_or(0) + 1;
    format!("{url}:{line}:{column}")
}

/// The message of `Runtime.ExceptionDetails`: the thrown error's description (with its stack)
/// when there is one, else the summary text.
pub(crate) fn exception_text(details: &Value) -> String {
    let exception = details.get("exception");
    if let Some(Value::String(description)) =
        exception.and_then(|exception| exception.get("description"))
    {
        return description.clone();
    }
    let text = details
        .get("text")
        .and_then(Value::as_str)
        .unwrap_or("exception");
    match exception.and_then(|exception| exception.get("value")) {
        Some(value) => format!("{text} {value}"),
        None => text.to_owned(),
    }
}

fn exception_location(details: &Value) -> Option<String> {
    if let Some(frame) = details.pointer("/stackTrace/callFrames/0") {
        return Some(frame_location(frame, "lineNumber", "columnNumber"));
    }
    details
        .get("url")
        .and_then(Value::as_str)
        .filter(|url| !url.is_empty())
        .map(|_| frame_location(details, "lineNumber", "columnNumber"))
}
