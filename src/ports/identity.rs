//! Owned by U01 (sol) — publisher authentication and viewer identity contracts.

use axum::http::HeaderMap;

use super::BoxFuture;
use crate::{
    error::AppError,
    model::{OrgId, PublisherIdentity, Viewer},
};

pub trait PublisherAuthenticator: Send + Sync {
    fn authenticate<'a>(
        &'a self,
        headers: &'a HeaderMap,
    ) -> BoxFuture<'a, Result<PublisherIdentity, AppError>>;
}

pub trait ViewerIdentity: Send + Sync {
    fn resolve<'a>(&'a self, headers: &'a HeaderMap) -> BoxFuture<'a, Result<Viewer, AppError>>;
}

/// Resolve the organization an email address belongs to at the time of the call.
///
/// Background work has no request to authenticate, so it cannot use [`ViewerIdentity`]. It asks
/// this directory instead, and must not rely on an organization it stored earlier: a viewer can be
/// removed from an organization, or lose administrator status, without making another request.
pub trait ViewerOrgDirectory: Send + Sync {
    /// The organization for `email`. An administrator resolves to the administrator tenant.
    fn current_org<'a>(&'a self, email: &'a str) -> BoxFuture<'a, Result<OrgId, AppError>>;
}
