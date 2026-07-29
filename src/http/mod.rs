//! Owned by U01 (sol) — frozen route aggregation; route files are independently owned.

use axum::{Router, middleware as axum_middleware};

use crate::AppDeps;

pub mod artifact_response;
pub mod middleware;
pub mod routes;

pub(crate) fn router() -> Router<AppDeps> {
    Router::new()
        .merge(routes::health::router())
        .merge(routes::mcp::router())
        .merge(routes::admin::router())
        .merge(routes::public_share::router())
        .merge(routes::thumbnails::router())
        .merge(routes::raw::router())
        .merge(routes::feedback::router())
        .merge(routes::artifact::router())
        .merge(routes::gallery::router())
        .layer(axum_middleware::from_fn(middleware::express_etag))
}
