//! Public metadata policy. Ingress adapters opt into fields and retain their wire order.
//!
//! This type deliberately does not implement `Serialize`: serializing the whole view would
//! opt every endpoint into new fields. It is a snapshot, not an authorization witness.

use super::{ArtifactId, ArtifactMeta, ArtifactRevision, ClientId, OrgId, Timestamp};

/// One public projection, with current-only or revision-only fields selected at construction.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArtifactPublicView<Details = CurrentArtifactPublicFields> {
    pub id: ArtifactId,
    pub org: OrgId,
    pub title: String,
    pub description: String,
    pub bytes: u64,
    pub created_at: Timestamp,
    pub is_bundle: bool,
    pub entry: String,
    pub revision: u64,
    pub category: String,
    pub details: Details,
}

/// Public fields available for a current artifact. No owner email or publisher key ID.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CurrentArtifactPublicFields {
    pub updated_at: Timestamp,
    pub uploader_label: String,
    pub hidden: bool,
    pub artifact_key: Option<String>,
    /// Full digest for the existing thumbnail cache URLs and MCP preview-regeneration result.
    /// Collection previews keep an empty `?v=` when absent; never shorten this digest.
    pub content_digest: String,
}

/// Retained revisions already expose a digest and an optional publisher ID through MCP.
/// HTTP history selects the digest but deliberately omits `client_id`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RevisionArtifactPublicFields {
    pub body_sha256: String,
    pub client_id: Option<ClientId>,
}

impl ArtifactPublicView {
    /// `ArtifactMeta` fields this projection does not carry under their own name.
    ///
    /// - `owner_email` and current `client_id` are withheld entirely. Metadata endpoints never
    ///   emit them (the HTML templates have their own display contract).
    /// - `body_sha256` is renamed: it leaves only as the explicitly selected cache/preview
    ///   `content_digest`.
    ///
    /// Retained revisions have a separate, existing digest/client-ID contract (`from_revision`).
    pub const WITHHELD_META_FIELDS: &[&str] = &["client_id", "owner_email", "body_sha256"];

    /// Project borrowed current metadata. Prefer `From<ArtifactMeta>` when the caller owns it.
    #[must_use]
    pub fn from_meta(meta: &ArtifactMeta) -> Self {
        Self::from(meta.clone())
    }
}

impl From<ArtifactMeta> for ArtifactPublicView {
    /// The exhaustive destructure forces a public or withheld decision for every added field.
    fn from(meta: ArtifactMeta) -> Self {
        let ArtifactMeta {
            id,
            client_id: _,
            org,
            title,
            description,
            bytes,
            created_at,
            updated_at,
            uploader_label,
            owner_email: _,
            is_bundle,
            entry,
            revision,
            category,
            hidden,
            body_sha256,
            artifact_key,
        } = meta;
        Self {
            id,
            org,
            title,
            description,
            bytes,
            created_at,
            is_bundle,
            entry,
            revision,
            category,
            details: CurrentArtifactPublicFields {
                updated_at,
                uploader_label,
                hidden,
                artifact_key,
                content_digest: body_sha256,
            },
        }
    }
}

impl ArtifactPublicView<RevisionArtifactPublicFields> {
    /// Project borrowed retained metadata. Prefer `From<ArtifactRevision>` when owned.
    #[must_use]
    pub fn from_revision(meta: &ArtifactRevision) -> Self {
        Self::from(meta.clone())
    }
}

impl From<ArtifactRevision> for ArtifactPublicView<RevisionArtifactPublicFields> {
    /// Project retained metadata without inventing values for current-only fields. The
    /// exhaustive destructure forces a decision for every added revision field.
    fn from(meta: ArtifactRevision) -> Self {
        let ArtifactRevision {
            artifact_id,
            org,
            revision,
            title,
            description,
            category,
            bytes,
            is_bundle,
            entry,
            body_sha256,
            created_at,
            client_id,
        } = meta;
        Self {
            id: artifact_id,
            org,
            title,
            description,
            bytes,
            created_at,
            is_bundle,
            entry,
            revision,
            category,
            details: RevisionArtifactPublicFields {
                body_sha256,
                client_id,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_projection_classifies_every_meta_field() {
        // The public names plus explicit raw-private list must cover the actual serialized
        // domain record. Adding a field also fails the exhaustive constructor above.
        let mut meta = ArtifactMeta::fixture("a", "acme");
        meta.artifact_key = Some("project/topic".into());
        let actual = serde_json::to_value(meta).unwrap();
        let mut expected = vec![
            "id",
            "org",
            "title",
            "description",
            "bytes",
            "created_at",
            "updated_at",
            "uploader_label",
            "is_bundle",
            "entry",
            "revision",
            "category",
            "hidden",
            "artifact_key",
        ];
        expected.extend(ArtifactPublicView::WITHHELD_META_FIELDS);
        expected.sort_unstable();
        assert_eq!(
            actual
                .as_object()
                .unwrap()
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            expected
        );
    }

    #[test]
    fn public_projection_withholds_owner_email_and_client_id() {
        let mut meta = ArtifactMeta::fixture("a", "acme");
        meta.owner_email = Some("Owner@example.test".into());
        meta.client_id = "publisher-key-id".into();
        let view = format!("{:?}", ArtifactPublicView::from(meta));
        assert!(!view.contains("Owner@example.test"));
        assert!(!view.contains("publisher-key-id"));
    }
}
