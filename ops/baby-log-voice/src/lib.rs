//! Fixed, authenticated voice commands for the household Baby Log.
#![forbid(unsafe_code)]

use artifact_mcp::{
    artifacts::lifecycle::ArtifactStore,
    mcp::protocol::OrderedJson,
    model::state::StateError,
    model::{EmailAddress, OrgId, Viewer},
    ports::{
        ArtifactService,
        state::{StateAudience, ViewerStateService},
    },
    security::access::{AccessPolicy, AuthorizedArtifact},
};
use axum::{
    Json, Router,
    body::to_bytes,
    extract::{Request, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use hmac::{Hmac, KeyInit as _, Mac};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::Sha256;
use std::sync::Arc;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use tower_http::timeout::TimeoutLayer;

pub const ARTIFACT_ID: &str = "7qgi2ehng52j";
pub const ORG: &str = "homelab";
const WRITER: &str = "baby-log-voice@homelab.internal";
const MAX_BODY: usize = 1024;
const AUTH_DOMAIN: &[u8] = b"baby-log-voice-bearer-v1";

#[derive(Clone)]
pub struct AppState {
    artifacts: Arc<ArtifactStore>,
    state: Arc<dyn ViewerStateService>,
    // Only a verifier is retained. Neither this struct nor its fields implement Debug.
    verifier: [u8; 32],
}

impl AppState {
    pub fn new(
        artifacts: Arc<ArtifactStore>,
        state: Arc<dyn ViewerStateService>,
        token: &[u8],
    ) -> Result<Self, LogError> {
        if !(32..=256).contains(&token.len()) || !token.iter().all(|b| b.is_ascii_graphic()) {
            return Err(LogError::Invalid);
        }
        let mut mac = Hmac::<Sha256>::new_from_slice(AUTH_DOMAIN).map_err(|_| LogError::Invalid)?;
        mac.update(token);
        Ok(Self {
            artifacts,
            state,
            verifier: mac.finalize().into_bytes().into(),
        })
    }

    async fn grant(&self) -> Result<AuthorizedArtifact, LogError> {
        let viewer = Viewer {
            email: Some(WRITER.into()),
            org: Some(OrgId::from(ORG)),
            is_admin: false,
        };
        let meta = self
            .artifacts
            .find_meta(&ARTIFACT_ID.into())
            .await
            .map_err(LogError::Storage)?;
        AccessPolicy::authorize_viewer(&viewer, meta).map_err(LogError::Storage)
    }

    fn authorized(&self, request: &Request) -> bool {
        let mut values = request.headers().get_all(header::AUTHORIZATION).iter();
        let Some(value) = values
            .next()
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
        else {
            return false;
        };
        if values.next().is_some() || value.len() > 256 {
            return false;
        }
        let Ok(mut mac) = Hmac::<Sha256>::new_from_slice(AUTH_DOMAIN) else {
            return false;
        };
        mac.update(value.as_bytes());
        mac.verify_slice(&self.verifier).is_ok()
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LogRequest {
    command: Command,
    request_id: String,
    occurred_at: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Command {
    WetDiaper,
    DirtyDiaper,
    #[serde(rename = "bottle_2oz")]
    Bottle2oz,
    #[serde(rename = "bottle_4oz")]
    Bottle4oz,
}

#[derive(Debug, thiserror::Error)]
pub enum LogError {
    #[error("invalid request or stored shard")]
    Invalid,
    #[error("request conflicts with an existing or removed entry")]
    Conflict,
    #[error("storage unavailable")]
    Storage(#[source] artifact_mcp::error::AppError),
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/log", post(log))
        .layer(TimeoutLayer::with_status_code(
            StatusCode::REQUEST_TIMEOUT,
            std::time::Duration::from_secs(8),
        ))
        .with_state(state)
}

async fn health(State(state): State<AppState>, request: Request) -> Response {
    if !state.authorized(&request) {
        return reply(StatusCode::UNAUTHORIZED, json!({"error":"unauthorized"}));
    }
    match state.grant().await {
        Ok(grant) => match state.state.list(grant, StateAudience::Org).await {
            Ok(_) => reply(StatusCode::OK, json!({"ok":true})),
            Err(error) => failure(LogError::Storage(error)),
        },
        Err(error) => failure(error),
    }
}

async fn log(State(state): State<AppState>, request: Request) -> Response {
    if !state.authorized(&request) {
        return reply(StatusCode::UNAUTHORIZED, json!({"error":"unauthorized"}));
    }
    if request
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.split(';').next().unwrap_or("").trim())
        != Some("application/json")
    {
        return reply(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            json!({"error":"json_required"}),
        );
    }
    let bytes = match to_bytes(request.into_body(), MAX_BODY).await {
        Ok(bytes) => bytes,
        Err(_) => return reply(StatusCode::PAYLOAD_TOO_LARGE, json!({"error":"body_limit"})),
    };
    let request: LogRequest = match serde_json::from_slice(&bytes) {
        Ok(r) => r,
        Err(_) => return failure(LogError::Invalid),
    };
    let event = match validate(&request, OffsetDateTime::now_utc()) {
        Ok(e) => e,
        Err(e) => return failure(e),
    };
    let grant = match state.grant().await {
        Ok(g) => g,
        Err(e) => return failure(e),
    };
    match append(state.state.as_ref(), grant, &event).await {
        Ok(duplicate) => reply(
            StatusCode::OK,
            json!({"ok":true,"duplicate":duplicate,"entry_id":event.id,"command":request.command}),
        ),
        Err(error) => failure(error),
    }
}

fn reply(status: StatusCode, value: Value) -> Response {
    (status, [(header::CACHE_CONTROL, "no-store")], Json(value)).into_response()
}

fn failure(error: LogError) -> Response {
    match error {
        LogError::Invalid => reply(
            StatusCode::BAD_REQUEST,
            json!({"error":"invalid_request_or_shard"}),
        ),
        LogError::Conflict => reply(StatusCode::CONFLICT, json!({"error":"entry_conflict"})),
        LogError::Storage(error) => {
            tracing::error!(error = %error, "Baby Log voice storage operation failed");
            reply(
                StatusCode::SERVICE_UNAVAILABLE,
                json!({"error":"storage_unavailable"}),
            )
        }
    }
}

struct Event {
    id: String,
    shard: String,
    entry: Value,
}

fn validate(request: &LogRequest, now: OffsetDateTime) -> Result<Event, LogError> {
    if !(16..=80).contains(&request.request_id.len())
        || !request
            .request_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        return Err(LogError::Invalid);
    }
    let occurred =
        OffsetDateTime::parse(&request.occurred_at, &Rfc3339).map_err(|_| LogError::Invalid)?;
    if (occurred - now).abs() > time::Duration::minutes(5) {
        return Err(LogError::Invalid);
    }
    let shard = format!(
        "baby.log.{:04}{:02}",
        occurred.year(),
        u8::from(occurred.month())
    );
    let id = format!("voice_{}", request.request_id);
    let t = i64::try_from(occurred.unix_timestamp_nanos() / 1_000_000)
        .map_err(|_| LogError::Invalid)?;
    let entry = match request.command {
        Command::WetDiaper => json!({"id":id,"t":t,"k":"nappy","wet":true,"dirty":false,"col":""}),
        Command::DirtyDiaper => {
            json!({"id":id,"t":t,"k":"nappy","wet":false,"dirty":true,"col":""})
        }
        Command::Bottle2oz => json!({"id":id,"t":t,"k":"bottle","ml":59.147,"src":"formula"}),
        Command::Bottle4oz => json!({"id":id,"t":t,"k":"bottle","ml":118.294,"src":"formula"}),
    };
    Ok(Event { id, shard, entry })
}

async fn append(
    service: &dyn ViewerStateService,
    grant: AuthorizedArtifact,
    event: &Event,
) -> Result<bool, LogError> {
    // Re-read and re-merge after each conflict. Advancing only the revision would erase mobile edits.
    for _ in 0..8 {
        let current = service
            .get(grant.clone(), event.shard.clone(), StateAudience::Org)
            .await
            .map_err(LogError::Storage)?;
        let (mut document, revision) = current
            .map(|v| (v.value.into_value(), v.revision))
            .unwrap_or((json!({"v":1,"items":[],"tomb":[]}), 0));
        let obj = document.as_object_mut().ok_or(LogError::Invalid)?;
        if obj.get("v").and_then(Value::as_f64) != Some(1.0) {
            return Err(LogError::Invalid);
        }
        let tomb = obj
            .get("tomb")
            .and_then(Value::as_array)
            .ok_or(LogError::Invalid)?;
        if !tomb.iter().all(Value::is_string) {
            return Err(LogError::Invalid);
        }
        if tomb.iter().any(|v| v.as_str() == Some(&event.id)) {
            return Err(LogError::Conflict);
        }
        let items = obj
            .get_mut("items")
            .and_then(Value::as_array_mut)
            .ok_or(LogError::Invalid)?;
        if !items
            .iter()
            .all(|item| item.get("id").and_then(Value::as_str).is_some())
        {
            return Err(LogError::Invalid);
        }
        if let Some(existing) = items
            .iter()
            .find(|v| v.get("id").and_then(Value::as_str) == Some(&event.id))
        {
            return if existing == &event.entry {
                Ok(true)
            } else {
                Err(LogError::Conflict)
            };
        }
        items.push(event.entry.clone());
        let value: OrderedJson = serde_json::from_value(document).map_err(|_| LogError::Invalid)?;
        match service
            .put(
                grant.clone(),
                event.shard.clone(),
                value,
                Some(revision),
                EmailAddress::from(WRITER),
                StateAudience::Org,
            )
            .await
        {
            Ok(_) => return Ok(false),
            Err(StateError::Conflict(_)) => continue,
            Err(StateError::App(error)) => return Err(LogError::Storage(error)),
            Err(_) => return Err(LogError::Invalid),
        }
    }
    Err(LogError::Conflict)
}

#[cfg(test)]
mod tests;
