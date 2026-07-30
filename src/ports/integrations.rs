//! Owned by U01 (sol) — preview, notification, and health contracts.

use serde::{Deserialize, Serialize};

use super::BoxFuture;
use crate::{
    error::AppError,
    model::{
        ArtifactId, ArtifactMeta, DeliveryResult, NotificationPayload, OrgId, WebhookDelivery,
        WebhookEvent,
    },
    security::access::AuthorizedArtifact,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PreviewPriority {
    High,
    Low,
}

pub trait PreviewService: Send + Sync {
    fn enabled(&self) -> bool;
    fn read_thumbnail<'a>(
        &'a self,
        artifact: &'a AuthorizedArtifact,
        digest: &'a str,
    ) -> BoxFuture<'a, Result<Option<Vec<u8>>, AppError>>;
    /// Synchronous file read used only inside ArtifactStore's lifecycle-read critical section.
    /// The default keeps deterministic non-filesystem test doubles inert; production overrides it.
    fn read_thumbnail_sync(
        &self,
        _meta: &ArtifactMeta,
        _digest: &str,
    ) -> Result<Option<Vec<u8>>, AppError> {
        Ok(None)
    }
    fn placeholder(&self, meta: &ArtifactMeta, accent: Option<&str>) -> Vec<u8>;
    fn ensure_thumbnail<'a>(
        &'a self,
        meta: &'a ArtifactMeta,
        html: &'a str,
        priority: PreviewPriority,
    ) -> BoxFuture<'a, Result<Option<Vec<u8>>, AppError>>;
    fn remove_artifact<'a>(&'a self, id: &'a ArtifactId) -> BoxFuture<'a, Result<(), AppError>>;
}

pub trait NotificationSink: Send + Sync {
    fn emit(
        &self,
        event: WebhookEvent,
        org: OrgId,
        payload: NotificationPayload,
    ) -> BoxFuture<'_, Result<(), AppError>>;
    fn test<'a>(
        &'a self,
        webhook: &'a WebhookDelivery,
    ) -> BoxFuture<'a, Result<DeliveryResult, AppError>>;
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HealthReport {
    pub status: String,
}

impl HealthReport {
    #[must_use]
    pub fn ok() -> Self {
        Self {
            status: "ok".to_owned(),
        }
    }

    #[must_use]
    pub fn error() -> Self {
        Self {
            status: "error".to_owned(),
        }
    }
}

pub trait HealthProbe: Send + Sync {
    fn check(&self) -> BoxFuture<'_, Result<HealthReport, AppError>>;
}
