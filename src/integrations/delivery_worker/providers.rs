//! Provider ports and their Discord transports. Neither port receives a URL or secret it could log.

use std::sync::Arc;

use crate::{
    integrations::{
        discord_delivery::{
            DeliveryClassification, DiscordProviderTransport, DuplicateRisk, ProviderRequest,
            RetryReason,
        },
        discord_discussion::{DiscordDiscussionTransport, DiscussionRequest, DiscussionResult},
    },
    model::OrgId,
    ports::{BoxFuture, discussions::OrganizationDiscordCredentialService},
};

/// Provider port receives the redacted request envelope, not a URL or secret.
pub trait WorkerProvider: Send + Sync {
    fn deliver<'a>(&'a self, request: ProviderRequest) -> BoxFuture<'a, DeliveryClassification>;
}
impl WorkerProvider for DiscordProviderTransport {
    fn deliver<'a>(&'a self, request: ProviderRequest) -> BoxFuture<'a, DeliveryClassification> {
        Box::pin(self.deliver(request))
    }
}

/// Provider port for PBI-079's forum/media-thread operations.
pub trait WorkerDiscussionProvider: Send + Sync {
    fn deliver<'a>(
        &'a self,
        org: &'a OrgId,
        request: DiscussionRequest,
    ) -> BoxFuture<'a, DiscussionResult>;
}
impl WorkerDiscussionProvider for DiscordDiscussionTransport {
    fn deliver<'a>(
        &'a self,
        _org: &'a OrgId,
        request: DiscussionRequest,
    ) -> BoxFuture<'a, DiscussionResult> {
        Box::pin(self.deliver(request))
    }
}

/// Resolves the organization credential only after a tenant-bound delivery row is claimed.
/// Resolution errors occur before provider I/O and are therefore safely retryable without
/// duplicate risk. A missing token is still allowed through for legacy forum-webhook operations;
/// notification-thread operations reject it at the transport boundary.
pub struct OrganizationDiscordDiscussionProvider {
    credentials: Arc<dyn OrganizationDiscordCredentialService>,
}

impl OrganizationDiscordDiscussionProvider {
    #[must_use]
    pub fn new(credentials: Arc<dyn OrganizationDiscordCredentialService>) -> Self {
        Self { credentials }
    }
}

impl WorkerDiscussionProvider for OrganizationDiscordDiscussionProvider {
    fn deliver<'a>(
        &'a self,
        org: &'a OrgId,
        request: DiscussionRequest,
    ) -> BoxFuture<'a, DiscussionResult> {
        Box::pin(async move {
            let bot_token = match self.credentials.credential_for_provider(org).await {
                Ok(value) => value,
                Err(_) => {
                    return DiscussionResult::Retry {
                        reason: RetryReason::Network,
                        duplicate_risk: DuplicateRisk::None,
                        rate_limit: None,
                    };
                }
            };
            let transport = match DiscordDiscussionTransport::with_bot_token(bot_token) {
                Ok(value) => value,
                Err(_) => {
                    return DiscussionResult::Retry {
                        reason: RetryReason::Network,
                        duplicate_risk: DuplicateRisk::None,
                        rate_limit: None,
                    };
                }
            };
            transport.deliver(request).await
        })
    }
}
