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
    /// Case-insensitive owner match for presentation/filtering, never an access grant.
    pub is_owned_by_viewer: bool,
}

/// Retained revisions already expose a digest and an optional publisher ID through MCP.
/// HTTP history selects the digest but deliberately omits `client_id`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RevisionArtifactPublicFields {
    pub body_sha256: String,
    pub client_id: Option<ClientId>,
}

impl ArtifactPublicView {
    /// Fields deliberately unavailable as raw current metadata in this projection.
    ///
    /// `owner_email` becomes only `is_owned_by_viewer`. Current `client_id` is not emitted
    /// by metadata endpoints (the HTML templates have their own display contract).
    /// `body_sha256` becomes only the explicitly selected cache/preview `content_digest`.
    /// Retained revisions
    /// have a separate, existing digest/client-ID contract, selected by `from_revision`.
    pub const PRIVATE_META_FIELDS: &[&str] = &["client_id", "owner_email", "body_sha256"];

    /// Project authorized current metadata once. Pass a viewer email only for viewer output.
    /// The exhaustive destructure forces a public/private decision for every added meta field.
    #[must_use]
    pub fn from_meta(meta: &ArtifactMeta, viewer_email: Option<&str>) -> Self {
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
            owner_email,
            is_bundle,
            entry,
            revision,
            category,
            hidden,
            body_sha256,
            artifact_key,
        } = meta;
        Self {
            id: id.clone(),
            org: org.clone(),
            title: title.clone(),
            description: description.clone(),
            bytes: *bytes,
            created_at: created_at.clone(),
            is_bundle: *is_bundle,
            entry: entry.clone(),
            revision: *revision,
            category: category.clone(),
            details: CurrentArtifactPublicFields {
                updated_at: updated_at.clone(),
                uploader_label: uploader_label.clone(),
                hidden: *hidden,
                artifact_key: artifact_key.clone(),
                content_digest: body_sha256.clone(),
                is_owned_by_viewer: viewer_email.is_some_and(|viewer| {
                    owner_email
                        .as_deref()
                        .is_some_and(|owner| owner.eq_ignore_ascii_case(viewer))
                }),
            },
        }
    }
}

impl ArtifactPublicView<RevisionArtifactPublicFields> {
    /// Project retained metadata without inventing values for current-only fields.
    #[must_use]
    pub fn from_revision(meta: &ArtifactRevision) -> Self {
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
            id: artifact_id.clone(),
            org: org.clone(),
            title: title.clone(),
            description: description.clone(),
            bytes: *bytes,
            created_at: created_at.clone(),
            is_bundle: *is_bundle,
            entry: entry.clone(),
            revision: *revision,
            category: category.clone(),
            details: RevisionArtifactPublicFields {
                body_sha256: body_sha256.clone(),
                client_id: client_id.clone(),
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
        expected.extend(ArtifactPublicView::PRIVATE_META_FIELDS);
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
    fn public_projection_derives_ownership_without_retaining_email() {
        let mut meta = ArtifactMeta::fixture("a", "acme");
        meta.owner_email = Some("Owner@example.test".into());
        let view = ArtifactPublicView::from_meta(&meta, Some("owner@EXAMPLE.test"));
        assert!(view.details.is_owned_by_viewer);
        assert!(!format!("{view:?}").contains("Owner@example.test"));
        assert!(
            !ArtifactPublicView::from_meta(&meta, None)
                .details
                .is_owned_by_viewer
        );
        assert!(
            !ArtifactPublicView::from_meta(&meta, Some("other@example.test"))
                .details
                .is_owned_by_viewer
        );
    }
}
