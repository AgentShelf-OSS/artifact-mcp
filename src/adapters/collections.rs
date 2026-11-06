//! SQLite collection adapter with audit policy fixed at composition time.

use crate::{
    error::AppError,
    persistence::{collections::CollectionStore, db::DbPool},
    ports::{
        BoxFuture,
        collections::{
            Collection, CollectionActor, CollectionService, CollectionUpdate, GalleryPreferences,
            MembershipDiff, NewCollection,
        },
    },
    security::audit::{AuditAccess, MutationAudit},
};

pub struct ProductionCollections {
    store: CollectionStore,
}

impl ProductionCollections {
    #[must_use]
    pub fn new(pool: DbPool, audit_access: Option<&AuditAccess>) -> Self {
        let store = match audit_access {
            Some(audit) => CollectionStore::with_audit(pool, audit.mutation_key()),
            None => CollectionStore::new(pool),
        };
        Self { store }
    }
}

impl CollectionService for ProductionCollections {
    fn list<'a>(
        &'a self,
        actor: &'a CollectionActor,
        limit: usize,
    ) -> BoxFuture<'a, Result<Vec<Collection>, AppError>> {
        Box::pin(async move { self.store.list(actor, limit).await })
    }
    fn list_for_org<'a>(
        &'a self,
        actor: &'a CollectionActor,
        org: String,
        limit: usize,
    ) -> BoxFuture<'a, Result<Vec<Collection>, AppError>> {
        Box::pin(async move { self.store.list_for_org(actor, org, limit).await })
    }
    fn create_atomic<'a>(
        &'a self,
        actor: &'a CollectionActor,
        collection: NewCollection,
        audit: MutationAudit,
    ) -> BoxFuture<'a, Result<Collection, AppError>> {
        Box::pin(async move {
            self.store
                .create_atomic(actor, collection, Some(audit))
                .await
        })
    }
    fn members<'a>(
        &'a self,
        actor: &'a CollectionActor,
        collection_id: String,
    ) -> BoxFuture<'a, Result<Vec<String>, AppError>> {
        Box::pin(async move { self.store.members(actor, collection_id).await })
    }
    fn readable_members<'a>(
        &'a self,
        actor: &'a CollectionActor,
        collection_id: String,
        offset: usize,
        limit: usize,
    ) -> BoxFuture<'a, Result<Vec<String>, AppError>> {
        Box::pin(async move {
            self.store
                .readable_members(actor, collection_id, offset, limit)
                .await
        })
    }
    fn add_memberships_audited<'a>(
        &'a self,
        actor: &'a CollectionActor,
        collection_id: String,
        artifact_ids: Vec<String>,
        audit: MutationAudit,
    ) -> BoxFuture<'a, Result<MembershipDiff, AppError>> {
        Box::pin(async move {
            self.store
                .add_memberships_diff_audited(actor, collection_id, artifact_ids, Some(audit))
                .await
        })
    }
    fn remove_memberships_audited<'a>(
        &'a self,
        actor: &'a CollectionActor,
        collection_id: String,
        artifact_ids: Vec<String>,
        audit: MutationAudit,
    ) -> BoxFuture<'a, Result<MembershipDiff, AppError>> {
        Box::pin(async move {
            self.store
                .remove_memberships_diff_audited(actor, collection_id, artifact_ids, Some(audit))
                .await
        })
    }
    fn update_audited<'a>(
        &'a self,
        actor: &'a CollectionActor,
        collection_id: String,
        update: CollectionUpdate,
        audit: MutationAudit,
    ) -> BoxFuture<'a, Result<(), AppError>> {
        Box::pin(async move {
            self.store
                .update_audited(actor, collection_id, update, Some(audit))
                .await
        })
    }
    fn delete_audited<'a>(
        &'a self,
        actor: &'a CollectionActor,
        collection_id: String,
        audit: MutationAudit,
    ) -> BoxFuture<'a, Result<(), AppError>> {
        Box::pin(async move {
            self.store
                .delete_audited(actor, collection_id, Some(audit))
                .await
        })
    }
    fn preferences<'a>(
        &'a self,
        actor: &'a CollectionActor,
    ) -> BoxFuture<'a, Result<GalleryPreferences, AppError>> {
        Box::pin(async move { self.store.preferences(actor).await })
    }
    fn save_preferences<'a>(
        &'a self,
        actor: &'a CollectionActor,
        preferences: GalleryPreferences,
    ) -> BoxFuture<'a, Result<GalleryPreferences, AppError>> {
        Box::pin(async move { self.store.save_preferences(actor, preferences).await })
    }
}
