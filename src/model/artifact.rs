//! Artifact metadata, lifecycle commands, and storage reports.

use serde::{Deserialize, Serialize};

use super::{ArtifactId, ClientId, OrgId, PublisherIdentity, Timestamp};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactMeta {
    pub id: ArtifactId,
    pub client_id: ClientId,
    pub org: OrgId,
    pub title: String,
    pub description: String,
    pub bytes: u64,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
    pub uploader_label: String,
    /// Immutable, server-controlled publish-time owner.  This stays in the authorized server
    /// model; member renderers project only `is_owned_by_viewer` and never this email.
    pub owner_email: Option<String>,
    pub is_bundle: bool,
    pub entry: String,
    pub revision: u64,
    pub category: String,
    pub hidden: bool,
    pub body_sha256: String,
    /// Optional publisher-chosen key, unique per organization (ADR-0015).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact_key: Option<String>,
}

#[cfg(any(test, feature = "test-util"))]
impl ArtifactMeta {
    /// Test metadata for the given artifact and organization, with publisher `publisher`,
    /// title `Artifact`, revision 1, empty content metadata, and no owner or artifact key.
    #[must_use]
    pub fn fixture(id: &str, org: &str) -> Self {
        Self {
            id: ArtifactId::from(id),
            client_id: ClientId::from("publisher"),
            org: OrgId::from(org),
            title: "Artifact".to_owned(),
            description: String::new(),
            bytes: 0,
            created_at: Timestamp(String::new()),
            updated_at: Timestamp(String::new()),
            uploader_label: String::new(),
            owner_email: None,
            is_bundle: false,
            entry: String::new(),
            revision: 1,
            category: String::new(),
            hidden: false,
            body_sha256: String::new(),
            artifact_key: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ArtifactContent {
    SingleHtml(String),
    Bundle {
        /// Ordered `(relative_path, contents)` pairs — **caller order is load-bearing**.
        ///
        /// Entry auto-selection picks the first `.html` in publisher order.
        /// For `{"z.html", "a.html"}` with no `index.html` and no explicit entry, this selects
        /// `z.html`; a sorted container such as `BTreeMap` would incorrectly select `a.html`.
        files: Vec<(String, String)>,
        entry: Option<String>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PublishArtifact {
    pub publisher: PublisherIdentity,
    pub target_org: OrgId,
    pub title: Option<String>,
    pub description: Option<String>,
    pub category: Option<String>,
    pub content: ArtifactContent,
    /// Optional `(org, artifact_key)` to record on the new artifact (ADR-0015).
    pub artifact_key: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PublishedArtifact {
    pub meta: ArtifactMeta,
    pub file_count: Option<usize>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ArtifactUpdate {
    pub expected_revision: u64,
    pub acting_client_id: Option<ClientId>,
    pub title: Option<String>,
    pub description: Option<String>,
    pub category: Option<String>,
    pub content: Option<ArtifactContent>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UpdateArtifactResult {
    pub meta: ArtifactMeta,
    pub changed: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RestoreArtifactResult {
    pub meta: ArtifactMeta,
    pub restored_from: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactRevision {
    pub artifact_id: ArtifactId,
    pub org: OrgId,
    pub revision: u64,
    pub title: String,
    pub description: String,
    pub category: String,
    pub bytes: u64,
    pub is_bundle: bool,
    pub entry: String,
    pub body_sha256: String,
    pub created_at: Timestamp,
    pub client_id: Option<ClientId>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RevisionHistory {
    pub current: u64,
    pub revisions: Vec<ArtifactRevision>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArtifactFile {
    pub content: Vec<u8>,
    pub content_type: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OrgArtifacts {
    pub org: OrgId,
    pub items: Vec<ArtifactMeta>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StorageAuditReport {
    pub missing_bodies: Vec<String>,
    pub divergent_bodies: Vec<String>,
    pub orphan_bodies: Vec<String>,
    pub orphan_history: Vec<String>,
    pub transient_paths: Vec<String>,
    pub recovered_paths: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DigestBackfillReport {
    pub scanned: usize,
    pub updated: usize,
}
