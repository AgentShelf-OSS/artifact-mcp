//! Public token-gated artifact delivery, independent of viewer identity.

use axum::{
    Router,
    extract::{Path, State},
    response::Response,
    routing::get,
};

use crate::{
    AppDeps,
    error::AppError,
    http::{
        artifact_response::{ArtifactResponseOptions, RawCachePolicy, artifact_response},
        routes::gallery::{found_redirect, page_error_response},
    },
    model::{ArtifactId, ShareToken},
    security::access::{AccessPolicy, AuthorizedArtifact},
};

pub(crate) fn router() -> Router<AppDeps> {
    Router::new()
        .route("/s/{token}/{*path}", get(shared_file))
        .route("/s/{token}/", get(shared_entry))
        .route("/s/{token}", get(shared_root))
}

async fn shared_artifact(deps: &AppDeps, token: &str) -> Result<AuthorizedArtifact, AppError> {
    let Some(grant) = deps.shares.resolve(&ShareToken(token.to_owned())).await? else {
        return Err(AppError::ConcealedNotFound);
    };
    let meta = deps
        .artifacts
        .find_meta(&ArtifactId(grant.artifact_id.0.clone()))
        .await?;
    AccessPolicy::authorize_share(&grant, meta)
}

async fn shared_root(State(deps): State<AppDeps>, Path(token): Path<String>) -> Response {
    match shared_root_result(&deps, &token).await {
        Ok(response) => response,
        Err(error) => page_error_response(&deps, error),
    }
}

async fn shared_root_result(deps: &AppDeps, token: &str) -> Result<Response, AppError> {
    let artifact = shared_artifact(deps, token).await?;
    if artifact.meta().is_bundle {
        return found_redirect(&format!("/s/{}/", encode_uri_component(token)), true);
    }
    let Some(file) = deps.artifacts.read_body(&artifact).await? else {
        return Err(AppError::ConcealedNotFound);
    };
    artifact_response(
        file,
        ArtifactResponseOptions {
            cache: RawCachePolicy::PublicShare,
            ..ArtifactResponseOptions::default()
        },
    )
}

async fn shared_file(
    State(deps): State<AppDeps>,
    Path((token, path)): Path<(String, String)>,
) -> Response {
    match shared_file_result(&deps, &token, &path).await {
        Ok(response) => response,
        Err(error) => page_error_response(&deps, error),
    }
}

async fn shared_entry(State(deps): State<AppDeps>, Path(token): Path<String>) -> Response {
    match shared_file_result(&deps, &token, "").await {
        Ok(response) => response,
        Err(error) => page_error_response(&deps, error),
    }
}

async fn shared_file_result(deps: &AppDeps, token: &str, path: &str) -> Result<Response, AppError> {
    let artifact = shared_artifact(deps, token).await?;
    if !artifact.meta().is_bundle {
        return Err(AppError::ConcealedNotFound);
    }
    // Axum includes the separator in a catch-all capture; Express's `req.params[0]` does not.
    let path = path.strip_prefix('/').unwrap_or(path);
    let Some(file) = deps.artifacts.read_bundle_file(&artifact, path).await? else {
        return Err(AppError::ConcealedNotFound);
    };
    artifact_response(
        file,
        ArtifactResponseOptions {
            cache: RawCachePolicy::PublicShare,
            ..ArtifactResponseOptions::default()
        },
    )
}

fn encode_uri_component(value: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut encoded = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        if byte.is_ascii_alphanumeric() || b"-_.!~*'()".contains(byte) {
            encoded.push(char::from(*byte));
        } else {
            encoded.push('%');
            encoded.push(char::from(HEX[usize::from(byte >> 4)]));
            encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
        }
    }
    encoded
}
