//! Authenticated live-data queries and one multiplexed viewer event stream.
use crate::{AppDeps, data::RequestedSubscription, http::routes::artifact::authorize};
use axum::{
    Json, Router,
    extract::{Path, Request, State},
    http::{StatusCode, header},
    response::{
        IntoResponse, Response, Sse,
        sse::{Event, KeepAlive},
    },
    routing::{get, post},
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashSet},
    convert::Infallible,
    time::Duration,
};

pub(crate) fn router() -> Router<AppDeps> {
    Router::new()
        .route("/{id}/data", get(manifest))
        .route("/{id}/data/query", post(query))
        .route("/{id}/data/events", get(events))
}
fn failure(reason: &str) -> Response {
    let (status, reason) = match reason {
        "bad_params" => (StatusCode::BAD_REQUEST, "bad_params"),
        "not_found" => (StatusCode::NOT_FOUND, "not_found"),
        "too_large" => (StatusCode::PAYLOAD_TOO_LARGE, "too_large"),
        _ => (StatusCode::BAD_GATEWAY, "data_unavailable"),
    };
    (
        status,
        [(header::CACHE_CONTROL, "no-store")],
        Json(json!({"error":reason})),
    )
        .into_response()
}
async fn manifest(
    State(deps): State<AppDeps>,
    Path(id): Path<String>,
    request: Request,
) -> Response {
    if let Err(e) = authorize(&deps, request.headers(), &id).await {
        return e.into_response();
    }
    match deps.data.try_bindings(&id).await {
        Ok(value) => ([(header::CACHE_CONTROL, "no-store")], Json(value)).into_response(),
        Err(e) => failure(&e),
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct QueryBody {
    binding: String,
    operation: String,
    #[serde(default = "empty_params")]
    params: Value,
}
fn empty_params() -> Value {
    json!({})
}
async fn query(State(deps): State<AppDeps>, Path(id): Path<String>, request: Request) -> Response {
    if let Err(e) = authorize(&deps, request.headers(), &id).await {
        return e.into_response();
    }
    let Ok(body) = axum::body::to_bytes(request.into_body(), 64 * 1024).await else {
        return failure("too_large");
    };
    let Ok(input) = serde_json::from_slice::<QueryBody>(&body) else {
        return failure("bad_params");
    };
    match deps
        .data
        .query(&id, &input.binding, &input.operation, input.params)
        .await
    {
        Ok(data) => (
            [(header::CACHE_CONTROL, "no-store")],
            Json(json!({"data":data})),
        )
            .into_response(),
        Err(e) => failure(&e),
    }
}
fn parse_cursor(raw: &str) -> Result<BTreeMap<String, String>, String> {
    if raw.len() > 8192 {
        return Err("bad_params".into());
    }
    let bytes = URL_SAFE_NO_PAD.decode(raw).map_err(|_| "bad_params")?;
    let cursor: BTreeMap<String, String> =
        serde_json::from_slice(&bytes).map_err(|_| "bad_params")?;
    if cursor.len() > 16
        || cursor.iter().any(|(k, v)| {
            k.len() > 129
                || k.chars().any(char::is_control)
                || v.len() > 128
                || v.chars().any(char::is_control)
        })
    {
        return Err("bad_params".into());
    }
    Ok(cursor)
}
async fn events(State(deps): State<AppDeps>, Path(id): Path<String>, request: Request) -> Response {
    if let Err(e) = authorize(&deps, request.headers(), &id).await {
        return e.into_response();
    }
    let mut subscriptions = None;
    let mut raw_cursor = None;
    let mut seen = HashSet::new();
    for (k, v) in url::form_urlencoded::parse(request.uri().query().unwrap_or_default().as_bytes())
    {
        if !seen.insert(k.to_string()) {
            return failure("bad_params");
        }
        match k.as_ref() {
            "subscriptions" => subscriptions = Some(v.to_string()),
            "cursor" => raw_cursor = Some(v.to_string()),
            _ => return failure("bad_params"),
        }
    }
    let Some(raw) = subscriptions else {
        return failure("bad_params");
    };
    let Ok(topics) = serde_json::from_str::<Vec<RequestedSubscription>>(&raw) else {
        return failure("bad_params");
    };
    let raw_cursor = request
        .headers()
        .get("last-event-id")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
        .or(raw_cursor);
    let mut cursor = match raw_cursor {
        Some(c) if !c.is_empty() => match parse_cursor(&c) {
            Ok(c) => c,
            Err(e) => return failure(&e),
        },
        _ => BTreeMap::new(),
    };
    let selected: HashSet<String> = topics
        .iter()
        .map(|t| format!("{}:{}", t.binding, t.subscription))
        .collect();
    cursor.retain(|k, _| selected.contains(k));
    let stream = match deps.data.stream(&id, topics, cursor.clone()).await {
        Ok(s) => s,
        Err(e) => return failure(&e),
    };
    let events =
        futures_util::stream::unfold((stream, cursor), |(mut stream, mut cursor)| async move {
            let envelope = stream.receiver.recv().await?;
            let key = format!("{}:{}", envelope.binding, envelope.subscription);
            if envelope.event == "data:resync" {
                cursor.remove(&key);
            } else if !envelope.id.is_empty() {
                cursor.insert(key, envelope.id.clone());
            }
            let encoded = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&cursor).unwrap_or_default());
            let event = Event::default()
                .event("artifact-data")
                .id(encoded)
                .json_data(&envelope)
                .expect("data envelope JSON");
            Some((Ok::<_, Infallible>(event), (stream, cursor)))
        });
    (
        [
            (header::CACHE_CONTROL, "no-store"),
            (header::HeaderName::from_static("x-accel-buffering"), "no"),
        ],
        Sse::new(events).keep_alive(KeepAlive::new().interval(Duration::from_secs(15))),
    )
        .into_response()
}
