//! Configurable, deterministic artifact adapter for tests only.

use super::{ArtifactService, BoxFuture, artifacts::BundleFileListing};
use crate::{
    error::AppError,
    model::*,
    security::{access::AuthorizedArtifact, audit::MutationAudit},
};
use std::{collections::BTreeMap, sync::Mutex};

/// A synchronous script. It can inspect arguments, return a fixed result, or consume a queue.
pub type Script<F> = Option<Box<F>>;

/// Optional method scripts. Missing scripts use seeded reads or the fake's default error.
/// A call is recorded when the method is called, even if its future is dropped unpolled.
/// Scripts run later, when the returned future is polled.
#[derive(Default)]
#[expect(
    clippy::type_complexity,
    reason = "Scripts mirror the port signatures so fixture closures receive typed arguments"
)]
pub struct ArtifactScripts {
    pub find_meta:
        Script<dyn Fn(&ArtifactId) -> Result<Option<ArtifactMeta>, AppError> + Send + Sync>,
    pub find_meta_by_key:
        Script<dyn Fn(&OrgId, &str) -> Result<Option<ArtifactMeta>, AppError> + Send + Sync>,
    pub publish: Script<
        dyn Fn(PublishArtifact, MutationAudit) -> Result<PublishedArtifact, AppError> + Send + Sync,
    >,
    pub list_for_publisher:
        Script<dyn Fn(&PublisherIdentity) -> Result<Vec<ArtifactMeta>, AppError> + Send + Sync>,
    pub list_org_artifacts:
        Script<dyn Fn(&OrgId, bool) -> Result<Vec<ArtifactMeta>, AppError> + Send + Sync>,
    pub list_all_grouped_by_org:
        Script<dyn Fn(bool) -> Result<Vec<OrgArtifacts>, AppError> + Send + Sync>,
    pub list_org_ids:
        Script<dyn Fn(&OrgId, bool) -> Result<Vec<ArtifactId>, AppError> + Send + Sync>,
    pub read_body:
        Script<dyn Fn(&AuthorizedArtifact) -> Result<Option<ArtifactFile>, AppError> + Send + Sync>,
    pub read_bundle_file: Script<
        dyn Fn(&AuthorizedArtifact, &str) -> Result<Option<ArtifactFile>, AppError> + Send + Sync,
    >,
    pub read_revision_body: Script<
        dyn Fn(&AuthorizedArtifact, u64, Option<&str>) -> Result<Option<ArtifactFile>, AppError>
            + Send
            + Sync,
    >,
    pub list_bundle_files: Script<
        dyn Fn(&AuthorizedArtifact, Option<u64>) -> Result<Option<BundleFileListing>, AppError>
            + Send
            + Sync,
    >,
    pub list_revisions:
        Script<dyn Fn(&AuthorizedArtifact) -> Result<RevisionHistory, AppError> + Send + Sync>,
    pub update: Script<
        dyn Fn(
                AuthorizedArtifact,
                ArtifactUpdate,
                MutationAudit,
            ) -> Result<UpdateArtifactResult, AppError>
            + Send
            + Sync,
    >,
    pub restore: Script<
        dyn Fn(
                AuthorizedArtifact,
                u64,
                Option<ClientId>,
                MutationAudit,
            ) -> Result<RestoreArtifactResult, AppError>
            + Send
            + Sync,
    >,
    pub delete:
        Script<dyn Fn(AuthorizedArtifact, MutationAudit) -> Result<bool, AppError> + Send + Sync>,
    pub set_category: Script<
        dyn Fn(AuthorizedArtifact, String, MutationAudit) -> Result<ArtifactMeta, AppError>
            + Send
            + Sync,
    >,
    pub set_hidden: Script<
        dyn Fn(AuthorizedArtifact, bool, MutationAudit) -> Result<ArtifactMeta, AppError>
            + Send
            + Sync,
    >,
    pub move_to_org: Script<
        dyn Fn(
                AuthorizedArtifact,
                OrgId,
                Option<String>,
                MutationAudit,
            ) -> Result<ArtifactMeta, AppError>
            + Send
            + Sync,
    >,
    pub audit_storage: Script<dyn Fn(bool) -> Result<StorageAuditReport, AppError> + Send + Sync>,
    pub backfill_body_digests:
        Script<dyn Fn() -> Result<DigestBackfillReport, AppError> + Send + Sync>,
}

/// In-memory metadata/body reads plus configurable scripts and an ordered method-name log.
/// This fake does not model persistence, authorization, lifecycle cleanup, or transactions.
/// Mutations are refused unless scripted; scripts decide whether to change their own state.
/// Unscripted `find_meta_by_key` reports no match, as the trait default does.
/// `read_current_thumbnail` keeps the trait default and is not recorded in [`Self::calls`].
///
/// Unscripted reads have three states:
/// - Nothing seeded ([`Self::default`]): `find_meta` returns the default error.
/// - At least one row seeded: `find_meta` returns the row, or `Ok(None)` for an unknown ID.
/// - `read_body` returns a seeded body, or the default error when that row has no body.
///
/// To inject an error, set `fake.scripts.delete = Some(Box::new(|_, _| Err(AppError::Internal)))`.
/// Closures may capture shared state for repeatable failures, queued results, or cross-port logs.
/// Prefer a local script for fixture-specific behavior instead of adding it to this adapter.
pub struct FakeArtifacts {
    pub scripts: ArtifactScripts,
    metas: Option<BTreeMap<ArtifactId, ArtifactMeta>>,
    bodies: BTreeMap<ArtifactId, ArtifactFile>,
    calls: Mutex<Vec<&'static str>>,
    error: fn() -> AppError,
}

impl Default for FakeArtifacts {
    fn default() -> Self {
        Self {
            scripts: ArtifactScripts::default(),
            metas: None,
            bodies: BTreeMap::new(),
            calls: Mutex::new(Vec::new()),
            error: || AppError::Internal,
        }
    }
}

impl FakeArtifacts {
    /// Seed metadata. All other unscripted capabilities still refuse calls.
    #[must_use]
    pub fn with(meta: ArtifactMeta) -> Self {
        let mut fake = Self::default();
        fake.seed(meta, None);
        fake
    }

    /// Seed another metadata row and, optionally, its body.
    pub fn seed(&mut self, meta: ArtifactMeta, body: Option<ArtifactFile>) {
        if let Some(body) = body {
            self.bodies.insert(meta.id.clone(), body);
        }
        self.metas
            .get_or_insert_default()
            .insert(meta.id.clone(), meta);
    }

    /// Select the error returned by unscripted capabilities.
    #[must_use]
    pub fn with_error(error: fn() -> AppError) -> Self {
        Self {
            error,
            ..Self::default()
        }
    }

    /// The method names called so far, in call order.
    #[must_use]
    pub fn calls(&self) -> Vec<&'static str> {
        self.calls
            .lock()
            .expect("fake call log is not poisoned")
            .clone()
    }

    /// Clear the recorded calls. Seeds, scripts and the default error are kept.
    pub fn reset(&self) {
        self.calls
            .lock()
            .expect("fake call log is not poisoned")
            .clear();
    }

    fn record(&self, method: &'static str) {
        self.calls
            .lock()
            .expect("fake call log is not poisoned")
            .push(method);
    }
}

impl ArtifactService for FakeArtifacts {
    fn find_meta<'a>(
        &'a self,
        id: &'a ArtifactId,
    ) -> BoxFuture<'a, Result<Option<ArtifactMeta>, AppError>> {
        self.record("find_meta");
        Box::pin(async move {
            if let Some(script) = &self.scripts.find_meta {
                script(id)
            } else {
                self.metas
                    .as_ref()
                    .map(|metas| metas.get(id).cloned())
                    .ok_or_else(self.error)
            }
        })
    }

    fn find_meta_by_key<'a>(
        &'a self,
        org: &'a OrgId,
        artifact_key: &'a str,
    ) -> BoxFuture<'a, Result<Option<ArtifactMeta>, AppError>> {
        self.record("find_meta_by_key");
        Box::pin(async move {
            match &self.scripts.find_meta_by_key {
                Some(script) => script(org, artifact_key),
                None => Ok(None),
            }
        })
    }

    fn publish(
        &self,
        request: PublishArtifact,
        audit: MutationAudit,
    ) -> BoxFuture<'_, Result<PublishedArtifact, AppError>> {
        self.record("publish");
        Box::pin(async move {
            if let Some(script) = &self.scripts.publish {
                script(request, audit)
            } else {
                Err((self.error)())
            }
        })
    }

    fn list_for_publisher<'a>(
        &'a self,
        publisher: &'a PublisherIdentity,
    ) -> BoxFuture<'a, Result<Vec<ArtifactMeta>, AppError>> {
        self.record("list_for_publisher");
        Box::pin(async move {
            if let Some(script) = &self.scripts.list_for_publisher {
                script(publisher)
            } else {
                Err((self.error)())
            }
        })
    }

    fn list_org_artifacts<'a>(
        &'a self,
        org: &'a OrgId,
        include_hidden: bool,
    ) -> BoxFuture<'a, Result<Vec<ArtifactMeta>, AppError>> {
        self.record("list_org_artifacts");
        Box::pin(async move {
            if let Some(script) = &self.scripts.list_org_artifacts {
                script(org, include_hidden)
            } else {
                Err((self.error)())
            }
        })
    }

    fn list_all_grouped_by_org(
        &self,
        include_hidden: bool,
    ) -> BoxFuture<'_, Result<Vec<OrgArtifacts>, AppError>> {
        self.record("list_all_grouped_by_org");
        Box::pin(async move {
            if let Some(script) = &self.scripts.list_all_grouped_by_org {
                script(include_hidden)
            } else {
                Err((self.error)())
            }
        })
    }

    fn list_org_ids<'a>(
        &'a self,
        org: &'a OrgId,
        include_hidden: bool,
    ) -> BoxFuture<'a, Result<Vec<ArtifactId>, AppError>> {
        self.record("list_org_ids");
        Box::pin(async move {
            if let Some(script) = &self.scripts.list_org_ids {
                script(org, include_hidden)
            } else {
                Err((self.error)())
            }
        })
    }

    fn read_body<'a>(
        &'a self,
        artifact: &'a AuthorizedArtifact,
    ) -> BoxFuture<'a, Result<Option<ArtifactFile>, AppError>> {
        self.record("read_body");
        Box::pin(async move {
            if let Some(script) = &self.scripts.read_body {
                script(artifact)
            } else {
                self.bodies
                    .get(&artifact.meta().id)
                    .cloned()
                    .map(Some)
                    .ok_or_else(self.error)
            }
        })
    }

    fn read_bundle_file<'a>(
        &'a self,
        artifact: &'a AuthorizedArtifact,
        relative_path: &'a str,
    ) -> BoxFuture<'a, Result<Option<ArtifactFile>, AppError>> {
        self.record("read_bundle_file");
        Box::pin(async move {
            if let Some(script) = &self.scripts.read_bundle_file {
                script(artifact, relative_path)
            } else {
                Err((self.error)())
            }
        })
    }

    fn read_revision_body<'a>(
        &'a self,
        artifact: &'a AuthorizedArtifact,
        revision: u64,
        relative_path: Option<&'a str>,
    ) -> BoxFuture<'a, Result<Option<ArtifactFile>, AppError>> {
        self.record("read_revision_body");
        Box::pin(async move {
            if let Some(script) = &self.scripts.read_revision_body {
                script(artifact, revision, relative_path)
            } else {
                Err((self.error)())
            }
        })
    }

    fn list_bundle_files<'a>(
        &'a self,
        artifact: &'a AuthorizedArtifact,
        revision: Option<u64>,
    ) -> BoxFuture<'a, Result<Option<BundleFileListing>, AppError>> {
        self.record("list_bundle_files");
        Box::pin(async move {
            if let Some(script) = &self.scripts.list_bundle_files {
                script(artifact, revision)
            } else {
                Err((self.error)())
            }
        })
    }

    fn list_revisions<'a>(
        &'a self,
        artifact: &'a AuthorizedArtifact,
    ) -> BoxFuture<'a, Result<RevisionHistory, AppError>> {
        self.record("list_revisions");
        Box::pin(async move {
            if let Some(script) = &self.scripts.list_revisions {
                script(artifact)
            } else {
                Err((self.error)())
            }
        })
    }

    fn update(
        &self,
        artifact: AuthorizedArtifact,
        update: ArtifactUpdate,
        audit: MutationAudit,
    ) -> BoxFuture<'_, Result<UpdateArtifactResult, AppError>> {
        self.record("update");
        Box::pin(async move {
            if let Some(script) = &self.scripts.update {
                script(artifact, update, audit)
            } else {
                Err((self.error)())
            }
        })
    }

    fn restore(
        &self,
        artifact: AuthorizedArtifact,
        revision: u64,
        acting_client_id: Option<ClientId>,
        audit: MutationAudit,
    ) -> BoxFuture<'_, Result<RestoreArtifactResult, AppError>> {
        self.record("restore");
        Box::pin(async move {
            if let Some(script) = &self.scripts.restore {
                script(artifact, revision, acting_client_id, audit)
            } else {
                Err((self.error)())
            }
        })
    }

    fn delete(
        &self,
        artifact: AuthorizedArtifact,
        audit: MutationAudit,
    ) -> BoxFuture<'_, Result<bool, AppError>> {
        self.record("delete");
        Box::pin(async move {
            if let Some(script) = &self.scripts.delete {
                script(artifact, audit)
            } else {
                Err((self.error)())
            }
        })
    }

    fn set_category(
        &self,
        artifact: AuthorizedArtifact,
        category: String,
        audit: MutationAudit,
    ) -> BoxFuture<'_, Result<ArtifactMeta, AppError>> {
        self.record("set_category");
        Box::pin(async move {
            if let Some(script) = &self.scripts.set_category {
                script(artifact, category, audit)
            } else {
                Err((self.error)())
            }
        })
    }

    fn set_hidden(
        &self,
        artifact: AuthorizedArtifact,
        hidden: bool,
        audit: MutationAudit,
    ) -> BoxFuture<'_, Result<ArtifactMeta, AppError>> {
        self.record("set_hidden");
        Box::pin(async move {
            if let Some(script) = &self.scripts.set_hidden {
                script(artifact, hidden, audit)
            } else {
                Err((self.error)())
            }
        })
    }

    fn move_to_org(
        &self,
        artifact: AuthorizedArtifact,
        target_org: OrgId,
        category: Option<String>,
        audit: MutationAudit,
    ) -> BoxFuture<'_, Result<ArtifactMeta, AppError>> {
        self.record("move_to_org");
        Box::pin(async move {
            if let Some(script) = &self.scripts.move_to_org {
                script(artifact, target_org, category, audit)
            } else {
                Err((self.error)())
            }
        })
    }

    fn audit_storage(
        &self,
        clean_transient: bool,
    ) -> BoxFuture<'_, Result<StorageAuditReport, AppError>> {
        self.record("audit_storage");
        Box::pin(async move {
            if let Some(script) = &self.scripts.audit_storage {
                script(clean_transient)
            } else {
                Err((self.error)())
            }
        })
    }

    fn backfill_body_digests(&self) -> BoxFuture<'_, Result<DigestBackfillReport, AppError>> {
        self.record("backfill_body_digests");
        Box::pin(async move {
            if let Some(script) = &self.scripts.backfill_body_digests {
                script()
            } else {
                Err((self.error)())
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::security::access::AccessPolicy;

    #[tokio::test]
    async fn seeded_reads_require_a_grant_and_unknown_ids_are_missing() {
        let meta = ArtifactMeta::fixture("abc123def456", "acme");
        let mut fake = FakeArtifacts::default();
        fake.seed(
            meta.clone(),
            Some(ArtifactFile {
                content: b"seeded body".to_vec(),
                content_type: "text/html".to_owned(),
            }),
        );
        assert_eq!(fake.find_meta(&meta.id).await.unwrap(), Some(meta.clone()));
        assert_eq!(
            fake.find_meta(&ArtifactId::from("zzzzzzzzzzzz"))
                .await
                .unwrap(),
            None
        );
        let grant = AccessPolicy::authorize_viewer(
            &Viewer {
                email: Some(EmailAddress::from("reader@acme.test")),
                org: Some(OrgId::from("acme")),
                is_admin: false,
            },
            Some(meta),
        )
        .unwrap();
        assert_eq!(
            fake.read_body(&grant).await.unwrap().unwrap().content,
            b"seeded body"
        );
        assert_eq!(fake.calls(), ["find_meta", "find_meta", "read_body"]);
    }

    #[tokio::test]
    async fn scripts_override_seeds_and_record_failures_in_order() {
        let meta = ArtifactMeta::fixture("abc123def456", "acme");
        let id = meta.id.clone();
        let mut fake = FakeArtifacts::with(meta);
        let replies = Mutex::new(std::collections::VecDeque::from([
            Err(AppError::Unavailable("scripted outage".to_owned())),
            Ok(None),
        ]));
        fake.scripts.find_meta = Some(Box::new(move |_| {
            replies.lock().unwrap().pop_front().unwrap()
        }));
        assert_eq!(
            fake.find_meta(&id).await,
            Err(AppError::Unavailable("scripted outage".to_owned()))
        );
        assert_eq!(fake.find_meta(&id).await.unwrap(), None);
        assert_eq!(fake.backfill_body_digests().await, Err(AppError::Internal));
        assert_eq!(
            fake.calls(),
            ["find_meta", "find_meta", "backfill_body_digests"]
        );
        fake.reset();
        assert!(fake.calls().is_empty());
    }
}
