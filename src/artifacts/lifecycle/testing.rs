//! Test-only access to raw lifecycle operations, without an authorization grant.
//!
//! Production callers must use the grant-bearing `ArtifactService` interface.

use super::*;

macro_rules! raw_lifecycle_test_ext {
    ($(fn $name:ident(&self, $($arg:ident: $ty:ty),*) -> $output:ty;)*) => {
        /// Raw lifecycle operations for external integration tests only.
        #[expect(async_fn_in_trait, reason = "test-only extension trait is not used as a generic service")]
        pub trait RawLifecycleTestExt {
            $(async fn $name(&self, $($arg: $ty),*) -> $output;)*
        }

        impl RawLifecycleTestExt for ArtifactStore {
            $(async fn $name(&self, $($arg: $ty),*) -> $output {
                ArtifactStore::$name(self, $($arg),*).await
            })*
        }
    };
}

raw_lifecycle_test_ext! {
    fn read_body_for(&self, meta: &ArtifactMeta) -> Result<Option<ArtifactFile>, AppError>;
    fn read_bundle_file_for(&self, meta: &ArtifactMeta, relative_path: &str) -> Result<Option<ArtifactFile>, AppError>;
    fn read_revision_body_for(&self, meta: &ArtifactMeta, revision: u64, relative_path: Option<&str>) -> Result<Option<ArtifactFile>, AppError>;
    fn list_bundle_files_for(&self, meta: &ArtifactMeta, revision: Option<u64>) -> Result<Option<Vec<(String, u64)>>, AppError>;
    fn list_revisions_for(&self, meta: &ArtifactMeta) -> Result<RevisionHistory, AppError>;
    fn update_for(&self, meta: &ArtifactMeta, update: ArtifactUpdate, audit: MutationAudit) -> Result<UpdateArtifactResult, AppError>;
    fn restore_for(&self, meta: &ArtifactMeta, revision: u64, acting_client_id: Option<ClientId>, audit: MutationAudit) -> Result<RestoreArtifactResult, AppError>;
    fn delete_for(&self, meta: &ArtifactMeta, audit: MutationAudit) -> Result<bool, AppError>;
    fn set_category_for(&self, meta: &ArtifactMeta, category: &str, audit: MutationAudit) -> Result<ArtifactMeta, AppError>;
    fn set_hidden_for(&self, meta: &ArtifactMeta, hidden: bool, audit: MutationAudit) -> Result<ArtifactMeta, AppError>;
    fn move_to_org_for(&self, meta: &ArtifactMeta, target_org: &str, category: Option<&str>, audit: MutationAudit) -> Result<ArtifactMeta, AppError>;
}
