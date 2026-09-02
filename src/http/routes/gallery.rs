//! Gallery index, notification watermark, and authorized artifact shell routes.

use axum::{
    Json, Router,
    body::Body,
    extract::{Path, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::Serialize;

use crate::{
    AppDeps,
    error::AppError,
    http::ingress::ViewerCost,
    model::{OrgArtifacts, OrgId, Reaction, ViewCounts, Viewer},
    render::view_models::{ArtifactNavigation, GalleryView, ShellView},
    security::access::{AccessPolicy, AuthorizedArtifact, resolve_for_viewer},
};

const HTML_CONTENT_TYPE: &str = "text/html; charset=utf-8";
const GALLERY_TOP_LIMIT: usize = 10;
const NOTIFICATION_LIMIT: usize = 30;

pub(crate) fn router() -> Router<AppDeps> {
    Router::new()
        .route("/", get(gallery))
        .route("/notifications/seen", post(mark_notifications_seen))
        .route("/{id}", get(shell))
}

/// Resolve identity and then use U06's single artifact composition gate.
pub(crate) async fn resolve_page_artifact(
    deps: &AppDeps,
    headers: &HeaderMap,
    id: &str,
) -> Result<(Viewer, AuthorizedArtifact), AppError> {
    let viewer = deps.viewer_identity.resolve(headers).await?;
    if !deps
        .ingress
        .allow_verified_viewer(headers, &viewer, ViewerCost::Read)
    {
        return Err(AppError::RateLimited);
    }
    let artifact = resolve_for_viewer(deps.artifacts.as_ref(), &viewer, id).await?;
    Ok((viewer, artifact))
}

/// Render the existing HTML not-found page for every page-shaped concealed response.
pub(crate) fn page_error_response(deps: &AppDeps, error: AppError) -> Response {
    if error == AppError::ConcealedNotFound {
        return match deps.pages.not_found(None) {
            Ok(body) => html_response(StatusCode::NOT_FOUND, body, false),
            Err(render_error) => render_error.into_response(),
        };
    }
    error.into_response()
}

pub(crate) fn html_response(status: StatusCode, body: String, no_store: bool) -> Response {
    let mut response = Response::new(Body::from(body));
    *response.status_mut() = status;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(HTML_CONTENT_TYPE),
    );
    if no_store {
        response
            .headers_mut()
            .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    }
    response
}

/// Express's default text redirect shape, with an optional public-share indexing guard.
pub(crate) fn found_redirect(location: &str, noindex: bool) -> Result<Response, AppError> {
    let mut response = Response::new(Body::from(format!("Found. Redirecting to {location}")));
    *response.status_mut() = StatusCode::FOUND;
    response.headers_mut().insert(
        header::LOCATION,
        HeaderValue::from_str(location).map_err(|_| AppError::Internal)?,
    );
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    response
        .headers_mut()
        .insert(header::VARY, HeaderValue::from_static("Accept"));
    if noindex {
        response
            .headers_mut()
            .insert("x-robots-tag", HeaderValue::from_static("noindex"));
    }
    Ok(response)
}

async fn gallery(State(deps): State<AppDeps>, headers: HeaderMap) -> Response {
    let viewer = match deps.viewer_identity.resolve(&headers).await {
        Ok(viewer) => viewer,
        Err(error) => return error.into_response(),
    };
    if !deps
        .ingress
        .allow_verified_viewer(&headers, &viewer, ViewerCost::Read)
    {
        return AppError::RateLimited.into_response();
    }
    match gallery_result(&deps, viewer).await {
        Ok(response) => response,
        Err(error) => {
            let mut response = error.into_response();
            response
                .headers_mut()
                .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
            response
        }
    }
}

async fn gallery_result(deps: &AppDeps, viewer: Viewer) -> Result<Response, AppError> {
    if !AccessPolicy::is_signed_in(&viewer) {
        let body = deps.pages.not_signed_in()?;
        return Ok(html_response(StatusCode::FORBIDDEN, body, true));
    }

    let sections = gallery_sections(deps, &viewer).await?;
    let mut view_counts = std::collections::BTreeMap::new();
    let mut top_viewed = std::collections::BTreeMap::new();
    for section in &sections {
        let counts = match deps.engagement.view_counts_for_org(&section.org).await {
            Ok(counts) => counts,
            Err(error) => {
                tracing::warn!(
                    org = %section.org,
                    error = %error,
                    "view analytics gallery read failed (ignored)"
                );
                continue;
            }
        };
        view_counts.extend(counts);
        if viewer.is_admin {
            match deps
                .engagement
                .top_for_org(&section.org, GALLERY_TOP_LIMIT)
                .await
            {
                Ok(top) => {
                    top_viewed.insert(section.org.clone(), top);
                }
                Err(error) => tracing::warn!(
                    org = %section.org,
                    error = %error,
                    "view analytics gallery read failed (ignored)"
                ),
            }
        }
    }

    // Deliberately not best-effort. Node builds this projection outside the analytics guard.
    let notifications = deps
        .engagement
        .recent_notifications(&viewer, NOTIFICATION_LIMIT)
        .await?;
    let unread_notifications = deps.engagement.unread_notifications(&viewer).await?;
    let reactions = deps.engagement.reactions_for_viewer(&viewer).await?;
    let sentiment = if viewer.is_admin {
        deps.engagement.sentiment().await?
    } else {
        std::collections::BTreeMap::new()
    };
    let org_colors = deps.admin.color_map().await?;
    let mut org_categories = std::collections::BTreeMap::new();
    for section in &sections {
        org_categories.insert(
            section.org.clone(),
            deps.admin.categories(&section.org).await?,
        );
    }

    let body = deps.pages.gallery(&GalleryView {
        viewer,
        sections,
        reactions,
        sentiment,
        view_counts,
        top_viewed,
        org_colors,
        org_categories,
        notifications,
        unread_notifications,
    })?;
    Ok(html_response(StatusCode::OK, body, true))
}

async fn gallery_sections(deps: &AppDeps, viewer: &Viewer) -> Result<Vec<OrgArtifacts>, AppError> {
    if viewer.is_admin {
        let grouped = deps.artifacts.list_all_grouped_by_org(true).await?;
        let mut names = Vec::<OrgId>::new();
        for name in deps.admin.org_names().await? {
            if !names.contains(&name) {
                names.push(name);
            }
        }
        for group in &grouped {
            if !names.contains(&group.org) {
                names.push(group.org.clone());
            }
        }
        return Ok(names
            .into_iter()
            .map(|org| OrgArtifacts {
                items: grouped
                    .iter()
                    .find(|group| group.org == org)
                    .map_or_else(Vec::new, |group| group.items.clone()),
                org,
            })
            .collect());
    }

    match viewer.org.as_ref().filter(|org| !org.0.is_empty()) {
        Some(org) => {
            // Keep the owner email in the server-only model and project only the rows this
            // viewer may discover: all visible work plus their own hidden uploads.
            let owner = viewer.email.as_ref().map(|email| email.0.to_lowercase());
            let items = deps
                .artifacts
                .list_org_artifacts(org, true)
                .await?
                .into_iter()
                .filter(|item| {
                    !item.hidden
                        || item.owner_email.as_ref().map(|email| email.to_lowercase()) == owner
                })
                .collect();
            Ok(vec![OrgArtifacts {
                org: org.clone(),
                items,
            }])
        }
        None => Ok(Vec::new()),
    }
}

async fn mark_notifications_seen(State(deps): State<AppDeps>, headers: HeaderMap) -> Response {
    match mark_notifications_seen_result(&deps, &headers).await {
        Ok(response) => response,
        Err(error) => error.into_response(),
    }
}

#[derive(Serialize)]
struct SeenResponse {
    ok: bool,
}

#[derive(Serialize)]
struct NotSignedInResponse<'a> {
    error: &'a str,
}

async fn mark_notifications_seen_result(
    deps: &AppDeps,
    headers: &HeaderMap,
) -> Result<Response, AppError> {
    let viewer = deps.viewer_identity.resolve(headers).await?;
    if !deps
        .ingress
        .allow_verified_viewer(headers, &viewer, ViewerCost::Mutation)
    {
        return Err(AppError::RateLimited);
    }
    let Some(email) = viewer.email.as_ref().filter(|email| !email.0.is_empty()) else {
        return Ok((
            StatusCode::FORBIDDEN,
            Json(NotSignedInResponse {
                error: "Not signed in.",
            }),
        )
            .into_response());
    };
    // Deliberately not best-effort: a failed watermark write fails the request in Node.
    deps.engagement.mark_notifications_seen(email).await?;
    Ok(Json(SeenResponse { ok: true }).into_response())
}

async fn shell(
    State(deps): State<AppDeps>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    match shell_result(&deps, &headers, &id).await {
        Ok(response) => response,
        Err(error) => page_error_response(&deps, error),
    }
}

async fn shell_result(deps: &AppDeps, headers: &HeaderMap, id: &str) -> Result<Response, AppError> {
    let (viewer, artifact) = resolve_page_artifact(deps, headers, id).await?;

    if !viewer.is_admin
        && viewer
            .email
            .as_ref()
            .is_some_and(|email| !email.0.is_empty())
        && let Err(error) = deps.engagement.record_view(&artifact, &viewer).await
    {
        tracing::warn!(
            artifact_id = %artifact.meta().id,
            error = %error,
            "view analytics record failed (ignored)"
        );
    }

    let mut view_counts = ViewCounts::default();
    let mut viewers = None;
    match deps.engagement.view_counts(&artifact).await {
        Ok(counts) => {
            view_counts = counts;
            if viewer.is_admin {
                match deps.engagement.viewers(&artifact).await {
                    Ok(rows) => viewers = Some(rows),
                    Err(error) => tracing::warn!(
                        artifact_id = %artifact.meta().id,
                        error = %error,
                        "view analytics shell read failed (ignored)"
                    ),
                }
            }
        }
        Err(error) => tracing::warn!(
            artifact_id = %artifact.meta().id,
            error = %error,
            "view analytics shell read failed (ignored)"
        ),
    }

    let ids = if viewer.is_admin {
        deps.artifacts
            .list_org_ids(&artifact.meta().org, true)
            .await?
    } else {
        let owner = viewer.email.as_ref().map(|email| email.0.to_lowercase());
        deps.artifacts
            .list_org_artifacts(&artifact.meta().org, true)
            .await?
            .into_iter()
            .filter(|item| {
                !item.hidden || item.owner_email.as_ref().map(|email| email.to_lowercase()) == owner
            })
            .map(|item| item.id)
            .collect()
    };
    let position = ids
        .iter()
        .position(|candidate| candidate == &artifact.meta().id);
    let navigation = ArtifactNavigation {
        previous_id: position
            .filter(|index| *index > 0)
            .map(|index| ids[index - 1].clone()),
        next_id: position
            .filter(|index| *index + 1 < ids.len())
            .map(|index| ids[index + 1].clone()),
        index: position.map_or(1, |index| index + 1),
        total: ids.len().max(1),
    };

    let feedback = deps.engagement.list_feedback(&artifact).await?;
    let reaction = deps.engagement.reaction(&artifact, &viewer).await?;
    let org_accent = deps
        .admin
        .color_map()
        .await?
        .get(&artifact.meta().org)
        .cloned()
        .flatten();
    let body = deps.pages.shell(&ShellView {
        artifact,
        navigation,
        reaction: Reaction {
            favorite: reaction.favorite,
            vote: reaction.vote,
        },
        feedback,
        view_counts,
        viewers,
        viewer,
        org_accent,
    })?;
    Ok(html_response(StatusCode::OK, body, true))
}
