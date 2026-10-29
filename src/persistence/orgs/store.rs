use std::collections::BTreeMap;

use rusqlite::{Connection, OptionalExtension as _, TransactionBehavior};

use super::mutations::{create_org_in_transaction_from_request, delete_org_in_transaction};
use super::semantics::{database_failure, norm_category, norm_org};
use super::{
    add_category, add_domain, add_email_member, add_email_member_with_name, categories, color_map,
    create_org, delete_org, email_display_name, list_orgs, org_exists, org_for_domain,
    org_for_email, org_names, remove_category, remove_domain, remove_email_member, set_color,
};
use crate::error::AppError;
use crate::model::{CreateOrganization, EmailAddress, OrgId, Organization};
use crate::persistence::db::{self, DbPool};
use crate::security::audit::{AuditEvent, MutationAudit, mutate_in_transaction};

// ---------------------------------------------------------------------------
// Pooled adapter
// ---------------------------------------------------------------------------

/// The `AdminService` organization surface backed by U03's pool.
///
/// `AdminService` is one frozen trait spanning keys, orgs, *and* webhooks (U12), so it cannot be
/// implemented piecewise. This store carries the org half under the frozen method names and
/// signatures; the composed adapter that implements the trait belongs to the unit that owns the
/// last piece.
#[derive(Clone)]
pub struct OrgStore {
    pool: DbPool,
}

impl OrgStore {
    /// Wrap a pool.
    #[must_use]
    pub const fn new(pool: DbPool) -> Self {
        Self { pool }
    }

    /// See [`org_exists`].
    ///
    /// # Errors
    /// Propagates the synchronous operation's error.
    pub async fn org_exists(&self, org: &OrgId) -> Result<bool, AppError> {
        let org = org.0.clone();
        db::interact(&self.pool, move |conn| org_exists(conn, &org)).await
    }

    /// See [`org_for_domain`].
    ///
    /// # Errors
    /// Propagates the synchronous operation's error.
    pub async fn org_for_domain(&self, domain: &str) -> Result<Option<OrgId>, AppError> {
        let domain = domain.to_owned();
        db::interact(&self.pool, move |conn| org_for_domain(conn, &domain)).await
    }

    /// See [`org_for_email`].
    ///
    /// # Errors
    /// Propagates the synchronous operation's error.
    pub async fn org_for_email(&self, email: &EmailAddress) -> Result<Option<OrgId>, AppError> {
        let email = email.0.clone();
        db::interact(&self.pool, move |conn| org_for_email(conn, &email)).await
    }

    /// See [`org_names`].
    ///
    /// # Errors
    /// Propagates the synchronous operation's error.
    pub async fn org_names(&self) -> Result<Vec<OrgId>, AppError> {
        db::interact(&self.pool, |conn| org_names(conn)).await
    }

    /// See [`list_orgs`].
    ///
    /// # Errors
    /// Propagates the synchronous operation's error.
    pub async fn list_orgs(&self) -> Result<Vec<Organization>, AppError> {
        db::interact(&self.pool, |conn| list_orgs(conn)).await
    }

    /// See [`create_org`].
    ///
    /// # Errors
    /// Propagates the synchronous operation's error.
    pub async fn create_org(&self, request: CreateOrganization) -> Result<Organization, AppError> {
        db::interact(&self.pool, move |conn| create_org(conn, &request)).await
    }

    pub async fn create_org_audited(
        &self,
        request: CreateOrganization,
        audit: MutationAudit,
        audit_key: [u8; 32],
    ) -> Result<Organization, AppError> {
        db::interact(&self.pool, move |conn| {
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(|_| AppError::Internal)?;
            let created = create_org_in_transaction_from_request(&tx, &request)?;
            let audit = audit.for_target_tenant(&created.name.0)?;
            let event = AuditEvent {
                operation: "org.create".to_owned(),
                target_type: "organization".to_owned(),
                target_id: created.name.0.clone(),
                result: "success".to_owned(),
                classification: "organization_created".to_owned(),
                revision: None,
            };
            crate::security::audit::append_in_transaction(
                &tx,
                &audit_key,
                &audit.event_id()?,
                audit.context(),
                &event,
            )?;
            tx.commit().map_err(|_| AppError::Internal)?;
            Ok(created)
        })
        .await
    }

    /// See [`delete_org`].
    ///
    /// # Errors
    /// Propagates the synchronous operation's error.
    pub async fn delete_org(&self, org: &OrgId) -> Result<bool, AppError> {
        let org = org.0.clone();
        db::interact(&self.pool, move |conn| delete_org(conn, &org)).await
    }

    pub async fn delete_org_audited(
        &self,
        org: OrgId,
        audit: MutationAudit,
        audit_key: [u8; 32],
    ) -> Result<bool, AppError> {
        db::interact(&self.pool, move |conn| {
            let target = norm_org(&org.0);
            let audit = audit.for_target_tenant(&target)?;
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(|_| AppError::Internal)?;
            if !delete_org_in_transaction(&tx, &target)? {
                tx.commit().map_err(|_| AppError::Internal)?;
                return Ok(false);
            }
            let event = AuditEvent {
                operation: "org.delete".to_owned(),
                target_type: "organization".to_owned(),
                target_id: target,
                result: "success".to_owned(),
                classification: "organization_deleted".to_owned(),
                revision: None,
            };
            crate::security::audit::append_in_transaction(
                &tx,
                &audit_key,
                &audit.event_id()?,
                audit.context(),
                &event,
            )?;
            tx.commit().map_err(|_| AppError::Internal)?;
            Ok(true)
        })
        .await
    }

    /// See [`add_domain`].
    ///
    /// # Errors
    /// Propagates the synchronous operation's error.
    pub async fn add_domain(&self, org: &OrgId, domain: &str) -> Result<String, AppError> {
        let (org, domain) = (org.0.clone(), domain.to_owned());
        db::interact(&self.pool, move |conn| add_domain(conn, &org, &domain)).await
    }

    pub async fn add_domain_audited(
        &self,
        org: OrgId,
        domain: String,
        audit: MutationAudit,
        audit_key: [u8; 32],
    ) -> Result<String, AppError> {
        self.audited_org_value(
            org,
            domain,
            audit,
            audit_key,
            "org.domain.add",
            "domain_added",
            add_domain,
        )
        .await
    }

    /// See [`remove_domain`].
    ///
    /// # Errors
    /// Propagates the synchronous operation's error.
    pub async fn remove_domain(&self, org: &OrgId, domain: &str) -> Result<bool, AppError> {
        let (org, domain) = (org.0.clone(), domain.to_owned());
        db::interact(&self.pool, move |conn| remove_domain(conn, &org, &domain)).await
    }

    pub async fn remove_domain_audited(
        &self,
        org: OrgId,
        domain: String,
        audit: MutationAudit,
        audit_key: [u8; 32],
    ) -> Result<bool, AppError> {
        self.audited_org_bool(
            org,
            domain,
            audit,
            audit_key,
            "org.domain.remove",
            "domain_removed",
            remove_domain,
        )
        .await
    }

    /// See [`add_email_member`].
    ///
    /// # Errors
    /// Propagates the synchronous operation's error.
    pub async fn add_email_member(
        &self,
        org: &OrgId,
        email: &EmailAddress,
    ) -> Result<EmailAddress, AppError> {
        let (org, email) = (org.0.clone(), email.0.clone());
        db::interact(&self.pool, move |conn| {
            add_email_member(conn, &org, &email).map(EmailAddress)
        })
        .await
    }

    pub async fn add_email_member_audited(
        &self,
        org: OrgId,
        email: EmailAddress,
        audit: MutationAudit,
        audit_key: [u8; 32],
    ) -> Result<EmailAddress, AppError> {
        let value = self
            .audited_org_value(
                org,
                email.0,
                audit,
                audit_key,
                "org.member.add",
                "member_added",
                add_email_member,
            )
            .await?;
        Ok(EmailAddress(value))
    }

    pub async fn add_email_member_with_name_audited(
        &self,
        org: OrgId,
        email: EmailAddress,
        display_name: Option<String>,
        audit: MutationAudit,
        audit_key: [u8; 32],
    ) -> Result<EmailAddress, AppError> {
        let value = self
            .audited_org_value(
                org,
                email.0,
                audit,
                audit_key,
                "org.member.add",
                "member_added",
                move |conn, org, email| {
                    add_email_member_with_name(conn, org, email, display_name.as_deref())
                },
            )
            .await?;
        Ok(EmailAddress(value))
    }

    pub async fn email_display_name(
        &self,
        email: &EmailAddress,
    ) -> Result<Option<String>, AppError> {
        let email = email.0.clone();
        db::interact(&self.pool, move |conn| email_display_name(conn, &email)).await
    }

    /// See [`remove_email_member`].
    ///
    /// # Errors
    /// Propagates the synchronous operation's error.
    pub async fn remove_email_member(
        &self,
        org: &OrgId,
        email: &EmailAddress,
    ) -> Result<bool, AppError> {
        let (org, email) = (org.0.clone(), email.0.clone());
        db::interact(&self.pool, move |conn| {
            remove_email_member(conn, &org, &email)
        })
        .await
    }

    pub async fn remove_email_member_audited(
        &self,
        org: OrgId,
        email: EmailAddress,
        audit: MutationAudit,
        audit_key: [u8; 32],
    ) -> Result<bool, AppError> {
        self.audited_org_bool(
            org,
            email.0,
            audit,
            audit_key,
            "org.member.remove",
            "member_removed",
            remove_email_member,
        )
        .await
    }

    /// See [`categories`].
    ///
    /// # Errors
    /// Propagates the synchronous operation's error.
    pub async fn categories(&self, org: &OrgId) -> Result<Vec<String>, AppError> {
        let org = org.0.clone();
        db::interact(&self.pool, move |conn| categories(conn, &org)).await
    }

    /// See [`add_category`].
    ///
    /// # Errors
    /// Propagates the synchronous operation's error.
    pub async fn add_category(&self, org: &OrgId, name: &str) -> Result<String, AppError> {
        let (org, name) = (org.0.clone(), name.to_owned());
        db::interact(&self.pool, move |conn| add_category(conn, &org, &name)).await
    }

    pub async fn add_category_audited(
        &self,
        org: OrgId,
        name: String,
        audit: MutationAudit,
        audit_key: [u8; 32],
    ) -> Result<String, AppError> {
        let target = org.0.clone();
        let audit = audit.for_target_tenant(&target)?;
        let pool = self.pool.clone();
        db::interact(&pool, move |conn| {
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(|_| AppError::Internal)?;
            let normalized = norm_category(&name);
            let existed = tx
                .query_row(
                    "SELECT 1 FROM org_categories WHERE org=?1 AND name=?2",
                    (&org.0, &normalized),
                    |_| Ok(()),
                )
                .optional()
                .map_err(|error| database_failure("read category", &error))?
                .is_some();
            let result = add_category(&tx, &org.0, &name)?;
            if existed {
                tx.commit().map_err(|_| AppError::Internal)?;
                return Ok(result);
            }
            let event = AuditEvent {
                operation: "org.category.add".to_owned(),
                target_type: "organization".to_owned(),
                target_id: target,
                result: "success".to_owned(),
                classification: "category_added".to_owned(),
                revision: None,
            };
            crate::security::audit::append_in_transaction(
                &tx,
                &audit_key,
                &audit.event_id()?,
                audit.context(),
                &event,
            )?;
            tx.commit().map_err(|_| AppError::Internal)?;
            Ok(result)
        })
        .await
    }

    /// See [`remove_category`].
    ///
    /// # Errors
    /// Propagates the synchronous operation's error.
    pub async fn remove_category(&self, org: &OrgId, name: &str) -> Result<bool, AppError> {
        let (org, name) = (org.0.clone(), name.to_owned());
        db::interact(&self.pool, move |conn| remove_category(conn, &org, &name)).await
    }

    pub async fn remove_category_audited(
        &self,
        org: OrgId,
        name: String,
        audit: MutationAudit,
        audit_key: [u8; 32],
    ) -> Result<bool, AppError> {
        self.audited_org_bool(
            org,
            name,
            audit,
            audit_key,
            "org.category.remove",
            "category_removed",
            remove_category,
        )
        .await
    }

    /// See [`color_map`].
    ///
    /// # Errors
    /// Propagates the synchronous operation's error.
    pub async fn color_map(&self) -> Result<BTreeMap<OrgId, Option<String>>, AppError> {
        db::interact(&self.pool, |conn| color_map(conn)).await
    }

    /// See [`set_color`].
    ///
    /// # Errors
    /// Propagates the synchronous operation's error.
    pub async fn set_color(
        &self,
        org: &OrgId,
        color: Option<&str>,
    ) -> Result<Option<String>, AppError> {
        let (org, color) = (org.0.clone(), color.map(str::to_owned));
        db::interact(&self.pool, move |conn| {
            set_color(conn, &org, color.as_deref())
        })
        .await
    }

    pub async fn set_color_audited(
        &self,
        org: OrgId,
        color: Option<String>,
        audit: MutationAudit,
        audit_key: [u8; 32],
    ) -> Result<Option<String>, AppError> {
        let target = org.0.clone();
        let audit = audit.for_target_tenant(&target)?;
        db::interact(&self.pool, move |conn| {
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(|_| AppError::Internal)?;
            let current = tx
                .query_row("SELECT color FROM orgs WHERE name=?1", [&org.0], |row| {
                    row.get::<_, Option<String>>(0)
                })
                .optional()
                .map_err(|error| database_failure("read org color", &error))?;
            let stored = set_color(&tx, &org.0, color.as_deref())?;
            if current.as_ref() == Some(&stored) {
                tx.commit().map_err(|_| AppError::Internal)?;
                return Ok(stored);
            }
            let event = AuditEvent {
                operation: "org.color.set".to_owned(),
                target_type: "organization".to_owned(),
                target_id: target,
                result: "success".to_owned(),
                classification: "color_updated".to_owned(),
                revision: None,
            };
            crate::security::audit::append_in_transaction(
                &tx,
                &audit_key,
                &audit.event_id()?,
                audit.context(),
                &event,
            )?;
            tx.commit().map_err(|_| AppError::Internal)?;
            Ok(stored)
        })
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn audited_org_value(
        &self,
        org: OrgId,
        value: String,
        audit: MutationAudit,
        audit_key: [u8; 32],
        operation: &'static str,
        classification: &'static str,
        mutation: impl FnOnce(&Connection, &str, &str) -> Result<String, AppError> + Send + 'static,
    ) -> Result<String, AppError> {
        let target = org.0.clone();
        let audit = audit.for_target_tenant(&target)?;
        db::interact(&self.pool, move |conn| {
            mutate_in_transaction(conn, &audit_key, &audit, |tx| {
                let result = mutation(tx, &org.0, &value)?;
                Ok((
                    result,
                    AuditEvent {
                        operation: operation.to_owned(),
                        target_type: "organization".to_owned(),
                        target_id: target,
                        result: "success".to_owned(),
                        classification: classification.to_owned(),
                        revision: None,
                    },
                ))
            })
        })
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn audited_org_bool(
        &self,
        org: OrgId,
        value: String,
        audit: MutationAudit,
        audit_key: [u8; 32],
        operation: &'static str,
        classification: &'static str,
        mutation: fn(&Connection, &str, &str) -> Result<bool, AppError>,
    ) -> Result<bool, AppError> {
        let target = org.0.clone();
        let audit = audit.for_target_tenant(&target)?;
        db::interact(&self.pool, move |conn| {
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(|_| AppError::Internal)?;
            if !mutation(&tx, &org.0, &value)? {
                tx.commit().map_err(|_| AppError::Internal)?;
                return Ok(false);
            }
            let event = AuditEvent {
                operation: operation.to_owned(),
                target_type: "organization".to_owned(),
                target_id: target,
                result: "success".to_owned(),
                classification: classification.to_owned(),
                revision: None,
            };
            crate::security::audit::append_in_transaction(
                &tx,
                &audit_key,
                &audit.event_id()?,
                audit.context(),
                &event,
            )?;
            tx.commit().map_err(|_| AppError::Internal)?;
            Ok(true)
        })
        .await
    }
}
