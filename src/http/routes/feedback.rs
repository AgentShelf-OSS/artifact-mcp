//! Viewer feedback routes.

use axum::{
    Json, Router,
    extract::Request,
    extract::{Path, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{delete, get, post},
};
use serde::Serialize;

use super::artifact::{
    authorize, javascript_or_empty, javascript_string, javascript_truthy, parse_json_request,
};
use crate::{
    AppDeps,
    error::AppError,
    http::artifact_response::is_html_content_type,
    mcp::protocol::OrderedJson,
    model::{
        ArtifactId, Feedback, FeedbackAnchor, FeedbackAnchorV2, FeedbackAuthor, FeedbackId, OrgId,
        SubmitFeedback, Timestamp,
    },
    persistence::feedback::{
        ANCHOR_NOT_OBJECT_MESSAGE, ANCHOR_PAGE_MISSING_MESSAGE, ANCHOR_PAGE_NOT_BUNDLE_MESSAGE,
        ANCHOR_PAGE_REQUIRED_MESSAGE, ANCHOR_PAGE_UNANCHORED_MESSAGE, ANCHOR_POINT_MESSAGE,
        validate_anchor_page,
    },
};

pub(crate) fn router() -> Router<AppDeps> {
    Router::new()
        .route("/{id}/feedback", get(list_feedback).post(submit_feedback))
        .route("/{id}/feedback/{fid}", delete(delete_feedback))
        .route("/{id}/feedback/{fid}/resolve", post(resolve_feedback))
}

#[derive(Serialize)]
struct FeedbackResponse {
    id: FeedbackId,
    artifact_id: ArtifactId,
    org: OrgId,
    viewer_email: Option<crate::model::EmailAddress>,
    author: FeedbackAuthor,
    body: String,
    artifact_revision: u64,
    created_at: Timestamp,
    resolved_at: Option<Timestamp>,
    resolved_by: Option<String>,
    parent_id: Option<FeedbackId>,
    anchor_path: Option<String>,
    anchor_x: Option<f64>,
    anchor_y: Option<f64>,
    anchor_w: Option<f64>,
    anchor_h: Option<f64>,
    anchor_approx: i64,
    anchor_page: Option<String>,
    anchor_kind: Option<String>,
    anchor_node_id: Option<String>,
    anchor_quote: Option<String>,
    anchor_version: u8,
}

impl From<Feedback> for FeedbackResponse {
    fn from(feedback: Feedback) -> Self {
        Self {
            id: feedback.id,
            artifact_id: feedback.artifact_id,
            org: feedback.org,
            viewer_email: feedback.viewer_email,
            author: feedback.author,
            body: feedback.body,
            artifact_revision: feedback.artifact_revision,
            created_at: feedback.created_at,
            resolved_at: feedback.resolved_at,
            resolved_by: feedback.resolved_by,
            parent_id: feedback.parent_id,
            anchor_path: feedback.anchor_path,
            anchor_x: feedback.anchor_x,
            anchor_y: feedback.anchor_y,
            anchor_w: feedback.anchor_w,
            anchor_h: feedback.anchor_h,
            anchor_approx: i64::from(feedback.anchor_approx),
            anchor_page: feedback.anchor_page,
            anchor_kind: feedback.anchor_kind,
            anchor_node_id: feedback.anchor_node_id,
            anchor_quote: feedback.anchor_quote,
            anchor_version: feedback.anchor_version,
        }
    }
}

async fn list_feedback(
    State(deps): State<AppDeps>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let (artifact, _viewer) = match authorize(&deps, &headers, &id).await {
        Ok(allowed) => allowed,
        Err(error) => return no_store(error.into_response()),
    };
    match deps.engagement.list_feedback(&artifact).await {
        Ok(feedback) => no_store(
            Json(
                feedback
                    .into_iter()
                    .map(FeedbackResponse::from)
                    .collect::<Vec<_>>(),
            )
            .into_response(),
        ),
        Err(error) => no_store(error.into_response()),
    }
}

#[derive(Serialize)]
struct CreatedFeedbackResponse {
    id: FeedbackId,
    artifact_id: ArtifactId,
    viewer_email: Option<crate::model::EmailAddress>,
    author: FeedbackAuthor,
    body: String,
    parent_id: Option<FeedbackId>,
    anchor_path: Option<String>,
    anchor_x: Option<f64>,
    anchor_y: Option<f64>,
    anchor_w: Option<f64>,
    anchor_h: Option<f64>,
    anchor_approx: i64,
    anchor_page: Option<String>,
    anchor_kind: Option<String>,
    anchor_node_id: Option<String>,
    anchor_quote: Option<String>,
    anchor_version: u8,
    artifact_revision: u64,
    created_at: Timestamp,
}

impl From<Feedback> for CreatedFeedbackResponse {
    fn from(feedback: Feedback) -> Self {
        Self {
            id: feedback.id,
            artifact_id: feedback.artifact_id,
            viewer_email: feedback.viewer_email,
            author: feedback.author,
            body: feedback.body,
            parent_id: feedback.parent_id,
            anchor_path: feedback.anchor_path,
            anchor_x: feedback.anchor_x,
            anchor_y: feedback.anchor_y,
            anchor_w: feedback.anchor_w,
            anchor_h: feedback.anchor_h,
            anchor_approx: i64::from(feedback.anchor_approx),
            anchor_page: feedback.anchor_page,
            anchor_kind: feedback.anchor_kind,
            anchor_node_id: feedback.anchor_node_id,
            anchor_quote: feedback.anchor_quote,
            anchor_version: feedback.anchor_version,
            artifact_revision: feedback.artifact_revision,
            created_at: feedback.created_at,
        }
    }
}

async fn submit_feedback(
    State(deps): State<AppDeps>,
    Path(id): Path<String>,
    request: Request,
) -> Response {
    let (headers, body) = match parse_json_request(
        request,
        deps.config.body.feedback_json,
        &deps.config.ingress,
    )
    .await
    {
        Ok(parsed) => parsed,
        Err(response) => return response,
    };
    let (artifact, viewer) = match authorize(&deps, &headers, &id).await {
        Ok(allowed) => allowed,
        Err(error) => return no_store(error.into_response()),
    };
    if let Err(error) = validate_feedback_object(&body) {
        return no_store(error.into_response());
    }
    let parsed_anchor = parse_anchor(body.get("anchor"));
    let meta = artifact.meta().clone();
    let anchor_page_value =
        match anchor_page_value(&meta, parsed_anchor.value.as_ref(), body.get("anchor_page")) {
            Ok(value) => value,
            Err(error) => return no_store(error.into_response()),
        };
    let anchor_page = match validate_anchor_page(
        meta.is_bundle,
        parsed_anchor.value.as_ref(),
        anchor_page_value,
        &|_| true,
    ) {
        Ok(value) => value,
        Err(error) => return no_store(error.into_response()),
    };
    if let Some(page) = anchor_page.as_deref() {
        let file = match deps.artifacts.read_bundle_file(&artifact, page).await {
            Ok(file) => file,
            Err(error) => return no_store(error.into_response()),
        };
        if !file
            .as_ref()
            .is_some_and(|file| is_html_content_type(&file.content_type))
        {
            return no_store(
                AppError::Validation(ANCHOR_PAGE_MISSING_MESSAGE.to_owned()).into_response(),
            );
        }
    }

    let viewer_email = viewer.email.clone().unwrap_or_default();
    let submission = SubmitFeedback {
        viewer_email: viewer_email.clone(),
        body: javascript_or_empty(body.get("body")),
        parent_id: feedback_id(body.get("parent_id")),
        anchor: parsed_anchor.value,
        anchor_path: anchor_path(body.get("anchor")),
        anchor_page,
        anchor_v2: parsed_anchor.v2,
    };
    let created = match deps.engagement.submit_feedback(artifact, submission).await {
        Ok(created) => created,
        Err(AppError::Validation(reason))
            if parsed_anchor.not_object && reason == ANCHOR_POINT_MESSAGE =>
        {
            return no_store(
                AppError::Validation(ANCHOR_NOT_OBJECT_MESSAGE.to_owned()).into_response(),
            );
        }
        Err(error) => return no_store(error.into_response()),
    };
    deps.delivery_wake.wake();
    no_store(
        (
            StatusCode::CREATED,
            Json(CreatedFeedbackResponse::from(created)),
        )
            .into_response(),
    )
}

fn validate_feedback_object(body: &OrderedJson) -> Result<(), AppError> {
    if body.as_object().is_none() {
        return Err(AppError::Validation(
            "Feedback body must be a JSON object.".to_owned(),
        ));
    }
    if let Some((unknown, _)) = body
        .object_entries()
        .into_iter()
        .find(|(key, _)| !matches!(*key, "body" | "parent_id" | "anchor" | "anchor_page"))
    {
        return Err(AppError::Validation(format!(
            "Unknown feedback field: {unknown}"
        )));
    }
    Ok(())
}

struct ParsedAnchor {
    value: Option<FeedbackAnchor>,
    v2: Option<FeedbackAnchorV2>,
    not_object: bool,
}

fn parse_anchor(value: Option<&OrderedJson>) -> ParsedAnchor {
    let Some(value) = value.filter(|value| !matches!(value, OrderedJson::Null)) else {
        return ParsedAnchor {
            value: None,
            v2: None,
            not_object: false,
        };
    };
    if value.as_object().is_none() {
        // The frozen `SubmitFeedback` cannot carry a non-object anchor. A NaN point makes U11
        // fail at the same late validation position (after body and parent checks); the route
        // remaps that one sentinel error back to Node's exact object-type message.
        return ParsedAnchor {
            value: Some(FeedbackAnchor {
                x: f64::NAN,
                y: f64::NAN,
                w: None,
                h: None,
                approx: false,
            }),
            v2: None,
            not_object: true,
        };
    };
    let v2 = anchor_v2(value);
    ParsedAnchor {
        value: Some(FeedbackAnchor {
            x: anchor_number(value.get("x")),
            y: anchor_number(value.get("y")),
            w: optional_anchor_number(value.get("w")),
            h: optional_anchor_number(value.get("h")),
            approx: value.get("approx").is_some_and(javascript_truthy),
        }),
        v2,
        not_object: false,
    }
}

fn anchor_v2(value: &OrderedJson) -> Option<FeedbackAnchorV2> {
    let v2_keys = ["version", "kind", "nodeId", "quote"];
    if !value
        .object_entries()
        .into_iter()
        .any(|(key, _)| v2_keys.iter().any(|v2_key| *v2_key == key))
    {
        return None;
    }
    let node = value.get("nodeId");
    let quote = value.get("quote");
    let approx = value.get("approx");
    Some(FeedbackAnchorV2 {
        version: value
            .get("version")
            .and_then(OrderedJson::as_number)
            .and_then(serde_json::Number::as_f64),
        kind: value
            .get("kind")
            .and_then(OrderedJson::as_str)
            .map(ToOwned::to_owned),
        node_id: node.and_then(OrderedJson::as_str).map(ToOwned::to_owned),
        quote: quote.and_then(OrderedJson::as_str).map(ToOwned::to_owned),
        path_is_string: value.get("path").and_then(OrderedJson::as_str).is_some(),
        node_id_is_string_or_null: matches!(
            node,
            None | Some(OrderedJson::Null | OrderedJson::String(_))
        ),
        quote_is_string_or_null: matches!(
            quote,
            None | Some(OrderedJson::Null | OrderedJson::String(_))
        ),
        approx_is_boolean_or_absent: matches!(approx, None | Some(OrderedJson::Bool(_))),
    })
}

fn anchor_number(value: Option<&OrderedJson>) -> f64 {
    value
        .and_then(OrderedJson::as_number)
        .and_then(serde_json::Number::as_f64)
        .unwrap_or(f64::NAN)
}

fn optional_anchor_number(value: Option<&OrderedJson>) -> Option<f64> {
    value
        .filter(|value| !matches!(value, OrderedJson::Null))
        .map(|value| anchor_number(Some(value)))
}

fn anchor_path(anchor: Option<&OrderedJson>) -> Option<String> {
    anchor
        .and_then(OrderedJson::as_object)
        .and_then(|_| anchor.and_then(|anchor| anchor.get("path")))
        .filter(|value| !matches!(value, OrderedJson::Null))
        .map(javascript_string)
}

fn feedback_id(value: Option<&OrderedJson>) -> Option<FeedbackId> {
    match value {
        None | Some(OrderedJson::Null) => None,
        Some(OrderedJson::String(value)) if value.is_empty() => None,
        Some(value) => Some(FeedbackId(javascript_string(value))),
    }
}

fn anchor_page_value<'a>(
    meta: &crate::model::ArtifactMeta,
    anchor: Option<&FeedbackAnchor>,
    value: Option<&'a OrderedJson>,
) -> Result<Option<&'a str>, AppError> {
    match value {
        None | Some(OrderedJson::Null) => Ok(None),
        Some(OrderedJson::String(value)) => Ok(Some(value)),
        Some(_) if anchor.is_none() => Err(AppError::Validation(
            ANCHOR_PAGE_UNANCHORED_MESSAGE.to_owned(),
        )),
        Some(_) if !meta.is_bundle => Err(AppError::Validation(
            ANCHOR_PAGE_NOT_BUNDLE_MESSAGE.to_owned(),
        )),
        Some(_) => Err(AppError::Validation(
            ANCHOR_PAGE_REQUIRED_MESSAGE.to_owned(),
        )),
    }
}

#[derive(Serialize)]
struct DeleteFeedbackResponse {
    id: String,
    deleted: bool,
}

async fn delete_feedback(
    State(deps): State<AppDeps>,
    Path((id, fid)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let (artifact, viewer) = match authorize(&deps, &headers, &id).await {
        Ok(allowed) => allowed,
        Err(error) => return no_store(error.into_response()),
    };
    match deps
        .engagement
        .delete_feedback(artifact, viewer, FeedbackId(fid.clone()))
        .await
    {
        Ok(_) => no_store(
            Json(DeleteFeedbackResponse {
                id: fid,
                deleted: true,
            })
            .into_response(),
        ),
        Err(error) => no_store(error.into_response()),
    }
}

#[derive(Serialize)]
struct ResolveFeedbackResponse {
    id: String,
    resolved: bool,
}

async fn resolve_feedback(
    State(deps): State<AppDeps>,
    Path((id, fid)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let (artifact, viewer) = match authorize(&deps, &headers, &id).await {
        Ok(allowed) => allowed,
        Err(error) => return no_store(error.into_response()),
    };
    let mutation = match deps
        .engagement
        .resolve_feedback_as_viewer(artifact, viewer, FeedbackId(fid.clone()))
        .await
    {
        Ok(mutation) => mutation,
        Err(error) => return no_store(error.into_response()),
    };
    if mutation.changed {
        deps.delivery_wake.wake();
    }
    no_store(
        Json(ResolveFeedbackResponse {
            id: fid,
            resolved: true,
        })
        .into_response(),
    )
}

fn no_store(mut response: Response) -> Response {
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}
