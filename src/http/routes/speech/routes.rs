use axum::{
    Json, Router,
    body::Body,
    extract::{Path, Request, State},
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use tokio::sync::{SemaphorePermit, mpsc};
use tokio::task::JoinHandle;

use crate::{
    AppDeps,
    error::AppError,
    http::routes::artifact::{authorize, parse_json_request},
};

use super::{
    catalog::{
        MAX_AUDIO_BYTES, MAX_JSON_BYTES, MAX_STREAM_BYTES, MAX_TEXT_CHARS, READER_PITCH_WORKLET,
        SOUNDTOUCH_LICENSE, SOUNDTOUCH_PROCESSOR, SOUNDTOUCH_SOURCE_MAP,
    },
    clients::{
        VoicesResponse, client, client_for_voice, enabled_for, is_pocket_voice, is_qwen_voice,
        is_raven_voice, moss_client, pocket_client, qwen_client, qwen_voice_enabled, raven_client,
        stream_client_for_voice, voices_for_request,
    },
    validation::validate_speech_body,
};

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
    let (response, permit) =
        match request_stream_response(tts, text, voice, instructions, timed).await {
            Ok(value) => value,
            Err(response) => return response,
        };
    let response = match validate_stream_response(response, timed) {
        Ok(response) => response,
        Err(response) => return *response,
    };
    stream_response_body(response, permit, timed)
}

async fn request_stream_response(
    tts: &'static super::clients::TtsClient,
    text: &str,
    voice: &str,
    instructions: Option<String>,
    timed: bool,
) -> Result<(reqwest::Response, SemaphorePermit<'static>), Response> {
    let permit = match tts.permits.try_acquire() {
        Ok(permit) => permit,
        Err(_) => {
            return Err((
                StatusCode::TOO_MANY_REQUESTS,
                [(header::RETRY_AFTER, "2")],
                Json(serde_json::json!({"error":"busy"})),
            )
                .into_response());
        }
    };
    let mut payload = serde_json::json!({"text":text,"voice":voice});
    if let Some(instructions) = instructions {
        payload["instructions"] = serde_json::Value::String(instructions);
    }
    match tts
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
        Ok(response) => Ok((response, permit)),
        Err(_) => Err(AppError::Unavailable("speech unavailable".to_owned()).into_response()),
    }
}

fn validate_stream_response(
    response: reqwest::Response,
    timed: bool,
) -> Result<reqwest::Response, Box<Response>> {
    if response.status() == StatusCode::TOO_MANY_REQUESTS {
        return Err(Box::new(
            (
                StatusCode::TOO_MANY_REQUESTS,
                [(header::RETRY_AFTER, "2")],
                Json(serde_json::json!({"error":"busy"})),
            )
                .into_response(),
        ));
    }
    if timed
        && matches!(
            response.status(),
            StatusCode::NOT_FOUND | StatusCode::UNSUPPORTED_MEDIA_TYPE
        )
    {
        return Err(Box::new(
            (
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                Json(serde_json::json!({"error":"speech timed unavailable"})),
            )
                .into_response(),
        ));
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
        return Err(Box::new(
            AppError::Unavailable("speech unavailable".to_owned()).into_response(),
        ));
    }
    Ok(response)
}

fn stream_response_body(
    response: reqwest::Response,
    permit: SemaphorePermit<'static>,
    timed: bool,
) -> Response {
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

fn bad_speech(code: &'static str) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(serde_json::json!({"error":code})),
    )
        .into_response()
}
#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::StreamExt;
    use std::time::Duration;

    #[tokio::test]
    async fn stream_body_yields_the_first_chunk_before_the_upstream_closes() {
        let (tx, rx) = mpsc::channel(1);
        let mut stream = StreamBody { rx, task: None };

        tx.send(Ok(vec![1, 2, 3]))
            .await
            .expect("stream receiver remains connected");
        let first = tokio::time::timeout(Duration::from_millis(100), stream.next())
            .await
            .expect("the first stream chunk is not buffered")
            .expect("first stream chunk");

        assert_eq!(first, Ok(vec![1, 2, 3]));
        assert!(!tx.is_closed());
    }
}
