//! Server-derived mutation attribution: who acted, from which transport, under which
//! request correlation.

use std::future::Future;

use crate::error::AppError;
use crate::model::{OrgId, PublisherIdentity, Viewer};

use super::encoding::id;
use super::schema::AuditContext;

tokio::task_local! {
    // The transport observation id is generated before parsing/authentication. It is not a
    // client field and survives every tool call made while the MCP request is in flight.
    static MCP_REQUEST_ID: String;
}

/// Scope work performed for one already-observed MCP request.  The task-local is deliberately
/// set only by the authenticated transport, never by an MCP argument.
pub async fn with_mcp_request_id<T>(request_id: String, future: impl Future<Output = T>) -> T {
    MCP_REQUEST_ID.scope(request_id, future).await
}

/// Opaque, server-derived mutation attribution. Routes may construct this only after their
/// authentication middleware has verified an identity; lifecycle code chooses the operation,
/// target and durability correlation and never accepts them from an HTTP/MCP payload.
#[derive(Clone, Debug)]
pub struct MutationAudit {
    context: AuditContext,
    request_id: String,
}

/// Private-to-the-server browser request correlation. It is deliberately distinct from the
/// outward `x-request-id`, which tower-http permits a client to supply.
#[derive(Clone, Debug)]
pub struct AuditRequestId(String);

impl AuditRequestId {
    pub(crate) fn generate() -> Result<Self, AppError> {
        MutationAudit::request_id("browser").map(Self)
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

impl MutationAudit {
    fn request_id(source: &str) -> Result<String, AppError> {
        let mut random = [0_u8; 16];
        getrandom::fill(&mut random).map_err(|_| AppError::Internal)?;
        Ok(format!("{source}-{}", hex::encode(random)))
    }

    pub fn publisher(publisher: &PublisherIdentity) -> Result<Self, AppError> {
        let request_id = match MCP_REQUEST_ID.try_with(Clone::clone) {
            Ok(request_id) => request_id,
            Err(_) => Self::request_id("mcp")?,
        };
        Ok(Self {
            context: AuditContext {
                tenant: publisher.org.0.clone(),
                actor_type: "api_key".to_owned(),
                actor_id: publisher.client_id.0.clone(),
                actor_role: publisher.role.clone(),
                source: "mcp".to_owned(),
                request_id: request_id.clone(),
            },
            request_id,
        })
    }

    pub fn viewer(viewer: &Viewer) -> Result<Self, AppError> {
        Self::viewer_with_request_id(viewer, None)
    }

    /// Browser route attribution with the server-generated `RequestId` when the caller is in
    /// the normal HTTP stack. Route-only tests without that middleware obtain a CSPRNG id.
    pub fn viewer_with_request_id(
        viewer: &Viewer,
        request_id: Option<&AuditRequestId>,
    ) -> Result<Self, AppError> {
        let email = viewer.email.as_ref().ok_or_else(|| {
            AppError::Validation("verified viewer identity required for audit".to_owned())
        })?;
        let org = viewer.org.as_ref().ok_or_else(|| {
            AppError::Validation("verified viewer organization required for audit".to_owned())
        })?;
        let request_id = request_id
            .map(|value| value.as_str().to_owned())
            .unwrap_or(Self::request_id("browser")?);
        Ok(Self {
            context: AuditContext {
                tenant: org.0.clone(),
                actor_type: "viewer".to_owned(),
                actor_id: email.0.clone(),
                actor_role: if viewer.is_admin { "admin" } else { "member" }.to_owned(),
                source: "browser".to_owned(),
                request_id: request_id.clone(),
            },
            request_id,
        })
    }

    pub fn recovery() -> Result<Self, AppError> {
        let request_id = Self::request_id("reconciliation")?;
        Ok(Self {
            context: AuditContext {
                tenant: "system".to_owned(),
                actor_type: "system".to_owned(),
                actor_id: "artifact-mcp".to_owned(),
                actor_role: String::new(),
                source: "reconciliation".to_owned(),
                request_id: request_id.clone(),
            },
            request_id,
        })
    }

    pub fn maintenance() -> Result<Self, AppError> {
        let request_id = Self::request_id("maintenance")?;
        Ok(Self {
            context: AuditContext {
                tenant: "system".to_owned(),
                actor_type: "system".to_owned(),
                actor_id: "artifact-mcp".to_owned(),
                actor_role: String::new(),
                source: "maintenance".to_owned(),
                request_id: request_id.clone(),
            },
            request_id,
        })
    }

    /// Scope a verified browser administrator's action to the organization it mutates.  This is
    /// server-only: callers cannot choose an actor or elevate an ordinary member into another
    /// tenant's ledger stream.
    pub fn admin_for_tenant(&self, tenant: &str) -> Result<Self, AppError> {
        if self.context.actor_type != "viewer" || self.context.actor_role != "admin" {
            return Err(AppError::Forbidden(
                "administrator audit context required".to_owned(),
            ));
        }
        let tenant = id(tenant, true)?;
        let mut context = self.context.clone();
        context.tenant = tenant;
        Ok(Self {
            context,
            request_id: self.request_id.clone(),
        })
    }

    /// Keep an actor in its own tenant, or require verified browser-admin authority for a
    /// cross-tenant target. Persistence uses this rather than falling back to a home tenant.
    pub fn for_target_tenant(&self, tenant: &str) -> Result<Self, AppError> {
        if self.context.tenant == tenant {
            return Ok(self.clone());
        }
        self.admin_for_tenant(tenant)
    }

    #[must_use]
    pub fn context(&self) -> &AuditContext {
        &self.context
    }

    #[must_use]
    pub fn correlation(&self, operation: &str, target_id: &str, revision: Option<u64>) -> String {
        format!(
            "audit:{operation}:{target_id}:{}:{}",
            revision.map_or_else(|| "new".to_owned(), |value| value.to_string()),
            self.request_id
        )
    }

    pub fn event_id(&self) -> Result<String, AppError> {
        // A request can complete more than one independent mutation.  The terminal record ID
        // must therefore be unique per receipt, while the request id remains the correlation.
        Self::request_id("audit")
    }

    /// Re-scope an already-verified administrator action to the tenant of the record being
    /// changed.  The tenant comes from persisted metadata, never a browser/MCP argument, so a
    /// global administrator's home organization cannot misfile a cross-organization audit row.
    pub(crate) fn for_affected_tenant(&self, tenant: &OrgId) -> Self {
        let mut context = self.context.clone();
        context.tenant.clone_from(&tenant.0);
        Self {
            context,
            request_id: self.request_id.clone(),
        }
    }
}
