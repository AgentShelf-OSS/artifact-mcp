//! Publisher-key and organization directory lookups backed by the production SQLite pool.

use rusqlite::OptionalExtension as _;

use crate::{
    error::AppError,
    model::{ClientId, EmailAddress, OrgId},
    persistence::{
        db::{self, DbPool},
        orgs::OrgStore,
    },
    ports::BoxFuture,
    security::{
        auth::{KeyHash, PublisherKeyDirectory, PublisherKeyRecord},
        identity::OrgDirectory,
    },
};

#[derive(Clone)]
pub struct ProductionDirectories {
    pool: DbPool,
}

impl ProductionDirectories {
    pub const fn new(pool: DbPool) -> Self {
        Self { pool }
    }
}

impl std::fmt::Debug for ProductionDirectories {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ProductionDirectories")
            .finish_non_exhaustive()
    }
}

impl PublisherKeyDirectory for ProductionDirectories {
    fn find_active<'a>(
        &'a self,
        hash: &'a KeyHash,
    ) -> BoxFuture<'a, Result<Option<PublisherKeyRecord>, AppError>> {
        let pool = self.pool.clone();
        let hash = hash.expose().to_owned();
        Box::pin(async move {
            db::interact(&pool, move |conn| {
                conn.query_row(
                    "SELECT client_id, org, label, role FROM api_keys \
                     WHERE key_hash = ?1 AND revoked_at IS NULL",
                    (&hash,),
                    |row| {
                        Ok(PublisherKeyRecord {
                            client_id: ClientId(row.get(0)?),
                            org: OrgId(row.get(1)?),
                            label: row.get(2)?,
                            role: row.get(3)?,
                        })
                    },
                )
                .optional()
                .map_err(|error| {
                    tracing::error!(error = %error, "publisher key lookup failed");
                    AppError::Internal
                })
            })
            .await
        })
    }
}

impl OrgDirectory for ProductionDirectories {
    fn org_for_email<'a>(
        &'a self,
        email: &'a EmailAddress,
    ) -> BoxFuture<'a, Result<Option<OrgId>, AppError>> {
        let store = OrgStore::new(self.pool.clone());
        Box::pin(async move { store.org_for_email(email).await })
    }

    fn org_for_domain<'a>(
        &'a self,
        domain: &'a str,
    ) -> BoxFuture<'a, Result<Option<OrgId>, AppError>> {
        let store = OrgStore::new(self.pool.clone());
        Box::pin(async move { store.org_for_domain(domain).await })
    }
}
