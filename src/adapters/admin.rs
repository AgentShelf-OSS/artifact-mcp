//! Administrator service over the key, organization, and webhook stores.

use std::collections::BTreeMap;

use crate::{
    error::AppError,
    integrations::access_sync::AccessSync,
    model::{
        ClientId, CreateOrganization, CreatePublisherKey, CreateWebhook, CreatedPublisherKey,
        EmailAddress, OrgId, Organization, PublisherKeySummary, WebhookDelivery, WebhookEvent,
        WebhookId, WebhookSummary,
    },
    persistence::{keys::KeyStore, orgs::OrgStore, webhooks::WebhookStore},
    ports::{AdminService, BoxFuture},
    security::audit::MutationAudit,
};

#[derive(Clone)]
pub struct ProductionAdmin {
    access_sync: AccessSync,
    keys: KeyStore,
    orgs: OrgStore,
    webhooks: WebhookStore,
    audit_key: [u8; 32],
}

impl ProductionAdmin {
    pub fn new(
        access_sync: AccessSync,
        keys: KeyStore,
        orgs: OrgStore,
        webhooks: WebhookStore,
        audit_key: [u8; 32],
    ) -> Self {
        Self {
            access_sync,
            keys,
            orgs,
            webhooks,
            audit_key,
        }
    }
}

impl AdminService for ProductionAdmin {
    fn access_sync_status(&self) -> crate::integrations::access_sync::SyncStatus {
        self.access_sync.status()
    }
    fn list_keys(&self) -> BoxFuture<'_, Result<Vec<PublisherKeySummary>, AppError>> {
        Box::pin(self.keys.list_keys())
    }

    fn create_key(
        &self,
        request: CreatePublisherKey,
        audit: MutationAudit,
    ) -> BoxFuture<'_, Result<CreatedPublisherKey, AppError>> {
        Box::pin(self.keys.create_key_audited(request, audit, self.audit_key))
    }

    fn revoke_key<'a>(
        &'a self,
        client_id: &'a ClientId,
        audit: MutationAudit,
    ) -> BoxFuture<'a, Result<bool, AppError>> {
        Box::pin(
            self.keys
                .revoke_key_audited(client_id.clone(), audit, self.audit_key),
        )
    }

    fn update_key(
        &self,
        client_id: ClientId,
        request: crate::model::UpdatePublisherKey,
        audit: MutationAudit,
    ) -> BoxFuture<'_, Result<Option<crate::model::UpdatedPublisherKey>, AppError>> {
        Box::pin(
            self.keys
                .update_key_audited(client_id, request, audit, self.audit_key),
        )
    }

    fn set_key_owner(
        &self,
        client_id: ClientId,
        owner_email: Option<String>,
        audit: MutationAudit,
    ) -> BoxFuture<'_, Result<Option<crate::model::KeyOwnerUpdate>, AppError>> {
        Box::pin(
            self.keys
                .set_key_owner_audited(client_id, owner_email, audit, self.audit_key),
        )
    }

    fn backfill_key_owner(
        &self,
        client_id: ClientId,
        owner_email: String,
        confirm: bool,
        audit: MutationAudit,
    ) -> BoxFuture<'_, Result<Option<crate::model::OwnerBackfillResult>, AppError>> {
        // A preview is intentionally not ledgered: it changes no state and must not become a
        // target-existence oracle. Confirmed backfills are atomically ledgered.
        if confirm {
            Box::pin(self.keys.backfill_key_owner_audited(
                client_id,
                owner_email,
                true,
                audit,
                self.audit_key,
            ))
        } else {
            Box::pin(self.keys.backfill_key_owner(client_id, owner_email, false))
        }
    }

    fn org_exists<'a>(&'a self, org: &'a OrgId) -> BoxFuture<'a, Result<bool, AppError>> {
        Box::pin(self.orgs.org_exists(org))
    }

    fn org_for_domain<'a>(
        &'a self,
        domain: &'a str,
    ) -> BoxFuture<'a, Result<Option<OrgId>, AppError>> {
        Box::pin(self.orgs.org_for_domain(domain))
    }

    fn org_for_email<'a>(
        &'a self,
        email: &'a EmailAddress,
    ) -> BoxFuture<'a, Result<Option<OrgId>, AppError>> {
        Box::pin(self.orgs.org_for_email(email))
    }

    fn org_names(&self) -> BoxFuture<'_, Result<Vec<OrgId>, AppError>> {
        Box::pin(self.orgs.org_names())
    }

    fn list_orgs(&self) -> BoxFuture<'_, Result<Vec<Organization>, AppError>> {
        Box::pin(self.orgs.list_orgs())
    }

    fn create_org(
        &self,
        request: CreateOrganization,
        audit: MutationAudit,
    ) -> BoxFuture<'_, Result<Organization, AppError>> {
        Box::pin(self.orgs.create_org_audited(request, audit, self.audit_key))
    }

    fn delete_org<'a>(
        &'a self,
        org: &'a OrgId,
        audit: MutationAudit,
    ) -> BoxFuture<'a, Result<bool, AppError>> {
        Box::pin(async move {
            let result = self
                .orgs
                .delete_org_audited(org.clone(), audit, self.audit_key)
                .await?;
            self.access_sync.wake();
            Ok(result)
        })
    }

    fn add_domain<'a>(
        &'a self,
        org: &'a OrgId,
        domain: &'a str,
        audit: MutationAudit,
    ) -> BoxFuture<'a, Result<String, AppError>> {
        Box::pin(self.orgs.add_domain_audited(
            org.clone(),
            domain.to_owned(),
            audit,
            self.audit_key,
        ))
    }

    fn remove_domain<'a>(
        &'a self,
        org: &'a OrgId,
        domain: &'a str,
        audit: MutationAudit,
    ) -> BoxFuture<'a, Result<bool, AppError>> {
        Box::pin(self.orgs.remove_domain_audited(
            org.clone(),
            domain.to_owned(),
            audit,
            self.audit_key,
        ))
    }

    fn add_email_member<'a>(
        &'a self,
        org: &'a OrgId,
        email: &'a EmailAddress,
        audit: MutationAudit,
    ) -> BoxFuture<'a, Result<EmailAddress, AppError>> {
        Box::pin(async move {
            let result = self
                .orgs
                .add_email_member_audited(org.clone(), email.clone(), audit, self.audit_key)
                .await?;
            self.access_sync.wake();
            Ok(result)
        })
    }

    fn add_email_member_with_name<'a>(
        &'a self,
        org: &'a OrgId,
        email: &'a EmailAddress,
        display_name: Option<String>,
        audit: MutationAudit,
    ) -> BoxFuture<'a, Result<EmailAddress, AppError>> {
        Box::pin(async move {
            let result = self
                .orgs
                .add_email_member_with_name_audited(
                    org.clone(),
                    email.clone(),
                    display_name,
                    audit,
                    self.audit_key,
                )
                .await?;
            self.access_sync.wake();
            Ok(result)
        })
    }

    fn email_display_name<'a>(
        &'a self,
        email: &'a EmailAddress,
    ) -> BoxFuture<'a, Result<Option<String>, AppError>> {
        Box::pin(self.orgs.email_display_name(email))
    }

    fn remove_email_member<'a>(
        &'a self,
        org: &'a OrgId,
        email: &'a EmailAddress,
        audit: MutationAudit,
    ) -> BoxFuture<'a, Result<bool, AppError>> {
        Box::pin(async move {
            let result = self
                .orgs
                .remove_email_member_audited(org.clone(), email.clone(), audit, self.audit_key)
                .await?;
            self.access_sync.wake();
            Ok(result)
        })
    }

    fn categories<'a>(&'a self, org: &'a OrgId) -> BoxFuture<'a, Result<Vec<String>, AppError>> {
        Box::pin(self.orgs.categories(org))
    }

    fn add_category<'a>(
        &'a self,
        org: &'a OrgId,
        name: &'a str,
        audit: MutationAudit,
    ) -> BoxFuture<'a, Result<String, AppError>> {
        Box::pin(self.orgs.add_category_audited(
            org.clone(),
            name.to_owned(),
            audit,
            self.audit_key,
        ))
    }

    fn remove_category<'a>(
        &'a self,
        org: &'a OrgId,
        name: &'a str,
        audit: MutationAudit,
    ) -> BoxFuture<'a, Result<bool, AppError>> {
        Box::pin(self.orgs.remove_category_audited(
            org.clone(),
            name.to_owned(),
            audit,
            self.audit_key,
        ))
    }

    fn color_map(&self) -> BoxFuture<'_, Result<BTreeMap<OrgId, Option<String>>, AppError>> {
        Box::pin(self.orgs.color_map())
    }

    fn set_color<'a>(
        &'a self,
        org: &'a OrgId,
        color: Option<&'a str>,
        audit: MutationAudit,
    ) -> BoxFuture<'a, Result<Option<String>, AppError>> {
        Box::pin(self.orgs.set_color_audited(
            org.clone(),
            color.map(str::to_owned),
            audit,
            self.audit_key,
        ))
    }

    fn list_webhooks<'a>(
        &'a self,
        org: &'a OrgId,
    ) -> BoxFuture<'a, Result<Vec<WebhookSummary>, AppError>> {
        Box::pin(self.webhooks.list_for_org(org))
    }

    fn create_webhook(
        &self,
        request: CreateWebhook,
        audit: MutationAudit,
    ) -> BoxFuture<'_, Result<WebhookSummary, AppError>> {
        Box::pin(self.webhooks.create_audited(request, audit, self.audit_key))
    }

    fn remove_webhook<'a>(
        &'a self,
        org: &'a OrgId,
        id: &'a WebhookId,
        audit: MutationAudit,
    ) -> BoxFuture<'a, Result<bool, AppError>> {
        Box::pin(
            self.webhooks
                .remove_audited(org.clone(), id.clone(), audit, self.audit_key),
        )
    }

    fn set_webhook_events<'a>(
        &'a self,
        org: &'a OrgId,
        id: &'a WebhookId,
        events: &'a [WebhookEvent],
        audit: MutationAudit,
    ) -> BoxFuture<'a, Result<Option<WebhookSummary>, AppError>> {
        Box::pin(self.webhooks.set_events_audited(
            org.clone(),
            id.clone(),
            events.to_vec(),
            audit,
            self.audit_key,
        ))
    }

    fn webhook_delivery<'a>(
        &'a self,
        id: &'a WebhookId,
    ) -> BoxFuture<'a, Result<Option<WebhookDelivery>, AppError>> {
        Box::pin(self.webhooks.delivery(id))
    }

    fn audit_webhook_test<'a>(
        &'a self,
        org: &'a OrgId,
        id: &'a WebhookId,
        outcome: Option<bool>,
        audit: MutationAudit,
    ) -> BoxFuture<'a, Result<(), AppError>> {
        Box::pin(
            self.webhooks
                .audit_test(org.clone(), id.clone(), outcome, audit, self.audit_key),
        )
    }
}
