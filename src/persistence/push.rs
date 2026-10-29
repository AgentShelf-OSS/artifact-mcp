//! ADR-0012 scheduled reminders and Web Push subscriptions.
//!
//! The Node reference `lib/push.js` stores and validates the same rows; only this runtime runs
//! the sweeper and the sender. Every function here is synchronous `rusqlite` work: callers run it
//! inside `spawn_blocking` (see [`crate::persistence::db::interact`]).

use base64::{
    Engine as _,
    engine::general_purpose::{STANDARD as BASE64_STANDARD, URL_SAFE_NO_PAD},
};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::{
    error::AppError,
    mcp::protocol::OrderedJson,
    persistence::{migrations::EncryptedUrl, state},
};

pub use crate::persistence::state::StateScope as ReminderScope;

/// At most 16 `armed` rows per artifact, all scopes together.
pub const ARMED_REMINDER_LIMIT: i64 = 16;
/// Registering an 11th subscription deletes the viewer's oldest row.
pub const SUBSCRIPTIONS_PER_VIEWER: usize = 10;
/// The earliest permitted fire time, relative to the server clock.
pub const MIN_LEAD_MS: i64 = 60 * 1000;
/// The latest permitted fire time, relative to the server clock.
pub const MAX_LEAD_MS: i64 = 30 * 24 * 60 * 60 * 1000;
/// A reminder this late is marked fired without delivery.
pub const STALE_AFTER_MS: i64 = 6 * 60 * 60 * 1000;
/// A delivery expires this long after it is queued.
pub const DELIVERY_TTL_MS: i64 = 30 * 60 * 1000;
/// Due reminders claimed per sweeper transaction.
pub const SWEEP_BATCH: i64 = 100;
/// Title limit in Unicode scalar values.
pub const TITLE_MAX_CHARS: usize = 80;
/// Body limit in Unicode scalar values.
pub const BODY_MAX_CHARS: usize = 240;
/// Endpoint limit in UTF-8 bytes.
pub const ENDPOINT_MAX_BYTES: usize = 1024;
/// Device label limit in Unicode scalar values.
pub const LABEL_MAX_CHARS: usize = 60;
/// Tool error text when the feature is off.
pub const PUSH_DISABLED_MESSAGE: &str = "Web Push reminders are not configured on this server.";

const JS_MAX_SAFE_INTEGER: f64 = 9_007_199_254_740_991.0;

/// Shared route/MCP validation and write failures, with their wire codes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum PushError {
    #[error("bad_scope")]
    BadScope,
    #[error("bad_key")]
    BadKey,
    #[error("bad_time")]
    BadTime,
    #[error("bad_text")]
    BadText,
    #[error("reminder_limit")]
    ReminderLimit,
    #[error("bad_endpoint")]
    BadEndpoint,
    #[error("bad_keys")]
    BadKeys,
    #[error("push_unavailable")]
    Unavailable,
}

impl PushError {
    /// The JSON `error` code.
    #[must_use]
    pub fn code(self) -> &'static str {
        match self {
            Self::BadScope => "bad_scope",
            Self::BadKey => "bad_key",
            Self::BadTime => "bad_time",
            Self::BadText => "bad_text",
            Self::ReminderLimit => "reminder_limit",
            Self::BadEndpoint => "bad_endpoint",
            Self::BadKeys => "bad_keys",
            Self::Unavailable => "push_unavailable",
        }
    }

    /// The HTTP status for the route surface.
    #[must_use]
    pub const fn status(self) -> u16 {
        match self {
            Self::ReminderLimit => 409,
            Self::Unavailable => 500,
            _ => 400,
        }
    }
}

impl From<AppError> for PushError {
    fn from(_: AppError) -> Self {
        Self::Unavailable
    }
}

fn db_error(error: rusqlite::Error) -> AppError {
    tracing::error!(error = %error, "push persistence failed");
    AppError::Internal
}

mod reminders;
mod sender;
mod subscriptions;
mod sweeper;
mod validation;

pub use reminders::*;
pub use sender::*;
pub use subscriptions::*;
pub use sweeper::*;
pub use validation::*;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ciphertext_packing_round_trips_and_rejects_other_shapes() {
        let record = EncryptedUrl {
            ciphertext: "YWJj".into(),
            nonce: "AAAAAAAAAAAAAAAA".into(),
            tag: "AAAAAAAAAAAAAAAAAAAAAA==".into(),
        };
        let packed = pack_endpoint_ciphertext(&record);
        assert_eq!(packed, "v1:AAAAAAAAAAAAAAAA:YWJj:AAAAAAAAAAAAAAAAAAAAAA==");
        assert_eq!(unpack_endpoint_ciphertext(&packed), Some(record));
        assert_eq!(unpack_endpoint_ciphertext("v2:a:b:c"), None);
        assert_eq!(unpack_endpoint_ciphertext("v1:a:b"), None);
    }

    #[test]
    fn host_rules_match_exact_names_and_subdomain_wildcards() {
        let hosts = vec![
            "fcm.googleapis.com".to_owned(),
            "*.push.apple.com".to_owned(),
        ];
        assert!(host_allowed("fcm.googleapis.com", &hosts));
        assert!(host_allowed("api.push.apple.com", &hosts));
        assert!(!host_allowed("push.apple.com", &hosts));
        assert!(!host_allowed("evilpush.apple.com", &hosts));
        assert!(!host_allowed("x.fcm.googleapis.com", &hosts));
    }

    #[test]
    fn set_reminder_checks_key_before_viewer_owner() {
        let mut conn = Connection::open_in_memory().expect("in-memory database");
        let text = ReminderText {
            title: "title".to_owned(),
            body: String::new(),
        };
        let write = ReminderWrite {
            artifact_id: "artifact",
            org: "org",
            scope: ReminderScope::Viewer,
            owner: "",
            key: "bad/key",
            fire_at: 60_000,
            text: &text,
            created_by: "viewer",
        };

        assert_eq!(
            set_reminder(&mut conn, &write, "now"),
            Err(PushError::BadKey)
        );
    }
}
