//! Identity stage: MCP publisher authentication and viewer identity.

use std::sync::Arc;

use crate::{
    adapters::ProductionDirectories,
    config::{AccessIdentityMode, AppConfig},
    error::AppError,
    persistence::db::DbPool,
    ports::{PublisherAuthenticator, ViewerIdentity, ViewerOrgDirectory},
    security::{
        auth::KeyAuthenticator,
        identity::AccessViewerIdentity,
        jwks::{CachingJwks, JwkDocument, JwksProvider, StaticJwks},
        oauth::{CompositePublisherAuthenticator, OAuthAuthenticator},
    },
};

/// The MCP publisher authenticator and the viewer identity resolver.
pub struct Authentication {
    pub publisher_auth: Arc<dyn PublisherAuthenticator>,
    pub viewer_identity: Arc<dyn ViewerIdentity>,
    /// The same resolver, for background work that has an email address and no request.
    pub viewer_orgs: Arc<dyn ViewerOrgDirectory>,
}

/// Build the MCP publisher authenticator and the viewer identity resolver.
///
/// # Errors
///
/// Returns an error when a configured JWKS endpoint is unusable.
pub fn authentication(config: &Arc<AppConfig>, pool: &DbPool) -> Result<Authentication, AppError> {
    let directories = Arc::new(ProductionDirectories::new(pool.clone()));
    let api_keys = config
        .oauth
        .api_keys_enabled
        .then(|| KeyAuthenticator::new(directories.clone()));
    let oauth = configured_oauth_jwks(config)?
        .map(|jwks| OAuthAuthenticator::new(config.oauth.clone(), jwks));
    let publisher_auth: Arc<dyn PublisherAuthenticator> =
        Arc::new(CompositePublisherAuthenticator::new(api_keys, oauth));
    tracing::info!(
        oauth_enabled = config.oauth.enabled(),
        api_keys_enabled = config.oauth.api_keys_enabled,
        "MCP publisher authentication ready"
    );
    let access_identity = Arc::new(AccessViewerIdentity::new(
        Arc::clone(config),
        configured_jwks(config)?,
        directories,
    ));
    let viewer_identity: Arc<dyn ViewerIdentity> = access_identity.clone();
    let viewer_orgs: Arc<dyn ViewerOrgDirectory> = access_identity;
    tracing::info!(
        identity_mode = %config.access.identity_mode(),
        "viewer identity mode ready"
    );
    Ok(Authentication {
        publisher_auth,
        viewer_identity,
        viewer_orgs,
    })
}

fn configured_jwks(config: &AppConfig) -> Result<Arc<dyn JwksProvider>, AppError> {
    match config.access.identity_mode() {
        AccessIdentityMode::Jwt => {
            let url = config.access.jwks_url().ok_or_else(|| {
                AppError::Validation("Cloudflare Access JWKS URL is unavailable".to_owned())
            })?;
            Ok(Arc::new(CachingJwks::remote(url)?))
        }
        AccessIdentityMode::HeaderTrust | AccessIdentityMode::Disabled => {
            Ok(Arc::new(StaticJwks::new(JwkDocument::default())))
        }
    }
}

fn configured_oauth_jwks(config: &AppConfig) -> Result<Option<Arc<dyn JwksProvider>>, AppError> {
    if config.oauth.enabled() {
        Ok(Some(Arc::new(CachingJwks::remote(
            config.oauth.jwks_url.clone(),
        )?)))
    } else {
        Ok(None)
    }
}
