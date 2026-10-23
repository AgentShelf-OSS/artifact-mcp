// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Neil Blackman
//! Chrome DevTools Protocol framing over `--remote-debugging-pipe`.
//!
//! The browser reads JSON commands from fd 3 and writes JSON replies and events to fd 4; each
//! message ends with one NUL byte. JSON never contains a raw NUL (serde_json escapes U+0000 as
//! `\u0000`), so the framing cannot be confused by page content. There is no TCP listener and no
//! WebSocket: no other local process can reach this channel.
//!
//! Every incoming message is size-capped. The largest legitimate message is a full-size
//! screenshot (a 3840×2160 PNG, base64-encoded); anything above the cap fails the caller instead
//! of growing memory without bound.
//!
//! [`Pipe`] owns both ends. [`Pipe::into_split`] separates them, so one task can read events and
//! replies while others write commands.

use std::fmt;
use std::io;

use serde_json::{Map, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::unix::pipe;

/// Default cap for one incoming message. A worst-case 3840×2160 screenshot (incompressible
/// pixels) is about 33 MB of PNG and 45 MB of base64.
pub const DEFAULT_MAX_MESSAGE_BYTES: usize = 64 * 1024 * 1024;

#[derive(Debug)]
pub enum CdpError {
    Io(io::Error),
    /// The browser closed its end of the pipe (it exited or crashed).
    Closed,
    TooLarge,
    Malformed(String),
    /// A command returned a protocol error.
    Command {
        method: String,
        message: String,
    },
    /// The page's renderer crashed or its target went away.
    TargetGone,
}

impl fmt::Display for CdpError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "devtools pipe I/O error: {error}"),
            Self::Closed => formatter.write_str("browser closed the devtools pipe"),
            Self::TooLarge => formatter.write_str("devtools message exceeded the size cap"),
            Self::Malformed(detail) => write!(formatter, "malformed devtools message: {detail}"),
            Self::Command { method, message } => write!(formatter, "{method} failed: {message}"),
            Self::TargetGone => formatter.write_str("page target crashed or detached"),
        }
    }
}

impl std::error::Error for CdpError {}

impl From<io::Error> for CdpError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

/// One parsed incoming message.
#[derive(Debug, PartialEq)]
pub enum Incoming {
    Response {
        id: u64,
        result: Result<Value, String>,
    },
    Event {
        method: String,
        params: Value,
        session_id: Option<String>,
    },
}

impl Incoming {
    pub fn parse(value: Value) -> Result<Self, CdpError> {
        let Value::Object(mut object) = value else {
            return Err(CdpError::Malformed("message is not an object".into()));
        };
        if let Some(id) = object.get("id") {
            let id = id
                .as_u64()
                .ok_or_else(|| CdpError::Malformed("id is not an integer".into()))?;
            let result = match object.remove("error") {
                Some(error) => Err(error_message(&error)),
                None => Ok(object.remove("result").unwrap_or(Value::Object(Map::new()))),
            };
            return Ok(Self::Response { id, result });
        }
        let Some(Value::String(method)) = object.remove("method") else {
            return Err(CdpError::Malformed(
                "message has neither id nor method".into(),
            ));
        };
        let params = object.remove("params").unwrap_or(Value::Object(Map::new()));
        let session_id = match object.remove("sessionId") {
            Some(Value::String(session)) => Some(session),
            _ => None,
        };
        Ok(Self::Event {
            method,
            params,
            session_id,
        })
    }
}

fn error_message(error: &Value) -> String {
    error
        .get("message")
        .and_then(Value::as_str)
        .map_or_else(|| error.to_string(), str::to_owned)
}

/// Serialize one command, without the trailing NUL.
pub fn encode_command(id: u64, session_id: Option<&str>, method: &str, params: Value) -> Vec<u8> {
    let mut message = Map::new();
    message.insert("id".into(), Value::from(id));
    message.insert("method".into(), Value::from(method));
    message.insert("params".into(), params);
    if let Some(session) = session_id {
        message.insert("sessionId".into(), Value::from(session));
    }
    serde_json::to_vec(&Value::Object(message)).expect("a JSON value always serializes")
}

/// The parent's two pipe ends.
pub struct Pipe {
    sender: PipeSender,
    receiver: PipeReceiver,
}

impl Pipe {
    pub fn new(sender: pipe::Sender, receiver: pipe::Receiver, max_message_bytes: usize) -> Self {
        Self {
            sender: PipeSender { sender },
            receiver: PipeReceiver {
                receiver,
                buffer: Vec::new(),
                start: 0,
                scanned: 0,
                max_message_bytes,
            },
        }
    }

    /// Write one message and its NUL terminator.
    pub async fn send(&mut self, message: &[u8]) -> Result<(), CdpError> {
        self.sender.send(message).await
    }

    /// Read the next message.
    pub async fn recv(&mut self) -> Result<Incoming, CdpError> {
        self.receiver.recv().await
    }

    /// Separate the write end from the read end.
    pub fn into_split(self) -> (PipeSender, PipeReceiver) {
        (self.sender, self.receiver)
    }
}

/// The write end: commands to the browser.
pub struct PipeSender {
    sender: pipe::Sender,
}

impl PipeSender {
    /// Write one message and its NUL terminator.
    pub async fn send(&mut self, message: &[u8]) -> Result<(), CdpError> {
        self.sender
            .write_all(message)
            .await
            .map_err(map_write_error)?;
        self.sender.write_all(&[0]).await.map_err(map_write_error)?;
        Ok(())
    }
}

/// The read end: replies and events from the browser, framed and size-capped.
pub struct PipeReceiver {
    receiver: pipe::Receiver,
    buffer: Vec<u8>,
    /// Start of the first unconsumed byte in `buffer`.
    start: usize,
    /// Bytes before this index (and after `start`) are known to contain no NUL.
    scanned: usize,
    max_message_bytes: usize,
}

impl PipeReceiver {
    /// Read the next message.
    pub async fn recv(&mut self) -> Result<Incoming, CdpError> {
        loop {
            if let Some(offset) = self.buffer[self.scanned..]
                .iter()
                .position(|&byte| byte == 0)
            {
                let end = self.scanned + offset;
                let parsed = serde_json::from_slice::<Value>(&self.buffer[self.start..end])
                    .map_err(|error| CdpError::Malformed(error.to_string()));
                self.start = end + 1;
                self.scanned = self.start;
                if self.start == self.buffer.len() {
                    self.buffer.clear();
                    self.start = 0;
                    self.scanned = 0;
                }
                return Incoming::parse(parsed?);
            }
            self.scanned = self.buffer.len();
            if self.buffer.len() - self.start > self.max_message_bytes {
                return Err(CdpError::TooLarge);
            }
            if self.start > 0 {
                self.buffer.drain(..self.start);
                self.scanned -= self.start;
                self.start = 0;
            }
            self.buffer.reserve(256 * 1024);
            if self.receiver.read_buf(&mut self.buffer).await? == 0 {
                return Err(CdpError::Closed);
            }
        }
    }
}

fn map_write_error(error: io::Error) -> CdpError {
    if error.kind() == io::ErrorKind::BrokenPipe {
        CdpError::Closed
    } else {
        CdpError::Io(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn commands_carry_id_method_params_and_session() {
        let bytes = encode_command(7, Some("S1"), "Page.enable", json!({}));
        let value: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            value,
            json!({"id": 7, "method": "Page.enable", "params": {}, "sessionId": "S1"})
        );
        let bytes = encode_command(8, None, "Browser.getVersion", json!({}));
        assert!(!String::from_utf8(bytes).unwrap().contains("sessionId"));
    }

    #[test]
    fn nul_in_page_content_is_escaped() {
        let bytes = encode_command(
            1,
            None,
            "Runtime.callFunctionOn",
            json!({"arguments": [{"value": "a\0b"}]}),
        );
        assert!(!bytes.contains(&0));
    }

    #[test]
    fn incoming_messages_are_classified() {
        assert_eq!(
            Incoming::parse(json!({"id": 3, "result": {"a": 1}})).unwrap(),
            Incoming::Response {
                id: 3,
                result: Ok(json!({"a": 1}))
            }
        );
        assert_eq!(
            Incoming::parse(json!({"id": 4, "error": {"code": -32000, "message": "nope"}}))
                .unwrap(),
            Incoming::Response {
                id: 4,
                result: Err("nope".into())
            }
        );
        assert_eq!(
            Incoming::parse(
                json!({"method": "Page.loadEventFired", "params": {"t": 1}, "sessionId": "S"})
            )
            .unwrap(),
            Incoming::Event {
                method: "Page.loadEventFired".into(),
                params: json!({"t": 1}),
                session_id: Some("S".into())
            }
        );
        assert!(Incoming::parse(json!([1])).is_err());
        assert!(Incoming::parse(json!({"foo": 1})).is_err());
    }

    #[tokio::test]
    async fn pipe_frames_messages_and_enforces_the_cap() {
        let (mut writer, reader) = pipe::pipe().unwrap();
        let (sender, _unused) = pipe::pipe().unwrap();
        let mut channel = Pipe::new(sender, reader, 64);
        writer
            .write_all(b"{\"id\":1,\"result\":{}}\0{\"method\":\"A.b\"}\0{\"id\":2")
            .await
            .unwrap();
        assert!(matches!(
            channel.recv().await.unwrap(),
            Incoming::Response { id: 1, .. }
        ));
        assert!(matches!(
            channel.recv().await.unwrap(),
            Incoming::Event { .. }
        ));
        writer.write_all(b",\"result\":{}}\0").await.unwrap();
        assert!(matches!(
            channel.recv().await.unwrap(),
            Incoming::Response { id: 2, .. }
        ));
        writer.write_all(&[b' '; 100]).await.unwrap();
        assert!(matches!(channel.recv().await, Err(CdpError::TooLarge)));
    }

    #[tokio::test]
    async fn split_halves_write_and_read_independently() {
        let (to_parent, reader) = pipe::pipe().unwrap();
        let (sender, from_parent) = pipe::pipe().unwrap();
        let (mut commands, mut replies) = Pipe::new(sender, reader, 64).into_split();
        commands.send(b"{\"id\":1}").await.unwrap();
        let mut written = vec![0; 9];
        let mut from_parent = from_parent;
        from_parent.read_exact(&mut written).await.unwrap();
        assert_eq!(written, b"{\"id\":1}\0");
        let mut to_parent = to_parent;
        to_parent
            .write_all(b"{\"method\":\"A.b\"}\0")
            .await
            .unwrap();
        assert!(matches!(
            replies.recv().await.unwrap(),
            Incoming::Event { .. }
        ));
    }

    #[tokio::test]
    async fn closed_pipe_is_reported() {
        let (writer, reader) = pipe::pipe().unwrap();
        let (sender, _unused) = pipe::pipe().unwrap();
        let mut channel = Pipe::new(sender, reader, 64);
        drop(writer);
        assert!(matches!(channel.recv().await, Err(CdpError::Closed)));
    }
}
