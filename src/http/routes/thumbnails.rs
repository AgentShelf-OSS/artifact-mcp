//! Authorization-gated, current-digest-only thumbnail delivery.

use axum::{
    Router,
    body::Body,
    extract::{Path, RawQuery, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::Response,
    routing::get,
};

use crate::{
    AppDeps,
    error::AppError,
    http::routes::{
        gallery::{page_error_response, resolve_page_artifact},
        raw::QueryValues,
    },
};

pub(crate) fn router() -> Router<AppDeps> {
    Router::new().route("/thumbnails/{id}", get(thumbnail))
}

async fn thumbnail(
    State(deps): State<AppDeps>,
    Path(id): Path<String>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    match thumbnail_result(&deps, &headers, &id, query.as_deref()).await {
        Ok(response) => response,
        Err(error) => page_error_response(&deps, error),
    }
}

async fn thumbnail_result(
    deps: &AppDeps,
    headers: &HeaderMap,
    id: &str,
    query: Option<&str>,
) -> Result<Response, AppError> {
    let (_viewer, artifact) = resolve_page_artifact(deps, headers, id).await?;
    let digest = QueryValues::parse(query).single("v").unwrap_or_default();
    let png = if digest == artifact.meta().body_sha256 {
        deps.previews.read_thumbnail(&artifact, &digest).await?
    } else {
        None
    };
    if let Some(png) = png {
        return Ok(binary_response(
            png,
            "image/png",
            "private, max-age=31536000, immutable",
        ));
    }

    let accent = deps
        .admin
        .color_map()
        .await?
        .get(&artifact.meta().org)
        .cloned()
        .flatten();
    let placeholder = deps
        .previews
        .placeholder(artifact.meta(), accent.as_deref());
    Ok(binary_response(
        placeholder,
        "image/svg+xml; charset=utf-8",
        "no-store",
    ))
}

fn binary_response(content: Vec<u8>, content_type: &'static str, cache: &'static str) -> Response {
    let mut response = Response::new(Body::from(content));
    *response.status_mut() = StatusCode::OK;
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    response.headers_mut().insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static(cache));
    response
}
