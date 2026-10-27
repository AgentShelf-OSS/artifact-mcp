//! Adapter stage: construct the production port adapters for the router.

use std::sync::Arc;

use crate::{
    adapters::{
        ProductionAdmin, ProductionDiscussions, ProductionEngagement, ProductionHealth,
        ProductionShares,
    },
    config::{AppConfig, IdSource, SystemClock},
    error::AppError,
    integrations::access_sync::AccessSync,
    persistence::{
        db::DbPool, feedback_delivery::DeliveryPlanningContext, keys::KeyStore, orgs::OrgStore,
        viewer_state_service::SqliteViewerState,
    },
    ports::{
        AdminService, DiscussionService, EngagementService, HealthProbe, ShareService,
        state::ViewerStateService,
    },
    security::audit::{AuditAccess, parse_hmac_key},
};

use super::workers::IntegrationStores;

/// Port adapters the router receives through `AppDeps`.
pub struct PortServices {
    pub admin: Arc<dyn AdminService>,
    pub discussions: Arc<dyn DiscussionService>,
    pub engagement: Arc<dyn EngagementService>,
    pub shares: Arc<dyn ShareService>,
    pub health: Arc<dyn HealthProbe>,
    pub viewer_state: Arc<dyn ViewerStateService>,
    pub audit_access: Option<Arc<AuditAccess>>,
}

/// Start Cloudflare Access sync and construct the production port adapters.
///
/// # Errors
///
/// Returns an error when Access sync is misconfigured or the audit-ledger key is malformed.
pub fn port_services(
    config: &AppConfig,
    pool: DbPool,
    key_store: KeyStore,
    stores: &IntegrationStores,
    ids: Arc<dyn IdSource>,
    audit_key: [u8; 32],
) -> Result<PortServices, AppError> {
    let discussions: Arc<dyn DiscussionService> = Arc::new(ProductionDiscussions::new(
        stores.discussions.as_ref().clone(),
        stores.organization_discord.clone(),
        stores.inbound.clone(),
        Arc::clone(&stores.webhooks),
        config.public_base_url.clone(),
        audit_key,
    ));
    let access_sync = AccessSync::start(config.access_sync.clone(), pool.clone())?;
    let admin: Arc<dyn AdminService> = Arc::new(ProductionAdmin::new(
        access_sync,
        key_store,
        OrgStore::new(pool.clone()),
        stores.webhooks.as_ref().clone(),
        audit_key,
    ));
    let engagement: Arc<dyn EngagementService> = Arc::new(ProductionEngagement::new(
        pool.clone(),
        Arc::clone(&ids),
        config.storage.feedback_max_body,
        config.public_base_url.clone(),
        DeliveryPlanningContext::production(),
    ));
    let shares: Arc<dyn ShareService> = Arc::new(ProductionShares::new(
        pool.clone(),
        ids,
        Arc::new(SystemClock),
        audit_key,
    ));
    let health: Arc<dyn HealthProbe> =
        Arc::new(ProductionHealth::new(pool.clone(), config.artifact_dir()));
    let viewer_state_key = config
        .audit_ledger_hmac_key
        .as_ref()
        .map(|encoded| parse_hmac_key(encoded.expose()))
        .transpose()?;
    let viewer_state = Arc::new(SqliteViewerState::new(pool.clone(), viewer_state_key));
    let audit_access = config
        .audit_ledger_hmac_key
        .as_ref()
        .map(|encoded| parse_hmac_key(encoded.expose()))
        .transpose()?
        .map(|key| Arc::new(AuditAccess::new(pool, key)));
    Ok(PortServices {
        admin,
        discussions,
        engagement,
        shares,
        health,
        viewer_state,
        audit_access,
    })
}
