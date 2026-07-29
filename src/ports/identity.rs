//! Owned by U01 (sol) — publisher authentication and viewer identity contracts.

use axum::http::HeaderMap;

use super::BoxFuture;
use crate::{
    error::AppError,
    model::{PublisherIdentity, Viewer},
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
