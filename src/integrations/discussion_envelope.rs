//! Canonical, secret-free PBI-079 discussion operations.
//!
//! A delivery row carries this object and an opaque connection reference, never a Discord URL.
//! Thread/message identifiers are intentionally resolved from durable discussion state only after
//! the row is leased, preventing an old envelope from selecting a newer generation's thread.

use std::fmt;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{
    error::AppError,
    integrations::discord_discussion::{DiscussionOperation, MAX_DISCUSSION_CONTENT_CHARS},
    model::OrgId,
    persistence::outbox::MAX_PAYLOAD_BYTES,
};

pub const DISCUSSION_ENVELOPE_VERSION: u8 = 1;
const MAX_ARTIFACT_ID_BYTES: usize = 120;
const MAX_EVENT_ID_BYTES: usize = 160;
const MAX_CONNECTION_ID_BYTES: usize = 112;
const MAX_FEEDBACK_ID_BYTES: usize = 128;
const MAX_THREAD_NAME_CHARS: usize = 100;

/// The durable intent, deliberately without Discord message or thread identifiers.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum DiscordDiscussionOperationV1 {
    Thread {
        feedback_id: String,
        thread_name: String,
        content: String,
    },
    Reply {
        feedback_id: String,
        content: String,
    },
    Resolved {
        feedback_id: String,
    },
    Reopened {
        feedback_id: String,
    },
    Tombstone {
        feedback_id: String,
    },
}

impl fmt::Debug for DiscordDiscussionOperationV1 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

impl DiscordDiscussionOperationV1 {
    pub fn thread(
        feedback_id: String,
        thread_name: String,
        content: String,
    ) -> Result<Self, AppError> {
        let operation = Self::Thread {
            feedback_id,
            thread_name,
            content,
        };
        operation.validate()?;
        Ok(operation)
    }

    pub fn reply(feedback_id: String, content: String) -> Result<Self, AppError> {
        let operation = Self::Reply {
            feedback_id,
            content,
        };
        operation.validate()?;
        Ok(operation)
    }

    pub fn resolved(feedback_id: String) -> Result<Self, AppError> {
        let operation = Self::Resolved { feedback_id };
        operation.validate()?;
        Ok(operation)
    }

    pub fn reopened(feedback_id: String) -> Result<Self, AppError> {
        let operation = Self::Reopened { feedback_id };
        operation.validate()?;
        Ok(operation)
    }

    pub fn tombstone(feedback_id: String) -> Result<Self, AppError> {
        let operation = Self::Tombstone { feedback_id };
        operation.validate()?;
        Ok(operation)
    }

    #[must_use]
    pub const fn name(&self) -> &'static str {
        match self {
            Self::Thread { .. } => "thread",
            Self::Reply { .. } => "reply",
            Self::Resolved { .. } => "resolved",
            Self::Reopened { .. } => "reopened",
            Self::Tombstone { .. } => "tombstone",
        }
    }

    #[must_use]
    pub fn feedback_id(&self) -> &str {
        match self {
            Self::Thread { feedback_id, .. }
            | Self::Reply { feedback_id, .. }
            | Self::Resolved { feedback_id }
            | Self::Reopened { feedback_id }
            | Self::Tombstone { feedback_id } => feedback_id,
        }
    }

    fn validate(&self) -> Result<(), AppError> {
        match self {
            Self::Thread {
                feedback_id,
                thread_name,
                content,
            } if valid_feedback_id(feedback_id)
                && valid_thread_name(thread_name)
                && valid_content(content) =>
            {
                Ok(())
            }
            Self::Reply {
                feedback_id,
                content,
            } if valid_feedback_id(feedback_id) && valid_content(content) => Ok(()),
            Self::Resolved { feedback_id }
            | Self::Reopened { feedback_id }
            | Self::Tombstone { feedback_id }
                if valid_feedback_id(feedback_id) =>
            {
                Ok(())
            }
            _ => Err(AppError::Validation("invalid discussion envelope".into())),
        }
    }
}

/// Versioned, canonically encoded discussion request.  Constructors and canonical decoding are
/// the only entry points so producers cannot persist a malformed operation.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiscordDiscussionEnvelopeV1 {
    version: u8,
    event_id: String,
    tenant: String,
    artifact_id: String,
    connection_id: String,
    generation: u64,
    provider: String,
    operation: DiscordDiscussionOperationV1,
}

impl fmt::Debug for DiscordDiscussionEnvelopeV1 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DiscordDiscussionEnvelopeV1")
            .field("version", &self.version)
            .field("event_id", &self.event_id)
            .field("tenant", &self.tenant)
            .field("artifact_id", &self.artifact_id)
            .field("connection_id", &self.connection_id)
            .field("generation", &self.generation)
            .field("provider", &self.provider)
            .field("operation", &self.operation.name())
            .finish()
    }
}

impl DiscordDiscussionEnvelopeV1 {
    pub fn build(
        event_id: String,
        tenant: &OrgId,
        artifact_id: String,
        connection_id: String,
        generation: u64,
        operation: DiscordDiscussionOperationV1,
    ) -> Result<Self, AppError> {
        let envelope = Self {
            version: DISCUSSION_ENVELOPE_VERSION,
            event_id,
            tenant: tenant.0.clone(),
            artifact_id,
            connection_id,
            generation,
            provider: "discord".into(),
            operation,
        };
        envelope.validate_bound(tenant, None, None, None, None)?;
        if envelope.canonical_bytes()?.len() > MAX_PAYLOAD_BYTES {
            return Err(AppError::PayloadTooLarge);
        }
        Ok(envelope)
    }

    /// Strictly decode one exact canonical object and bind it to durable row/state identities.
    pub fn decode_canonical(
        bytes: &[u8],
        tenant: &OrgId,
        event_id: &str,
        artifact_id: &str,
        connection_id: &str,
        generation: u64,
        expected_payload_sha256: Option<&str>,
    ) -> Result<Self, AppError> {
        if bytes.len() > MAX_PAYLOAD_BYTES {
            return Err(AppError::PayloadTooLarge);
        }
        let envelope: Self = serde_json::from_slice(bytes)
            .map_err(|_| AppError::Validation("invalid discussion envelope".into()))?;
        envelope.validate_bound(
            tenant,
            Some(event_id),
            Some(artifact_id),
            Some(connection_id),
            Some(generation),
        )?;
        if envelope.canonical_bytes()? != bytes {
            return Err(AppError::Validation(
                "non-canonical discussion envelope".into(),
            ));
        }
        let payload_sha256 = envelope.payload_sha256()?;
        if expected_payload_sha256.is_some_and(|expected| expected != payload_sha256) {
            return Err(AppError::Validation(
                "discussion envelope hash mismatch".into(),
            ));
        }
        Ok(envelope)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn validate_bound(
        &self,
        tenant: &OrgId,
        event_id: Option<&str>,
        artifact_id: Option<&str>,
        connection_id: Option<&str>,
        generation: Option<u64>,
    ) -> Result<(), AppError> {
        if self.version != DISCUSSION_ENVELOPE_VERSION
            || self.provider != "discord"
            || !valid_id(&self.event_id, MAX_EVENT_ID_BYTES)
            || self.tenant != tenant.0
            || !valid_id(&self.artifact_id, MAX_ARTIFACT_ID_BYTES)
            || !valid_id(&self.connection_id, MAX_CONNECTION_ID_BYTES)
            || self.generation == 0
            || event_id.is_some_and(|expected| expected != self.event_id)
            || artifact_id.is_some_and(|expected| expected != self.artifact_id)
            || connection_id.is_some_and(|expected| expected != self.connection_id)
            || generation.is_some_and(|expected| expected != self.generation)
        {
            return Err(AppError::Validation("invalid discussion envelope".into()));
        }
        self.operation.validate()
    }

    pub fn canonical_bytes(&self) -> Result<Vec<u8>, AppError> {
        serde_json::to_vec(self).map_err(|_| AppError::Internal)
    }

    pub fn payload_sha256(&self) -> Result<String, AppError> {
        Ok(hex::encode(Sha256::digest(self.canonical_bytes()?)))
    }

    /// Convert only after the worker has loaded the authoritative external identifiers.
    pub fn to_transport_operation(
        &self,
        external_thread_id: Option<&str>,
        external_message_id: Option<&str>,
    ) -> Result<DiscussionOperation, AppError> {
        let thread_id = external_thread_id.unwrap_or_default().to_owned();
        match &self.operation {
            DiscordDiscussionOperationV1::Thread {
                feedback_id: _,
                thread_name,
                content,
            } => DiscussionOperation::create_thread(thread_name.clone(), content.clone())
                .map_err(|_| AppError::Validation("invalid discussion envelope".into())),
            DiscordDiscussionOperationV1::Reply {
                feedback_id: _,
                content,
            } => DiscussionOperation::reply(thread_id, content.clone())
                .map_err(|_| AppError::Validation("invalid discussion envelope".into())),
            DiscordDiscussionOperationV1::Resolved { feedback_id: _ } => {
                DiscussionOperation::resolved_marker(thread_id)
                    .map_err(|_| AppError::Validation("invalid discussion envelope".into()))
            }
            DiscordDiscussionOperationV1::Reopened { feedback_id: _ } => {
                DiscussionOperation::reopened_marker(thread_id)
                    .map_err(|_| AppError::Validation("invalid discussion envelope".into()))
            }
            DiscordDiscussionOperationV1::Tombstone { feedback_id: _ } => {
                DiscussionOperation::tombstone(
                    thread_id,
                    external_message_id.unwrap_or_default().to_owned(),
                )
                .map_err(|_| AppError::Validation("invalid discussion envelope".into()))
            }
        }
    }

    #[must_use]
    pub fn event_id(&self) -> &str {
        &self.event_id
    }
    #[must_use]
    pub fn tenant(&self) -> &str {
        &self.tenant
    }
    #[must_use]
    pub fn artifact_id(&self) -> &str {
        &self.artifact_id
    }
    #[must_use]
    pub fn connection_id(&self) -> &str {
        &self.connection_id
    }
    #[must_use]
    pub const fn generation(&self) -> u64 {
        self.generation
    }
    #[must_use]
    pub fn operation(&self) -> &DiscordDiscussionOperationV1 {
        &self.operation
    }
}

fn valid_id(value: &str, max: usize) -> bool {
    !value.is_empty()
        && value.len() <= max
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.' | b':'))
}

fn valid_thread_name(value: &str) -> bool {
    !value.trim().is_empty()
        && value.chars().count() <= MAX_THREAD_NAME_CHARS
        && !value.contains('\0')
}

fn valid_content(value: &str) -> bool {
    !value.trim().is_empty()
        && value.chars().count() <= MAX_DISCUSSION_CONTENT_CHARS
        && !value.contains('\0')
}

fn valid_feedback_id(value: &str) -> bool {
    value.len() <= MAX_FEEDBACK_ID_BYTES
        && !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn envelope() -> DiscordDiscussionEnvelopeV1 {
        DiscordDiscussionEnvelopeV1::build(
            "discussion-event".into(),
            &OrgId("acme".into()),
            "artifact-a".into(),
            "connection-a".into(),
            1,
            DiscordDiscussionOperationV1::thread(
                "feedback-a".into(),
                "Artifact discussion".into(),
                "First feedback".into(),
            )
            .expect("operation"),
        )
        .expect("envelope")
    }

    #[test]
    fn canonical_envelope_is_bound_and_secret_free() {
        let value = envelope();
        let bytes = value.canonical_bytes().expect("canonical");
        let decoded = DiscordDiscussionEnvelopeV1::decode_canonical(
            &bytes,
            &OrgId("acme".into()),
            "discussion-event",
            "artifact-a",
            "connection-a",
            1,
            Some(&value.payload_sha256().expect("hash")),
        )
        .expect("decode");
        assert_eq!(decoded, value);
        assert_eq!(decoded.operation().feedback_id(), "feedback-a");
        assert!(!String::from_utf8(bytes).expect("utf8").contains("webhook"));
    }

    #[test]
    fn malformed_or_noncanonical_envelopes_are_rejected() {
        let value = envelope();
        let malformed = br#"{\"version\":1,\"event_id\":\"x\"}"#;
        assert!(
            DiscordDiscussionEnvelopeV1::decode_canonical(
                malformed,
                &OrgId("acme".into()),
                "x",
                "artifact-a",
                "connection-a",
                1,
                None
            )
            .is_err()
        );
        let mut bytes = value.canonical_bytes().expect("canonical");
        bytes.push(b' ');
        assert!(
            DiscordDiscussionEnvelopeV1::decode_canonical(
                &bytes,
                &OrgId("acme".into()),
                "discussion-event",
                "artifact-a",
                "connection-a",
                1,
                None
            )
            .is_err()
        );
    }

    #[test]
    fn every_operation_requires_a_bounded_feedback_identity() {
        for operation in [
            DiscordDiscussionOperationV1::reply(String::new(), "reply".into()),
            DiscordDiscussionOperationV1::resolved("feedback/unsafe".into()),
            DiscordDiscussionOperationV1::reopened(" ".into()),
            DiscordDiscussionOperationV1::tombstone("x".repeat(MAX_FEEDBACK_ID_BYTES + 1)),
        ] {
            assert!(operation.is_err());
        }
    }
}
