//! ADR-0012 scheduled reminders and Web Push subscriptions.
//!
//! This module stores and validates reminder and subscription rows and runs the sweeper and sender. Every function here is synchronous `rusqlite` work: callers run it inside `spawn_blocking` (see [`crate::persistence::db::interact`]).

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64_STANDARD};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::Serialize;

use crate::{error::AppError, persistence::migrations::EncryptedUrl};

pub use crate::model::push::*;

/// At most 16 `armed` rows per artifact, all scopes together.
pub const ARMED_REMINDER_LIMIT: i64 = 16;
/// Registering an 11th subscription deletes the viewer's oldest row.
pub const SUBSCRIPTIONS_PER_VIEWER: usize = 10;
/// A reminder this late is marked fired without delivery.
pub const STALE_AFTER_MS: i64 = 6 * 60 * 60 * 1000;
/// A delivery expires this long after it is queued.
pub const DELIVERY_TTL_MS: i64 = 30 * 60 * 1000;
/// Due reminders claimed per sweeper transaction.
pub const SWEEP_BATCH: i64 = 100;
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
