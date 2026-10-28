//! Public share links backed by the production SQLite pool.

use std::sync::Arc;

use crate::{
    config::{Clock, IdSource},
    error::AppError,
    model::{CreateShare, PublicShare, ShareGrant, ShareToken},
    persistence::{
        db::{self, DbPool},
        shares,
    },
    ports::{BoxFuture, ShareService},
    security::audit::MutationAudit,
};

#[derive(Clone)]
pub struct ProductionShares {
    pool: DbPool,
    ids: Arc<dyn IdSource>,
    clock: Arc<dyn Clock>,
    audit_key: [u8; 32],
}

impl ProductionShares {
    pub fn new(
        pool: DbPool,
        ids: Arc<dyn IdSource>,
        clock: Arc<dyn Clock>,
        audit_key: [u8; 32],
    ) -> Self {
        Self {
            pool,
            ids,
            clock,
            audit_key,
        }
    }
}

impl ShareService for ProductionShares {
    fn resolve<'a>(
        &'a self,
        token: &'a ShareToken,
    ) -> BoxFuture<'a, Result<Option<ShareGrant>, AppError>> {
        let pool = self.pool.clone();
        let token = token.clone();
        Box::pin(
            async move { db::interact(&pool, move |conn| shares::resolve(conn, &token)).await },
        )
    }

    fn create(
        &self,
        artifact: crate::security::access::AuthorizedArtifact,
        request: CreateShare,
        audit: MutationAudit,
    ) -> BoxFuture<'_, Result<PublicShare, AppError>> {
        let ids = Arc::clone(&self.ids);
        let clock = Arc::clone(&self.clock);
        Box::pin(shares::create_audited_pooled(
            &self.pool,
            ids,
            clock,
            artifact,
            request,
            audit,
            self.audit_key,
        ))
    }

    fn list<'a>(
        &'a self,
        artifact: &'a crate::security::access::AuthorizedArtifact,
    ) -> BoxFuture<'a, Result<Vec<PublicShare>, AppError>> {
        let pool = self.pool.clone();
        let id = artifact.meta().id.clone();
        Box::pin(async move {
            db::interact(&pool, move |conn| shares::list_for_artifact(conn, &id)).await
        })
    }

    fn revoke(
        &self,
        artifact: crate::security::access::AuthorizedArtifact,
        token: ShareToken,
        audit: MutationAudit,
    ) -> BoxFuture<'_, Result<bool, AppError>> {
        Box::pin(shares::revoke_audited_pooled(
            &self.pool,
            artifact,
            token,
            audit,
            self.audit_key,
        ))
    }
}
