//! Optional, authenticated speech synthesis for selected artifacts.

use std::{collections::BTreeSet, sync::OnceLock, time::Duration};

use axum::{
    Json, Router,
    body::Body,
    extract::{Path, Request, State},
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::Serialize;
use tokio::sync::{Semaphore, mpsc};
use tokio::task::JoinHandle;

use crate::{
    AppDeps,
    error::AppError,
    http::routes::artifact::{authorize, parse_json_request},
    mcp::protocol::OrderedJson,
};

const MAX_TEXT_CHARS: usize = 1_500;
const MAX_INSTRUCTIONS_CHARS: usize = 500;
const MAX_JSON_BYTES: usize = 8_192;
const MAX_AUDIO_BYTES: u64 = 4 * 1024 * 1024;
const MAX_STREAM_BYTES: u64 = 4_100_000;
const WORKER_TIMEOUT: Duration = Duration::from_secs(60);
const SOUNDTOUCH_PROCESSOR: &str = include_str!("../../../assets/vendor/soundtouch-processor.js");
const SOUNDTOUCH_SOURCE_MAP: &str =
    include_str!("../../../assets/vendor/soundtouch-processor.js.map");
const SOUNDTOUCH_LICENSE: &str = include_str!("../../../assets/vendor/soundtouch-LICENSE.txt");
const READER_PITCH_WORKLET: &str = include_str!("../../../assets/reader-pitch-worklet.js");

const VOICES: &[(&str, &str)] = &[
    ("bm_george", "George · Kokoro (British)"),
    ("bf_emma", "Emma · Kokoro (British)"),
    ("af_heart", "Heart · Kokoro (American)"),
];
const POCKET_VOICES: &[(&str, &str)] = &[
    ("pocket_alba", "Alba · Pocket"),
    ("pocket_marius", "Marius · Pocket"),
    ("pocket_javert", "Javert · Pocket"),
    ("pocket_jean", "Jean · Pocket"),
    ("pocket_cosette", "Cosette · Pocket"),
    ("pocket_eponine", "Eponine · Pocket"),
    ("pocket_fantine", "Fantine · Pocket"),
    ("pocket_azelma", "Azelma · Pocket"),
    ("pocket_anna", "Anna · Pocket"),
    ("pocket_bill_boerst", "Bill Boerst · Pocket"),
    ("pocket_caro_davy", "Caro Davy · Pocket"),
    ("pocket_charles", "Charles · Pocket"),
    ("pocket_eve", "Eve · Pocket"),
    ("pocket_george", "George · Pocket"),
    ("pocket_jane", "Jane · Pocket"),
    ("pocket_mary", "Mary · Pocket"),
    ("pocket_michael", "Michael · Pocket"),
    ("pocket_paul", "Paul · Pocket"),
    ("pocket_peter_yearsley", "Peter Yearsley · Pocket"),
    ("pocket_stuart_bell", "Stuart Bell · Pocket"),
    ("pocket_vera", "Vera · Pocket"),
];
const RAVEN_VOICES: &[(&str, &str)] = &[
    ("raven_alba", "Alba · RAVEN (trial)"),
    ("raven_marius", "Marius · RAVEN (trial)"),
];
const QWEN_VOICES: &[(&str, &str)] = &[
    ("qwen_reference", "Reference voice · Qwen"),
    ("qwen_ryan", "Ryan · Qwen"),
    ("qwen_aiden", "Aiden · Qwen"),
];
const MOSS_VOICES: &[(&str, &str)] = &[
    ("moss_trump", "Trump · MOSS-TTS Nano"),
    ("moss_ava", "Ava · MOSS-TTS Nano"),
    ("moss_bella", "Bella · MOSS-TTS Nano"),
    ("moss_adam", "Adam · MOSS-TTS Nano"),
    ("moss_nathan", "Nathan · MOSS-TTS Nano"),
];

struct TtsClient {
    endpoint: String,
    stream_endpoint: String,
    timed_stream_endpoint: String,
    token: String,
    http: reqwest::Client,
    permits: Semaphore,
}

static CLIENT: OnceLock<Option<TtsClient>> = OnceLock::new();
static POCKET_CLIENT: OnceLock<Option<TtsClient>> = OnceLock::new();
static RAVEN_CLIENT: OnceLock<Option<TtsClient>> = OnceLock::new();
static QWEN_CLIENT: OnceLock<Option<TtsClient>> = OnceLock::new();
static MOSS_CLIENT: OnceLock<Option<TtsClient>> = OnceLock::new();
static ARTIFACTS: OnceLock<BTreeSet<String>> = OnceLock::new();

fn client() -> Option<&'static TtsClient> {
    CLIENT
        .get_or_init(|| {
            let raw_endpoint = std::env::var("TTS_WORKER_URL")
                .ok()?
                .trim_end_matches('/')
                .to_owned();
            build_client(&raw_endpoint, std::env::var("TTS_WORKER_TOKEN_FILE").ok())
        })
        .as_ref()
}

fn pocket_client() -> Option<&'static TtsClient> {
    POCKET_CLIENT
        .get_or_init(|| {
            let raw_endpoint = std::env::var("POCKET_TTS_WORKER_URL").ok()?;
            let token_path = std::env::var("POCKET_TTS_WORKER_TOKEN_FILE")
                .ok()
                .or_else(|| std::env::var("TTS_WORKER_TOKEN_FILE").ok());
            build_client(raw_endpoint.trim_end_matches('/'), token_path)
        })
        .as_ref()
}

fn qwen_client() -> Option<&'static TtsClient> {
    QWEN_CLIENT
        .get_or_init(|| {
            let raw_endpoint = std::env::var("QWEN_TTS_WORKER_URL").ok()?;
            let token_path = std::env::var("QWEN_TTS_WORKER_TOKEN_FILE")
                .ok()
                .or_else(|| std::env::var("TTS_WORKER_TOKEN_FILE").ok());
            build_client(raw_endpoint.trim_end_matches('/'), token_path)
        })
        .as_ref()
}

fn raven_client() -> Option<&'static TtsClient> {
    RAVEN_CLIENT
        .get_or_init(|| {
            let raw_endpoint = std::env::var("RAVEN_TTS_WORKER_URL").ok()?;
            let token_path = std::env::var("RAVEN_TTS_WORKER_TOKEN_FILE")
                .ok()
                .or_else(|| std::env::var("TTS_WORKER_TOKEN_FILE").ok());
            build_client(raw_endpoint.trim_end_matches('/'), token_path)
        })
        .as_ref()
}

fn moss_client() -> Option<&'static TtsClient> {
    MOSS_CLIENT
        .get_or_init(|| {
            let raw_endpoint = std::env::var("MOSS_TTS_WORKER_URL").ok()?;
            let token_path = std::env::var("MOSS_TTS_WORKER_TOKEN_FILE")
                .ok()
                .or_else(|| std::env::var("TTS_WORKER_TOKEN_FILE").ok());
            build_client(raw_endpoint.trim_end_matches('/'), token_path)
        })
        .as_ref()
}

fn build_client(raw_endpoint: &str, token_path: Option<String>) -> Option<TtsClient> {
    let base = url::Url::parse(&format!("{raw_endpoint}/")).ok()?;
    if !matches!(base.scheme(), "http" | "https") || base.host_str().is_none() {
        return None;
    }
    let endpoint = base.join("speech").ok()?.to_string();
    let stream_endpoint = base.join("speech/stream").ok()?.to_string();
    let timed_stream_endpoint = base.join("speech/stream-timed").ok()?.to_string();
    let token = std::fs::read_to_string(token_path?).ok()?.trim().to_owned();
    if token.is_empty() {
        return None;
    }
    let http = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(WORKER_TIMEOUT)
        .build()
        .ok()?;
    Some(TtsClient {
        endpoint,
        stream_endpoint,
        timed_stream_endpoint,
        token,
        http,
        permits: Semaphore::new(1),
    })
}

fn voices_for_request() -> Vec<Voice> {
    let mut voices = Vec::new();
    if client().is_some() {
        voices.extend(VOICES.iter().map(|&(id, name)| Voice {
            id,
            name,
            streaming: false,
        }));
    }
    if pocket_client().is_some() {
        voices.extend(POCKET_VOICES.iter().map(|&(id, name)| Voice {
            id,
            name,
            streaming: true,
        }));
    }
    if raven_client().is_some() {
        voices.extend(RAVEN_VOICES.iter().map(|&(id, name)| Voice {
            id,
            name,
            streaming: false,
        }));
    }
    if qwen_client().is_some() {
        voices.extend(
            QWEN_VOICES
                .iter()
                .filter(|(id, _)| qwen_voice_enabled(id))
                .map(|&(id, name)| Voice {
                    id,
                    name,
                    streaming: true,
                }),
        );
    }
    if moss_client().is_some() {
        voices.extend(MOSS_VOICES.iter().map(|&(id, name)| Voice {
            id,
            name,
            streaming: false,
        }));
    }
    voices
}

fn enabled_for(id: &str) -> bool {
    let ids = ARTIFACTS.get_or_init(|| {
        std::env::var("TTS_ARTIFACT_IDS")
            .unwrap_or_default()
            .split(',')
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(ToOwned::to_owned)
            .collect()
    });
    let globally_enabled = std::env::var("TTS_ENABLED")
        .ok()
        .is_some_and(|value| value.trim() == "1");
    speech_enabled_for(id, globally_enabled, ids)
}

fn speech_enabled_for(id: &str, globally_enabled: bool, ids: &BTreeSet<String>) -> bool {
    (globally_enabled || !ids.is_empty()) && (ids.is_empty() || ids.contains(id))
}

pub(crate) fn router() -> Router<AppDeps> {
    Router::new()
        .route(
            "/reader-audio/soundtouch-2.1.1.js",
            get(soundtouch_processor),
        )
        .route(
            "/reader-audio/soundtouch-2.1.1.js.map",
            get(soundtouch_source_map),
        )
        .route(
            "/reader-audio/soundtouch-processor.js.map",
            get(soundtouch_source_map),
        )
        .route("/reader-audio/LICENSE", get(soundtouch_license))
        .route("/reader-audio/pitch-v1.js", get(reader_pitch_worklet))
        .route("/{id}/speech", post(synthesize))
        .route("/{id}/speech/stream", post(stream_synthesize))
        .route("/{id}/speech/stream-timed", post(stream_timed_synthesize))
        .route("/{id}/speech/voices", get(voices))
}

fn static_audio_asset(
    body: impl Into<Body>,
    content_type: &'static str,
    cache_control: &'static str,
) -> Response {
    let body = body.into();
    (
        [
            (header::CONTENT_TYPE, HeaderValue::from_static(content_type)),
            (
                header::CACHE_CONTROL,
                HeaderValue::from_static(cache_control),
            ),
            (
                header::X_CONTENT_TYPE_OPTIONS,
                HeaderValue::from_static("nosniff"),
            ),
        ],
        body,
    )
        .into_response()
}

async fn soundtouch_processor() -> Response {
    static_audio_asset(
        SOUNDTOUCH_PROCESSOR,
        "application/javascript; charset=utf-8",
        "public, max-age=3600",
    )
}

async fn soundtouch_source_map() -> Response {
    static_audio_asset(
        SOUNDTOUCH_SOURCE_MAP,
        "application/json; charset=utf-8",
        "public, max-age=3600",
    )
}

async fn soundtouch_license() -> Response {
    static_audio_asset(
        SOUNDTOUCH_LICENSE,
        "text/plain; charset=utf-8",
        "public, max-age=3600",
    )
}

async fn reader_pitch_worklet() -> Response {
    static_audio_asset(
        format!("{SOUNDTOUCH_PROCESSOR}\n{READER_PITCH_WORKLET}"),
        "application/javascript; charset=utf-8",
        "no-cache",
    )
}

#[derive(Serialize)]
struct Voice {
    id: &'static str,
    name: &'static str,
    #[serde(skip_serializing_if = "is_false")]
    streaming: bool,
}

fn is_false(value: &bool) -> bool {
    !*value
}

#[derive(Serialize)]
struct VoicesResponse {
    enabled: bool,
    voices: Vec<Voice>,
    #[serde(rename = "maxChars")]
    max_chars: usize,
}

async fn voices(
    State(deps): State<AppDeps>,
    Path(id): Path<String>,
    headers: axum::http::HeaderMap,
) -> Response {
    if let Err(error) = authorize(&deps, &headers, &id).await {
        return error.into_response();
    }
    let available_voices = if enabled_for(&id) {
        voices_for_request()
    } else {
        Vec::new()
    };
    let enabled = !available_voices.is_empty();
    Json(VoicesResponse {
        enabled,
        voices: available_voices,
        max_chars: MAX_TEXT_CHARS,
    })
    .into_response()
}

async fn synthesize(
    State(deps): State<AppDeps>,
    Path(id): Path<String>,
    request: Request,
) -> Response {
    let (_artifact, _) = match authorize(&deps, request.headers(), &id).await {
        Ok(value) => value,
        Err(error) => return error.into_response(),
    };
    if !enabled_for(&id)
        || (client().is_none()
            && pocket_client().is_none()
            && raven_client().is_none()
            && qwen_client().is_none()
            && moss_client().is_none())
    {
        return AppError::Unavailable("speech unavailable".to_owned()).into_response();
    }
    let (_, body) =
        match parse_json_request(request, MAX_JSON_BYTES as u64, &deps.config.ingress).await {
            Ok(value) => value,
            Err(response) => return response,
        };
    let (text, voice, instructions) = match validate_speech_body(&body) {
        Ok(value) => value,
        Err(error) => return bad_speech(error),
    };
    let Some(tts) = client_for_voice(voice) else {
        return AppError::Unavailable("speech unavailable".to_owned()).into_response();
    };
    let permit = match tts.permits.try_acquire() {
        Ok(permit) => permit,
        Err(_) => {
            return (
                StatusCode::TOO_MANY_REQUESTS,
                [(header::RETRY_AFTER, "2")],
                Json(serde_json::json!({"error":"busy"})),
            )
                .into_response();
        }
    };
    let mut payload = serde_json::json!({"text":text,"voice":voice});
    if let Some(instructions) = instructions {
        payload["instructions"] = serde_json::Value::String(instructions);
    }
    let response = tts
        .http
        .post(&tts.endpoint)
        .bearer_auth(&tts.token)
        .json(&payload)
        .send()
        .await;
    let mut response = match response {
        Ok(response) => response,
        Err(_) => return AppError::Unavailable("speech unavailable".to_owned()).into_response(),
    };
    if response.status() == StatusCode::TOO_MANY_REQUESTS {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            [(header::RETRY_AFTER, "2")],
            Json(serde_json::json!({"error":"busy"})),
        )
            .into_response();
    }
    if !response.status().is_success()
        || !response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.starts_with("audio/wav"))
        || response
            .content_length()
            .is_some_and(|n| n > MAX_AUDIO_BYTES)
    {
        return AppError::Unavailable("speech unavailable".to_owned()).into_response();
    }
    let mut audio =
        Vec::with_capacity(response.content_length().unwrap_or(0).min(MAX_AUDIO_BYTES) as usize);
    loop {
        match response.chunk().await {
            Ok(Some(chunk)) => {
                if audio.len() as u64 + chunk.len() as u64 > MAX_AUDIO_BYTES {
                    return AppError::Unavailable("speech unavailable".to_owned()).into_response();
                }
                audio.extend_from_slice(&chunk);
            }
            Ok(None) => break,
            Err(_) => {
                return AppError::Unavailable("speech unavailable".to_owned()).into_response();
            }
        }
    }
    drop(permit);
    let mut output = Response::new(axum::body::Body::from(audio));
    output
        .headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static("audio/wav"));
    output.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, no-store"),
    );
    output
}

/// Proxy Qwen's framed PCM stream without buffering it. The worker owns framing
/// validation; this endpoint bounds total bytes and cancels upstream on disconnect.
async fn stream_synthesize(
    State(deps): State<AppDeps>,
    Path(id): Path<String>,
    request: Request,
) -> Response {
    stream_synthesize_impl(deps, id, request, false).await
}

async fn stream_timed_synthesize(
    State(deps): State<AppDeps>,
    Path(id): Path<String>,
    request: Request,
) -> Response {
    stream_synthesize_impl(deps, id, request, true).await
}

async fn stream_synthesize_impl(
    deps: AppDeps,
    id: String,
    request: Request,
    timed: bool,
) -> Response {
    let (_artifact, _) = match authorize(&deps, request.headers(), &id).await {
        Ok(value) => value,
        Err(error) => return error.into_response(),
    };
    let stream_unavailable = if timed {
        pocket_client().is_none()
    } else {
        pocket_client().is_none() && raven_client().is_none() && qwen_client().is_none()
    };
    if !enabled_for(&id) || stream_unavailable {
        return AppError::Unavailable("speech unavailable".to_owned()).into_response();
    }
    let (_, body) =
        match parse_json_request(request, MAX_JSON_BYTES as u64, &deps.config.ingress).await {
            Ok(value) => value,
            Err(response) => return response,
        };
    let (text, voice, instructions) = match validate_speech_body(&body) {
        Ok(value) => value,
        Err(error) => return bad_speech(error),
    };
    if timed && !is_pocket_voice(voice) {
        return bad_speech("bad_voice");
    }
    if !timed
        && (!is_qwen_voice(voice) || !qwen_voice_enabled(voice))
        && !is_pocket_voice(voice)
        && !is_raven_voice(voice)
    {
        return bad_speech("bad_voice");
    }
    let Some(tts) = stream_client_for_voice(voice) else {
        return AppError::Unavailable("speech unavailable".to_owned()).into_response();
    };
    let permit = match tts.permits.try_acquire() {
        Ok(permit) => permit,
        Err(_) => {
            return (
                StatusCode::TOO_MANY_REQUESTS,
                [(header::RETRY_AFTER, "2")],
                Json(serde_json::json!({"error":"busy"})),
            )
                .into_response();
        }
    };
    let mut payload = serde_json::json!({"text":text,"voice":voice});
    if let Some(instructions) = instructions {
        payload["instructions"] = serde_json::Value::String(instructions);
    }
    let response = match tts
        .http
        .post(if timed {
            &tts.timed_stream_endpoint
        } else {
            &tts.stream_endpoint
        })
        .bearer_auth(&tts.token)
        .json(&payload)
        .send()
        .await
    {
        Ok(response) => response,
        Err(_) => return AppError::Unavailable("speech unavailable".to_owned()).into_response(),
    };
    if response.status() == StatusCode::TOO_MANY_REQUESTS {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            [(header::RETRY_AFTER, "2")],
            Json(serde_json::json!({"error":"busy"})),
        )
            .into_response();
    }
    if timed
        && matches!(
            response.status(),
            StatusCode::NOT_FOUND | StatusCode::UNSUPPORTED_MEDIA_TYPE
        )
    {
        return (
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            Json(serde_json::json!({"error":"speech timed unavailable"})),
        )
            .into_response();
    }
    if !response.status().is_success()
        || !response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| {
                if timed {
                    v.split(';')
                        .next()
                        .is_some_and(|value| value.trim() == "application/vnd.artifact.pcm-timed")
                } else {
                    v.starts_with("application/vnd.artifact.pcm")
                }
            })
        || response
            .content_length()
            .is_some_and(|n| n > MAX_STREAM_BYTES)
    {
        return AppError::Unavailable("speech unavailable".to_owned()).into_response();
    }
    let (tx, rx) = mpsc::channel::<Result<Vec<u8>, std::convert::Infallible>>(8);
    let task = tokio::spawn(async move {
        let _permit = permit;
        let mut response = response;
        let mut total = 0u64;
        while let Ok(Some(chunk)) = response.chunk().await {
            total = total.saturating_add(chunk.len() as u64);
            if total > MAX_STREAM_BYTES || tx.send(Ok(chunk.to_vec())).await.is_err() {
                break;
            }
        }
    });
    let stream = StreamBody {
        rx,
        task: Some(task),
    };
    let mut output = Response::new(Body::from_stream(stream));
    output.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(if timed {
            "application/vnd.artifact.pcm-timed"
        } else {
            "application/vnd.artifact.pcm"
        }),
    );
    output.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, no-store"),
    );
    output
}

struct StreamBody {
    rx: mpsc::Receiver<Result<Vec<u8>, std::convert::Infallible>>,
    task: Option<JoinHandle<()>>,
}

impl futures_util::Stream for StreamBody {
    type Item = Result<Vec<u8>, std::convert::Infallible>;
    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        self.rx.poll_recv(cx)
    }
}

impl Drop for StreamBody {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

fn client_for_voice(voice: &str) -> Option<&'static TtsClient> {
    if is_kokoro_voice(voice) {
        client()
    } else if is_pocket_voice(voice) {
        pocket_client()
    } else if is_raven_voice(voice) {
        raven_client()
    } else if is_qwen_voice(voice) {
        qwen_client()
    } else if is_moss_voice(voice) {
        moss_client()
    } else {
        None
    }
}

fn stream_client_for_voice(voice: &str) -> Option<&'static TtsClient> {
    if is_qwen_voice(voice) {
        qwen_client()
    } else if is_pocket_voice(voice) {
        pocket_client()
    } else if is_raven_voice(voice) {
        raven_client()
    } else {
        None
    }
}

fn is_kokoro_voice(voice: &str) -> bool {
    VOICES.iter().any(|(id, _)| *id == voice)
}

fn is_pocket_voice(voice: &str) -> bool {
    POCKET_VOICES.iter().any(|(id, _)| *id == voice)
}

fn is_raven_voice(voice: &str) -> bool {
    RAVEN_VOICES.iter().any(|(id, _)| *id == voice)
}

fn is_qwen_voice(voice: &str) -> bool {
    QWEN_VOICES.iter().any(|(id, _)| *id == voice)
}

fn is_moss_voice(voice: &str) -> bool {
    MOSS_VOICES.iter().any(|(id, _)| *id == voice)
}

fn qwen_voice_enabled(voice: &str) -> bool {
    (voice == "qwen_reference"
        && std::env::var("QWEN_TTS_REFERENCE_VOICE_ENABLED")
            .ok()
            .as_deref()
            != Some("0"))
        || (matches!(voice, "qwen_ryan" | "qwen_aiden")
            && std::env::var("QWEN_TTS_CUSTOM_VOICES_ENABLED")
                .ok()
                .as_deref()
                == Some("1"))
}

fn bad_speech(code: &'static str) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(serde_json::json!({"error":code})),
    )
        .into_response()
}

fn normalize_instructions(value: &str, voice: &str) -> Result<Option<String>, &'static str> {
    let value = value.trim();
    if value.is_empty() {
        return Ok(None);
    }
    if !matches!(voice, "qwen_ryan" | "qwen_aiden")
        || value.chars().count() > MAX_INSTRUCTIONS_CHARS
        || value.chars().any(is_disallowed_instruction_char)
    {
        return Err("bad_instructions");
    }
    Ok(Some(value.to_owned()))
}

fn is_disallowed_instruction_char(c: char) -> bool {
    matches!(c as u32, 0..=8 | 11..=12 | 14..=31 | 127)
}

fn validate_speech_body(body: &OrderedJson) -> Result<(&str, &str, Option<String>), &'static str> {
    let Some(entries) = body.as_object() else {
        return Err("bad_body");
    };
    if entries
        .iter()
        .any(|(key, _)| key != "text" && key != "voice" && key != "instructions")
    {
        return Err("bad_body");
    }
    let Some(text) = body.get("text").and_then(OrderedJson::as_str) else {
        return Err("bad_text");
    };
    let Some(voice) = body.get("voice").and_then(OrderedJson::as_str) else {
        return Err("bad_voice");
    };
    if text.is_empty()
        || text.chars().count() > MAX_TEXT_CHARS
        || text
            .chars()
            .any(|c| c.is_control() && c != '\n' && c != '\t')
    {
        return Err("bad_text");
    }
    if !is_kokoro_voice(voice)
        && !is_pocket_voice(voice)
        && !is_raven_voice(voice)
        && !is_moss_voice(voice)
        && (!is_qwen_voice(voice) || !qwen_voice_enabled(voice))
    {
        return Err("bad_voice");
    }
    let instructions = body
        .get("instructions")
        .map(|value| value.as_str().ok_or("bad_instructions"))
        .transpose()?
        .map(|value| normalize_instructions(value, voice))
        .transpose()?
        .flatten();
    Ok((text, voice, instructions))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fixed_voice_contract_is_unique() {
        assert_eq!(VOICES.len(), 3);
        assert_eq!(POCKET_VOICES.len(), 21);
        assert_eq!(RAVEN_VOICES.len(), 2);
        assert_eq!(QWEN_VOICES.len(), 3);
        assert_eq!(MOSS_VOICES.len(), 5);
        assert!(
            VOICES
                .iter()
                .chain(POCKET_VOICES.iter())
                .chain(RAVEN_VOICES.iter())
                .chain(QWEN_VOICES.iter())
                .chain(MOSS_VOICES.iter())
                .all(|(id, _)| { id.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') })
        );
    }
    #[test]
    fn speech_validation_rejects_unknown_fields_and_bad_voice() {
        let unknown = OrderedJson::Object(vec![
            ("text".into(), OrderedJson::String("hi".into())),
            ("voice".into(), OrderedJson::String("af_heart".into())),
            ("extra".into(), OrderedJson::Null),
        ]);
        assert_eq!(validate_speech_body(&unknown), Err("bad_body"));
        let bad_voice = OrderedJson::Object(vec![
            ("text".into(), OrderedJson::String("hi".into())),
            ("voice".into(), OrderedJson::String("nope".into())),
        ]);
        assert_eq!(validate_speech_body(&bad_voice), Err("bad_voice"));
        let pocket = OrderedJson::Object(vec![
            ("text".into(), OrderedJson::String("hi".into())),
            ("voice".into(), OrderedJson::String("pocket_alba".into())),
        ]);
        assert_eq!(
            validate_speech_body(&pocket),
            Ok(("hi", "pocket_alba", None))
        );
        assert!(is_pocket_voice("pocket_alba"));
        assert!(!is_pocket_voice("alba"));
        assert!(is_raven_voice("raven_alba"));
        assert!(!is_raven_voice("alba"));
        let qwen = OrderedJson::Object(vec![
            ("text".into(), OrderedJson::String("hi".into())),
            ("voice".into(), OrderedJson::String("qwen_reference".into())),
        ]);
        assert_eq!(
            validate_speech_body(&qwen),
            Ok(("hi", "qwen_reference", None))
        );
        assert!(is_qwen_voice("qwen_reference"));
        assert!(!is_qwen_voice("reference"));
    }

    #[test]
    fn speech_validation_normalizes_and_gates_instructions() {
        assert_eq!(
            normalize_instructions("  calm narrator  ", "qwen_ryan"),
            Ok(Some("calm narrator".into()))
        );
        assert_eq!(normalize_instructions("  ", "qwen_ryan"), Ok(None));
        assert_eq!(
            normalize_instructions(&"😀".repeat(MAX_INSTRUCTIONS_CHARS), "qwen_aiden")
                .unwrap()
                .unwrap()
                .chars()
                .count(),
            MAX_INSTRUCTIONS_CHARS
        );
        assert_eq!(
            normalize_instructions(&"😀".repeat(MAX_INSTRUCTIONS_CHARS + 1), "qwen_aiden"),
            Err("bad_instructions")
        );
        assert_eq!(
            normalize_instructions("line\rbreak", "qwen_ryan"),
            Ok(Some("line\rbreak".into()))
        );
        assert_eq!(
            normalize_instructions("line\r\u{0000}break", "qwen_ryan"),
            Err("bad_instructions")
        );
        let rejected = OrderedJson::Object(vec![
            ("text".into(), OrderedJson::String("hi".into())),
            ("voice".into(), OrderedJson::String("af_heart".into())),
            ("instructions".into(), OrderedJson::String("calm".into())),
        ]);
        assert_eq!(validate_speech_body(&rejected), Err("bad_instructions"));
        assert_eq!(
            normalize_instructions(&"x".repeat(MAX_INSTRUCTIONS_CHARS + 1), "qwen_aiden"),
            Err("bad_instructions")
        );
    }

    #[test]
    fn enablement_supports_global_allowlist_legacy_and_disabled_modes() {
        let ids = BTreeSet::from(["book".to_owned(), "report".to_owned()]);
        assert!(speech_enabled_for("book", true, &BTreeSet::new()));
        assert!(speech_enabled_for("book", true, &ids));
        assert!(!speech_enabled_for("other", true, &ids));
        assert!(speech_enabled_for("book", false, &ids));
        assert!(!speech_enabled_for("other", false, &ids));
        assert!(!speech_enabled_for("book", false, &BTreeSet::new()));
    }

    #[test]
    fn speech_validation_enforces_character_cap_and_control_policy() {
        let too_long = OrderedJson::Object(vec![
            (
                "text".into(),
                OrderedJson::String("x".repeat(MAX_TEXT_CHARS + 1)),
            ),
            ("voice".into(), OrderedJson::String("af_heart".into())),
        ]);
        assert_eq!(validate_speech_body(&too_long), Err("bad_text"));
        let control = OrderedJson::Object(vec![
            ("text".into(), OrderedJson::String("hi\u{0000}".into())),
            ("voice".into(), OrderedJson::String("af_heart".into())),
        ]);
        assert_eq!(validate_speech_body(&control), Err("bad_text"));
    }
}
