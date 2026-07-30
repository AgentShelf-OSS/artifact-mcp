//! Versioned, canonical, secret-free delivery planning shared by future outbox producers.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{
    artifacts::validation::{BodyDigest, SafeArtifactId},
    error::AppError,
    integrations::notify::{
        DiscordPayload, EmbedImage, PREVIEW_FILENAME, accepts_preview, build_embed,
        multipart_request,
    },
    model::{NotificationPayload, OrgId, WebhookEvent},
    persistence::{outbox::MAX_PAYLOAD_BYTES, webhooks::event_name},
};

pub const DELIVERY_ENVELOPE_VERSION: u8 = 1;

/// Secret-free immutable selector for an optional artifact thumbnail.
///
/// The outbox contains neither PNG bytes nor filesystem paths.  A worker must re-load artifact
/// metadata after claiming a row and prove this reference still names the same tenant, revision,
/// and body before it can read or render a preview.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeliveryPreviewReferenceV1 {
    artifact_id: String,
    revision: u64,
    body_sha256: String,
}

impl DeliveryPreviewReferenceV1 {
    /// Builds a bounded reference from already-authoritative lifecycle metadata.
    pub fn new(artifact_id: &str, revision: u64, body_sha256: &str) -> Result<Self, AppError> {
        if revision == 0
            || SafeArtifactId::addressable(artifact_id).is_none()
            || BodyDigest::parse(body_sha256).is_none()
        {
            return Err(AppError::Validation(
                "invalid delivery preview reference".into(),
            ));
        }
        Ok(Self {
            artifact_id: artifact_id.to_owned(),
            revision,
            body_sha256: body_sha256.to_owned(),
        })
    }

    #[must_use]
    pub fn artifact_id(&self) -> &str {
        &self.artifact_id
    }

    #[must_use]
    pub const fn revision(&self) -> u64 {
        self.revision
    }

    #[must_use]
    pub fn body_sha256(&self) -> &str {
        &self.body_sha256
    }
}

/// Stored durable object. Its private fields prevent a caller from hand-constructing an
/// unvalidated envelope; use [`build`](Self::build) or [`decode_canonical`](Self::decode_canonical).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeliveryEnvelopeV1 {
    version: u8,
    event_id: String,
    tenant: String,
    event_type: String,
    provider: String,
    payload: DiscordPayload,
    #[serde(skip_serializing_if = "Option::is_none")]
    preview: Option<DeliveryPreviewReferenceV1>,
}

impl DeliveryEnvelopeV1 {
    pub fn build(
        event_id: String,
        tenant: &OrgId,
        event: &WebhookEvent,
        input: &NotificationPayload,
    ) -> Result<Self, AppError> {
        Self::build_with_preview(event_id, tenant, event, input, None)
    }

    /// Builds a canonical envelope with an optional immutable artifact preview reference.
    pub fn build_with_preview(
        event_id: String,
        tenant: &OrgId,
        event: &WebhookEvent,
        input: &NotificationPayload,
        preview: Option<DeliveryPreviewReferenceV1>,
    ) -> Result<Self, AppError> {
        let envelope = Self {
            version: DELIVERY_ENVELOPE_VERSION,
            event_id,
            tenant: tenant.0.clone(),
            event_type: event_name(event).to_owned(),
            provider: "discord".into(),
            payload: build_embed(event, tenant, input),
            preview,
        };
        envelope.validate_bound(tenant, event, None)?;
        if envelope.canonical_bytes()?.len() > MAX_PAYLOAD_BYTES {
            return Err(AppError::PayloadTooLarge);
        }
        Ok(envelope)
    }
    /// Strictly decodes one exact canonical JSON object and binds it to the expected queue row.
    pub fn decode_canonical(
        bytes: &[u8],
        tenant: &OrgId,
        event: &WebhookEvent,
        event_id: &str,
        expected_payload_sha256: Option<&str>,
    ) -> Result<Self, AppError> {
        if bytes.len() > MAX_PAYLOAD_BYTES {
            return Err(AppError::PayloadTooLarge);
        }
        let envelope: Self = serde_json::from_slice(bytes)
            .map_err(|_| AppError::Validation("invalid delivery envelope".into()))?;
        envelope.validate_bound(tenant, event, Some(event_id))?;
        if envelope.canonical_bytes()? != bytes {
            return Err(AppError::Validation(
                "non-canonical delivery envelope".into(),
            ));
        }
        let payload_sha256 = envelope.payload_sha256()?;
        if expected_payload_sha256.is_some_and(|expected| expected != payload_sha256) {
            return Err(AppError::Validation(
                "delivery envelope hash mismatch".into(),
            ));
        }
        Ok(envelope)
    }
    /// Validates the binding a fanout/worker has independently from untrusted public input.
    pub fn validate_bound(
        &self,
        tenant: &OrgId,
        event: &WebhookEvent,
        expected_event_id: Option<&str>,
    ) -> Result<(), AppError> {
        if self.version != DELIVERY_ENVELOPE_VERSION
            || self.provider != "discord"
            || self.event_id.trim().is_empty()
            || self.tenant.trim().is_empty()
            || self.event_type != event_name(event)
            || self.tenant != tenant.0
            || expected_event_id.is_some_and(|expected| expected != self.event_id)
            || self.payload.embeds.is_empty()
            || self
                .payload
                .embeds
                .iter()
                .any(|embed| embed.image.is_some())
            || self.preview.as_ref().is_some_and(|preview| {
                !accepts_preview(event)
                    || DeliveryPreviewReferenceV1::new(
                        preview.artifact_id(),
                        preview.revision(),
                        preview.body_sha256(),
                    )
                    .is_err()
                    || preview.artifact_id() != self.payload_artifact_id(event)
            })
        {
            return Err(AppError::Validation("invalid delivery envelope".into()));
        }
        Ok(())
    }
    pub fn canonical_bytes(&self) -> Result<Vec<u8>, AppError> {
        serde_json::to_vec(self).map_err(|_| AppError::Internal)
    }
    /// Canonical HTTP request bytes: the nested Discord payload only, never envelope metadata.
    pub fn discord_request_body_bytes(&self) -> Result<Vec<u8>, AppError> {
        serde_json::to_vec(&self.payload).map_err(|_| AppError::Internal)
    }

    /// Builds the JSON or multipart Discord request only after a worker has resolved an optional
    /// preview. The PNG is ephemeral request data; canonical outbox bytes remain image-free.
    pub fn discord_request(&self, preview: Option<&[u8]>) -> Result<(String, Vec<u8>), AppError> {
        let Some(preview) = preview.filter(|bytes| !bytes.is_empty() && self.preview.is_some())
        else {
            return Ok((
                "application/json".to_owned(),
                self.discord_request_body_bytes()?,
            ));
        };
        let mut payload = self.payload.clone();
        let Some(embed) = payload.embeds.first_mut() else {
            return Err(AppError::Validation("invalid delivery envelope".into()));
        };
        embed.image = Some(EmbedImage {
            url: format!("attachment://{PREVIEW_FILENAME}"),
        });
        let json = serde_json::to_string(&payload).map_err(|_| AppError::Internal)?;
        // This is deliberately deterministic: the durable provider request does not need the
        // legacy notifier's random boundary and tests can inspect only the content type/parts.
        let request = multipart_request(&json, preview, "artifactmcpdeliverypreviewv1");
        Ok((request.content_type, request.body))
    }
    pub fn payload_sha256(&self) -> Result<String, AppError> {
        Ok(hex::encode(Sha256::digest(self.canonical_bytes()?)))
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
    pub fn event_type(&self) -> &str {
        &self.event_type
    }

    /// A worker may only use this after the envelope has been canonically decoded and bound to
    /// its durable queue row.
    #[must_use]
    pub fn preview(&self) -> Option<&DeliveryPreviewReferenceV1> {
        self.preview.as_ref()
    }

    // The artifact identifier is present in every artifact embed URL and cannot safely be
    // recovered from user-controlled title/description text. The immutable reference itself is
    // the authority; this helper exists solely to keep a malformed envelope from coupling a
    // preview of one artifact to another artifact notification.
    fn payload_artifact_id(&self, event: &WebhookEvent) -> &str {
        if accepts_preview(event) {
            self.payload
                .embeds
                .first()
                .and_then(|embed| embed.url.as_deref())
                .and_then(|url| url.rsplit('/').next())
                .unwrap_or_default()
        } else {
            ""
        }
    }
}

#[must_use]
pub fn stable_delivery_event_id(tenant: &OrgId, event: &WebhookEvent, subject: &str) -> String {
    let material = format!(
        "delivery-envelope-v1\0{}\0{}\0{subject}",
        tenant.0,
        event_name(event)
    );
    format!(
        "delivery:v1:{}",
        hex::encode(Sha256::digest(material.as_bytes()))
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ArtifactId;

    fn payload() -> NotificationPayload {
        NotificationPayload {
            artifact_id: ArtifactId("abc123".into()),
            title: "Previewed artifact".into(),
            url: "https://artifact.example/abc123".into(),
            description: "description".into(),
            uploader_label: "publisher".into(),
            category: "docs".into(),
            revision: 7,
            bytes: 42,
            viewer_email: None,
            body: None,
            resolver: None,
        }
    }

    fn reference() -> DeliveryPreviewReferenceV1 {
        DeliveryPreviewReferenceV1::new("abc123", 7, &"a".repeat(64)).expect("reference")
    }

    #[test]
    fn preview_reference_is_canonical_secret_free_and_byte_free() {
        let envelope = DeliveryEnvelopeV1::build_with_preview(
            "event".into(),
            &OrgId("acme".into()),
            &WebhookEvent::Published,
            &payload(),
            Some(reference()),
        )
        .expect("envelope");
        let bytes = envelope.canonical_bytes().expect("canonical");
        let text = String::from_utf8(bytes.clone()).expect("json");
        assert_eq!(
            DeliveryEnvelopeV1::decode_canonical(
                &bytes,
                &OrgId("acme".into()),
                &WebhookEvent::Published,
                "event",
                None,
            )
            .expect("canonical parity")
            .canonical_bytes()
            .expect("canonical again"),
            bytes
        );
        assert!(text.contains("\"preview\":{\"artifact_id\":\"abc123\",\"revision\":7"));
        assert!(!text.contains("preview.png"));
        assert!(!text.contains("/mnt/"));
        assert!(!text.contains("\u{89}PNG"));
    }

    #[test]
    fn preview_request_is_multipart_but_stored_envelope_remains_image_free() {
        let envelope = DeliveryEnvelopeV1::build_with_preview(
            "event".into(),
            &OrgId("acme".into()),
            &WebhookEvent::Updated,
            &payload(),
            Some(reference()),
        )
        .expect("envelope");
        let stored = envelope.canonical_bytes().expect("stored");
        let (content_type, request) = envelope
            .discord_request(Some(&[0x89, b'P', b'N', b'G']))
            .expect("multipart");
        assert!(content_type.starts_with("multipart/form-data; boundary="));
        assert!(
            request
                .windows(b"filename=\"preview.png\"".len())
                .any(|part| part == b"filename=\"preview.png\"")
        );
        assert!(
            request
                .windows(b"attachment://preview.png".len())
                .any(|part| part == b"attachment://preview.png")
        );
        assert!(
            !stored
                .windows(b"preview.png".len())
                .any(|part| part == b"preview.png")
        );
        assert!(
            !stored
                .windows(4)
                .any(|part| part == [0x89, b'P', b'N', b'G'])
        );
    }

    #[test]
    fn preview_reference_is_rejected_for_non_preview_events_or_other_artifacts() {
        for event in [
            WebhookEvent::Deleted,
            WebhookEvent::Feedback,
            WebhookEvent::Resolved,
        ] {
            assert!(
                DeliveryEnvelopeV1::build_with_preview(
                    "event".into(),
                    &OrgId("acme".into()),
                    &event,
                    &payload(),
                    Some(reference()),
                )
                .is_err()
            );
        }

        let mut other = payload();
        other.url = "https://artifact.example/def456".into();
        assert!(
            DeliveryEnvelopeV1::build_with_preview(
                "event".into(),
                &OrgId("acme".into()),
                &WebhookEvent::Restored,
                &other,
                Some(reference()),
            )
            .is_err()
        );
    }
}
