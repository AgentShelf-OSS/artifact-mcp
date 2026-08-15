//! Owned by U01 (sol) — organization, key, category, color, and webhook contract.

use std::collections::BTreeMap;

use super::BoxFuture;
use crate::{
    error::AppError,
    model::{
        ClientId, CreateOrganization, CreatePublisherKey, CreateWebhook, CreatedPublisherKey,
        EmailAddress, KeyOwnerUpdate, OrgId, Organization, OwnerBackfillResult,
        PublisherKeySummary, UpdatePublisherKey, UpdatedPublisherKey, WebhookDelivery,
        WebhookEvent, WebhookId, WebhookSummary,
    },
    security::audit::MutationAudit,
};

pub trait AdminService: Send + Sync {
    fn list_keys(&self) -> BoxFuture<'_, Result<Vec<PublisherKeySummary>, AppError>>;
    fn create_key(
        &self,
        request: CreatePublisherKey,
        audit: MutationAudit,
    ) -> BoxFuture<'_, Result<CreatedPublisherKey, AppError>>;
    fn revoke_key<'a>(
        &'a self,
        client_id: &'a ClientId,
        audit: MutationAudit,
    ) -> BoxFuture<'a, Result<bool, AppError>>;
    fn update_key(
        &self,
        _client_id: ClientId,
        _request: UpdatePublisherKey,
        _audit: MutationAudit,
    ) -> BoxFuture<'_, Result<Option<UpdatedPublisherKey>, AppError>> {
        Box::pin(async { Err(AppError::Internal) })
    }
    fn set_key_owner(
        &self,
        _client_id: ClientId,
        _owner_email: Option<String>,
        _audit: MutationAudit,
    ) -> BoxFuture<'_, Result<Option<KeyOwnerUpdate>, AppError>> {
        Box::pin(async { Err(AppError::Internal) })
    }
    fn backfill_key_owner(
        &self,
        _client_id: ClientId,
        _owner_email: String,
        _confirm: bool,
        _audit: MutationAudit,
    ) -> BoxFuture<'_, Result<Option<OwnerBackfillResult>, AppError>> {
        Box::pin(async { Err(AppError::Internal) })
    }

    fn org_exists<'a>(&'a self, org: &'a OrgId) -> BoxFuture<'a, Result<bool, AppError>>;
    fn org_for_domain<'a>(
        &'a self,
        domain: &'a str,
    ) -> BoxFuture<'a, Result<Option<OrgId>, AppError>>;
    fn org_for_email<'a>(
        &'a self,
        email: &'a EmailAddress,
    ) -> BoxFuture<'a, Result<Option<OrgId>, AppError>>;
    fn org_names(&self) -> BoxFuture<'_, Result<Vec<OrgId>, AppError>>;
    fn list_orgs(&self) -> BoxFuture<'_, Result<Vec<Organization>, AppError>>;
    fn create_org(
        &self,
        request: CreateOrganization,
        audit: MutationAudit,
    ) -> BoxFuture<'_, Result<Organization, AppError>>;
    fn delete_org<'a>(
        &'a self,
        org: &'a OrgId,
        audit: MutationAudit,
    ) -> BoxFuture<'a, Result<bool, AppError>>;

    fn add_domain<'a>(
        &'a self,
        org: &'a OrgId,
        domain: &'a str,
        audit: MutationAudit,
    ) -> BoxFuture<'a, Result<String, AppError>>;
    fn remove_domain<'a>(
        &'a self,
        org: &'a OrgId,
        domain: &'a str,
        audit: MutationAudit,
    ) -> BoxFuture<'a, Result<bool, AppError>>;
    fn add_email_member<'a>(
        &'a self,
        org: &'a OrgId,
        email: &'a EmailAddress,
        audit: MutationAudit,
    ) -> BoxFuture<'a, Result<EmailAddress, AppError>>;
    fn remove_email_member<'a>(
        &'a self,
        org: &'a OrgId,
        email: &'a EmailAddress,
        audit: MutationAudit,
    ) -> BoxFuture<'a, Result<bool, AppError>>;

    fn categories<'a>(&'a self, org: &'a OrgId) -> BoxFuture<'a, Result<Vec<String>, AppError>>;
    fn add_category<'a>(
        &'a self,
        org: &'a OrgId,
        name: &'a str,
        audit: MutationAudit,
    ) -> BoxFuture<'a, Result<String, AppError>>;
    fn remove_category<'a>(
        &'a self,
        org: &'a OrgId,
        name: &'a str,
        audit: MutationAudit,
    ) -> BoxFuture<'a, Result<bool, AppError>>;
    fn color_map(&self) -> BoxFuture<'_, Result<BTreeMap<OrgId, Option<String>>, AppError>>;
    fn set_color<'a>(
        &'a self,
        org: &'a OrgId,
        color: Option<&'a str>,
        audit: MutationAudit,
    ) -> BoxFuture<'a, Result<Option<String>, AppError>>;

    fn list_webhooks<'a>(
        &'a self,
        org: &'a OrgId,
    ) -> BoxFuture<'a, Result<Vec<WebhookSummary>, AppError>>;
    fn create_webhook(
        &self,
        request: CreateWebhook,
        audit: MutationAudit,
    ) -> BoxFuture<'_, Result<WebhookSummary, AppError>>;
    fn remove_webhook<'a>(
        &'a self,
        org: &'a OrgId,
        id: &'a WebhookId,
        audit: MutationAudit,
    ) -> BoxFuture<'a, Result<bool, AppError>>;
    fn set_webhook_events<'a>(
        &'a self,
        org: &'a OrgId,
        id: &'a WebhookId,
        events: &'a [WebhookEvent],
        audit: MutationAudit,
    ) -> BoxFuture<'a, Result<Option<WebhookSummary>, AppError>>;
    fn webhook_delivery<'a>(
        &'a self,
        id: &'a WebhookId,
    ) -> BoxFuture<'a, Result<Option<WebhookDelivery>, AppError>>;
    /// Append either the pre-I/O request marker (`outcome = None`) or the terminal delivery
    /// result (`Some(true|false)`). Both records share the server-derived request id.
    fn audit_webhook_test<'a>(
        &'a self,
        org: &'a OrgId,
        id: &'a WebhookId,
        outcome: Option<bool>,
        audit: MutationAudit,
    ) -> BoxFuture<'a, Result<(), AppError>> {
        let _ = (org, id, outcome, audit);
        Box::pin(async { Err(AppError::Internal) })
    }
}
