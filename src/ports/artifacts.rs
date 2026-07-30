//! Owned by U01 (sol) — artifact lifecycle and authorized-read contract.

use std::sync::Arc;

use super::{BoxFuture, PreviewService};
use crate::{
    error::AppError,
    model::{
        ArtifactFile, ArtifactId, ArtifactMeta, ArtifactUpdate, ClientId, DigestBackfillReport,
        OrgArtifacts, OrgId, PublishArtifact, PublishedArtifact, PublisherIdentity,
        RestoreArtifactResult, RevisionHistory, StorageAuditReport, UpdateArtifactResult,
    },
    security::{access::AuthorizedArtifact, audit::MutationAudit},
};

pub type BundleFileListing = Vec<(String, u64)>;

pub trait ArtifactService: Send + Sync {
    /// Load only tenant and display metadata. Body/history/subordinate reads require a grant.
    fn find_meta<'a>(
        &'a self,
        id: &'a ArtifactId,
    ) -> BoxFuture<'a, Result<Option<ArtifactMeta>, AppError>>;

    fn publish(
        &self,
        request: PublishArtifact,
        audit: MutationAudit,
    ) -> BoxFuture<'_, Result<PublishedArtifact, AppError>>;

    fn list_for_publisher<'a>(
        &'a self,
        publisher: &'a PublisherIdentity,
    ) -> BoxFuture<'a, Result<Vec<ArtifactMeta>, AppError>>;

    fn list_org_artifacts<'a>(
        &'a self,
        org: &'a OrgId,
        include_hidden: bool,
    ) -> BoxFuture<'a, Result<Vec<ArtifactMeta>, AppError>>;

    fn list_all_grouped_by_org(
        &self,
        include_hidden: bool,
    ) -> BoxFuture<'_, Result<Vec<OrgArtifacts>, AppError>>;

    fn list_org_ids<'a>(
        &'a self,
        org: &'a OrgId,
        include_hidden: bool,
    ) -> BoxFuture<'a, Result<Vec<ArtifactId>, AppError>>;

    fn read_body<'a>(
        &'a self,
        artifact: &'a AuthorizedArtifact,
    ) -> BoxFuture<'a, Result<Option<ArtifactFile>, AppError>>;

    /// Recheck readiness and read a digest-addressed preview under one lifecycle read guard.
    fn read_current_thumbnail<'a>(
        &'a self,
        artifact: &'a AuthorizedArtifact,
        digest: &'a str,
        previews: Arc<dyn PreviewService>,
    ) -> BoxFuture<'a, Result<Option<Vec<u8>>, AppError>> {
        // Deterministic test adapters do not own a filesystem lifecycle gate. Production
        // ArtifactStore overrides this with the linearized implementation above.
        let meta = artifact.meta().clone();
        let digest = digest.to_owned();
        Box::pin(async move {
            if meta.body_sha256 != digest {
                return Ok(None);
            }
            previews.read_thumbnail_sync(&meta, &digest)
        })
    }

    fn read_bundle_file<'a>(
        &'a self,
        artifact: &'a AuthorizedArtifact,
        relative_path: &'a str,
    ) -> BoxFuture<'a, Result<Option<ArtifactFile>, AppError>>;

    fn read_revision_body<'a>(
        &'a self,
        artifact: &'a AuthorizedArtifact,
        revision: u64,
        relative_path: Option<&'a str>,
    ) -> BoxFuture<'a, Result<Option<ArtifactFile>, AppError>>;

    /// List relative paths and byte sizes for a current or retained bundle snapshot.
    fn list_bundle_files<'a>(
        &'a self,
        artifact: &'a AuthorizedArtifact,
        revision: Option<u64>,
    ) -> BoxFuture<'a, Result<Option<BundleFileListing>, AppError>>;

    fn list_revisions<'a>(
        &'a self,
        artifact: &'a AuthorizedArtifact,
    ) -> BoxFuture<'a, Result<RevisionHistory, AppError>>;

    fn update(
        &self,
        artifact: AuthorizedArtifact,
        update: ArtifactUpdate,
        audit: MutationAudit,
    ) -> BoxFuture<'_, Result<UpdateArtifactResult, AppError>>;

    fn restore(
        &self,
        artifact: AuthorizedArtifact,
        revision: u64,
        acting_client_id: Option<ClientId>,
        audit: MutationAudit,
    ) -> BoxFuture<'_, Result<RestoreArtifactResult, AppError>>;

    fn delete(
        &self,
        artifact: AuthorizedArtifact,
        audit: MutationAudit,
    ) -> BoxFuture<'_, Result<bool, AppError>>;

    fn set_category(
        &self,
        artifact: AuthorizedArtifact,
        category: String,
        audit: MutationAudit,
    ) -> BoxFuture<'_, Result<ArtifactMeta, AppError>>;

    fn set_hidden(
        &self,
        artifact: AuthorizedArtifact,
        hidden: bool,
        audit: MutationAudit,
    ) -> BoxFuture<'_, Result<ArtifactMeta, AppError>>;

    fn move_to_org(
        &self,
        artifact: AuthorizedArtifact,
        target_org: OrgId,
        category: Option<String>,
        audit: MutationAudit,
    ) -> BoxFuture<'_, Result<ArtifactMeta, AppError>>;

    fn audit_storage(
        &self,
        clean_transient: bool,
    ) -> BoxFuture<'_, Result<StorageAuditReport, AppError>>;

    fn backfill_body_digests(&self) -> BoxFuture<'_, Result<DigestBackfillReport, AppError>>;
}
