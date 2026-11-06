//! Viewer state accepts only artifact grants produced by viewer authorization.

use super::BoxFuture;
use crate::{
    error::AppError,
    model::EmailAddress,
    model::json::OrderedJson,
    model::state::{StateError, StateKey, StateValue},
    security::{access::AuthorizedArtifact, audit::MutationAudit},
};

#[derive(Clone, Debug)]
pub enum StateAudience {
    Org,
    Viewer(EmailAddress),
}

pub trait ViewerStateService: Send + Sync {
    fn list(
        &self,
        artifact: AuthorizedArtifact,
        audience: StateAudience,
    ) -> BoxFuture<'_, Result<Vec<StateKey>, AppError>>;
    fn get(
        &self,
        artifact: AuthorizedArtifact,
        key: String,
        audience: StateAudience,
    ) -> BoxFuture<'_, Result<Option<StateValue>, AppError>>;
    fn put(
        &self,
        artifact: AuthorizedArtifact,
        key: String,
        value: OrderedJson,
        if_revision: Option<u64>,
        writer: EmailAddress,
        audience: StateAudience,
    ) -> BoxFuture<'_, Result<StateValue, StateError>>;
    fn delete(
        &self,
        artifact: AuthorizedArtifact,
        key: String,
        audience: StateAudience,
        audit: MutationAudit,
    ) -> BoxFuture<'_, Result<(), AppError>>;
}

/// Explicit unavailable adapter for unrelated route tests.
pub struct InertViewerState;

impl ViewerStateService for InertViewerState {
    fn list(
        &self,
        _: AuthorizedArtifact,
        _: StateAudience,
    ) -> BoxFuture<'_, Result<Vec<StateKey>, AppError>> {
        Box::pin(async { Err(AppError::Internal) })
    }
    fn get(
        &self,
        _: AuthorizedArtifact,
        _: String,
        _: StateAudience,
    ) -> BoxFuture<'_, Result<Option<StateValue>, AppError>> {
        Box::pin(async { Err(AppError::Internal) })
    }
    fn put(
        &self,
        _: AuthorizedArtifact,
        _: String,
        _: OrderedJson,
        _: Option<u64>,
        _: EmailAddress,
        _: StateAudience,
    ) -> BoxFuture<'_, Result<StateValue, StateError>> {
        Box::pin(async { Err(StateError::App(AppError::Internal)) })
    }
    fn delete(
        &self,
        _: AuthorizedArtifact,
        _: String,
        _: StateAudience,
        _: MutationAudit,
    ) -> BoxFuture<'_, Result<(), AppError>> {
        Box::pin(async { Err(AppError::Internal) })
    }
}
