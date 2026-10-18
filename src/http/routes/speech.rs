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
const POCKET_VOICES: &[(&str, &str, f64)] = &[
    ("pocket_alba", "Alba · Pocket", 0.0),
    ("pocket_marius", "Marius · Pocket", 8.5),
    ("pocket_javert", "Javert · Pocket", 7.0),
    ("pocket_jean", "Jean · Pocket", 0.0),
    ("pocket_cosette", "Cosette · Pocket", 11.5),
    ("pocket_eponine", "Eponine · Pocket", 1.5),
    ("pocket_fantine", "Fantine · Pocket", 1.0),
    ("pocket_azelma", "Azelma · Pocket", 1.5),
    ("pocket_anna", "Anna · Pocket", 1.0),
    ("pocket_bill_boerst", "Bill Boerst · Pocket", 8.0),
    ("pocket_caro_davy", "Caro Davy · Pocket", 7.0),
    ("pocket_charles", "Charles · Pocket", 3.5),
    ("pocket_eve", "Eve · Pocket", 2.0),
    ("pocket_george", "George · Pocket", 1.0),
    ("pocket_jane", "Jane · Pocket", 1.5),
    ("pocket_mary", "Mary · Pocket", 1.5),
    ("pocket_michael", "Michael · Pocket", 2.5),
    ("pocket_paul", "Paul · Pocket", 0.5),
    ("pocket_peter_yearsley", "Peter Yearsley · Pocket", 10.5),
    ("pocket_stuart_bell", "Stuart Bell · Pocket", 12.0),
    ("pocket_vera", "Vera · Pocket", 2.5),
];

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ExtraPocketVoice {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) gain_db: Option<f64>,
}

/// Searchable tags shown by the Listen voice picker (ADR-0020).
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub(crate) struct VoiceTags {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) gender: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) accent: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) style: Option<String>,
    pub(crate) source: String,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct CatalogVoice {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) gain_db: Option<f64>,
    pub(crate) tags: VoiceTags,
}

const MAX_CATALOG_VOICES: usize = 400;
const MAX_CATALOG_BYTES: u64 = 512 * 1024;

fn catalog_text(
    value: Option<&serde_json::Value>,
    max: usize,
) -> Result<Option<String>, &'static str> {
    match value {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(serde_json::Value::String(text))
            if (1..=max).contains(&text.chars().count())
                && text.bytes().all(|byte| (b' '..=b'~').contains(&byte)) =>
        {
            Ok(Some(text.clone()))
        }
        _ => Err("POCKET_TTS_VOICE_CATALOG has an invalid tag"),
    }
}

/// Parse the operator's Pocket voice catalog (ADR-0020). Entries add voices or override the
/// label, gain and tags of a built-in or `POCKET_TTS_EXTRA_VOICES` voice with the same id.
pub(crate) fn parse_pocket_catalog(raw: &str) -> Result<Vec<CatalogVoice>, &'static str> {
    let error = "POCKET_TTS_VOICE_CATALOG is invalid";
    let root: serde_json::Value = serde_json::from_str(raw).map_err(|_| error)?;
    let object = root.as_object().ok_or(error)?;
    if object.keys().any(|key| key != "version" && key != "voices")
        || object.get("version") != Some(&serde_json::json!(1))
    {
        return Err(error);
    }
    let entries = object
        .get("voices")
        .and_then(|value| value.as_array())
        .ok_or(error)?;
    if entries.len() > MAX_CATALOG_VOICES {
        return Err("POCKET_TTS_VOICE_CATALOG has too many voices");
    }
    let mut voices: Vec<CatalogVoice> = Vec::new();
    for entry in entries {
        let fields = entry.as_object().ok_or(error)?;
        const KEYS: [&str; 7] = [
            "id", "label", "gainDb", "gender", "accent", "style", "source",
        ];
        if fields.keys().any(|key| !KEYS.contains(&key.as_str())) {
            return Err(error);
        }
        let id = fields
            .get("id")
            .and_then(|value| value.as_str())
            .ok_or(error)?;
        if !(1..=40).contains(&id.len())
            || !id
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
            || voices
                .iter()
                .any(|voice| voice.id == format!("pocket_{id}"))
        {
            return Err("POCKET_TTS_VOICE_CATALOG contains an invalid or duplicate id");
        }
        let label = fields
            .get("label")
            .and_then(|value| value.as_str())
            .ok_or(error)?;
        if !(1..=40).contains(&label.chars().count())
            || !label
                .bytes()
                .all(|byte| (b' '..=b'~').contains(&byte) && byte != b',' && byte != b':')
        {
            return Err(
                "POCKET_TTS_VOICE_CATALOG labels must be 1 to 40 printable ASCII characters",
            );
        }
        let gain_db = match fields.get("gainDb") {
            None | Some(serde_json::Value::Null) => None,
            Some(value) => {
                let gain = value
                    .as_f64()
                    .filter(|gain| (-12.0..=12.0).contains(gain))
                    .ok_or("POCKET_TTS_VOICE_CATALOG gainDb must be a number from -12 to 12")?;
                Some(gain)
            }
        };
        let gender = catalog_text(fields.get("gender"), 8)?;
        if gender
            .as_deref()
            .is_some_and(|value| !matches!(value, "female" | "male" | "neutral"))
        {
            return Err("POCKET_TTS_VOICE_CATALOG gender must be female, male or neutral");
        }
        let source = fields
            .get("source")
            .and_then(|value| value.as_str())
            .ok_or(error)?;
        if !matches!(
            source,
            "pocket-preset" | "kyutai-donation" | "voice-zero" | "custom"
        ) {
            return Err("POCKET_TTS_VOICE_CATALOG has an unknown source");
        }
        voices.push(CatalogVoice {
            id: format!("pocket_{id}"),
            name: format!("{label} · Pocket"),
            gain_db,
            tags: VoiceTags {
                gender,
                accent: catalog_text(fields.get("accent"), 24)?,
                style: catalog_text(fields.get("style"), 24)?,
                source: source.to_owned(),
            },
        });
    }
    Ok(voices)
}

/// The catalog named by `POCKET_TTS_VOICE_CATALOG`, re-read when the file changes. A missing
/// or invalid catalog is logged and ignored so a bad file cannot take narration down.
pub(crate) fn pocket_catalog() -> Vec<CatalogVoice> {
    use std::sync::{Mutex, OnceLock};
    type Cached = (
        String,
        Option<std::time::SystemTime>,
        u64,
        Vec<CatalogVoice>,
    );
    static CACHE: OnceLock<Mutex<Option<Cached>>> = OnceLock::new();
    let Some(path) = std::env::var("POCKET_TTS_VOICE_CATALOG")
        .ok()
        .filter(|value| !value.trim().is_empty())
    else {
        return Vec::new();
    };
    let meta = std::fs::metadata(&path).ok();
    let (modified, len) = (
        meta.as_ref().and_then(|m| m.modified().ok()),
        meta.as_ref().map_or(0, |m| m.len()),
    );
    let cache = CACHE.get_or_init(|| Mutex::new(None));
    let mut guard = cache
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some((cached_path, cached_modified, cached_len, voices)) = guard.as_ref()
        && *cached_path == path
        && *cached_modified == modified
        && *cached_len == len
    {
        return voices.clone();
    }
    let voices = if len > MAX_CATALOG_BYTES {
        tracing::warn!(path = %path, "pocket voice catalog is too large; ignored");
        Vec::new()
    } else {
        match std::fs::read_to_string(&path)
            .map_err(|_| "unreadable")
            .and_then(|raw| parse_pocket_catalog(&raw))
        {
            Ok(voices) => voices,
            Err(error) => {
                tracing::warn!(path = %path, error = %error, "pocket voice catalog ignored");
                Vec::new()
            }
        }
    };
    *guard = Some((path, modified, len, voices.clone()));
    voices
}

/// Parse operator supplied Pocket voices. The result is shared by startup validation and
/// request handling so the accepted IDs cannot differ from the advertised IDs.
pub(crate) fn parse_extra_pocket_voices(
    raw: Option<&str>,
) -> Result<Vec<ExtraPocketVoice>, &'static str> {
    let Some(raw) = raw.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(Vec::new());
    };
    let mut voices = Vec::new();
    for entry in raw.split(',') {
        if voices.len() == 32 || entry.trim().is_empty() {
            return Err("POCKET_TTS_EXTRA_VOICES must contain at most 32 nonempty entries");
        }
        let parts: Vec<_> = entry.trim().split(':').collect();
        if !(2..=3).contains(&parts.len()) {
            return Err("POCKET_TTS_EXTRA_VOICES entries must be id:Label[:gainDb]");
        }
        let (id, label) = (parts[0], parts[1]);
        if id.len() > 32
            || !id.as_bytes().first().is_some_and(u8::is_ascii_lowercase)
            || !id
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
            || POCKET_VOICES
                .iter()
                .any(|(built_in, _, _)| *built_in == format!("pocket_{id}"))
            || voices
                .iter()
                .any(|voice: &ExtraPocketVoice| voice.id == format!("pocket_{id}"))
        {
            return Err("POCKET_TTS_EXTRA_VOICES contains an invalid or duplicate id");
        }
        if !(1..=40).contains(&label.chars().count())
            || !label
                .bytes()
                .all(|byte| (b' '..=b'~').contains(&byte) && byte != b',' && byte != b':')
        {
            return Err(
                "POCKET_TTS_EXTRA_VOICES labels must be 1 to 40 printable ASCII characters",
            );
        }
        let gain_db = if parts.len() == 3 {
            let gain = parts[2];
            let unsigned = gain.strip_prefix('-').unwrap_or(gain);
            let mut segments = unsigned.split('.');
            let integer = segments.next().unwrap_or_default();
            let fraction = segments.next();
            if integer.is_empty()
                || !integer.bytes().all(|byte| byte.is_ascii_digit())
                || fraction.is_some_and(|value| {
                    value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit())
                })
                || segments.next().is_some()
            {
                return Err("POCKET_TTS_EXTRA_VOICES gainDb must be a decimal from -12 to 12");
            }
            let value: f64 = gain
                .parse()
                .map_err(|_| "POCKET_TTS_EXTRA_VOICES gainDb must be a decimal from -12 to 12")?;
            if !(-12.0..=12.0).contains(&value) {
                return Err("POCKET_TTS_EXTRA_VOICES gainDb must be a decimal from -12 to 12");
            }
            Some(value)
        } else {
            None
        };
        voices.push(ExtraPocketVoice {
            id: format!("pocket_{id}"),
            name: format!("{label} · Pocket"),
            gain_db,
        });
    }
    Ok(voices)
}
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
        permits: Semaphore::new(worker_concurrency().expect("invalid TTS_WORKER_CONCURRENCY")),
    })
}

/// Each configured worker has its own bounded lane. Keep this parser small and
/// shared so an invalid deployment value fails during client initialization.
fn worker_concurrency() -> Result<usize, &'static str> {
    parse_worker_concurrency(std::env::var("TTS_WORKER_CONCURRENCY").ok().as_deref())
}

/// Absent or blank means the default of 3. Startup configuration validation
/// rejects other invalid values before the first speech request.
pub(crate) fn parse_worker_concurrency(raw: Option<&str>) -> Result<usize, &'static str> {
    match raw.map(str::trim).filter(|value| !value.is_empty()) {
        None => Ok(3),
        Some(value) => match value.parse::<usize>() {
            Ok(value @ 1..=8) => Ok(value),
            _ => Err("TTS_WORKER_CONCURRENCY must be an integer from 1 to 8"),
        },
    }
}

fn voices_for_request() -> Vec<Voice> {
    let mut voices = Vec::new();
    if client().is_some() {
        voices.extend(VOICES.iter().map(|&(id, name)| Voice {
            id: id.to_owned(),
            name: name.to_owned(),
            streaming: false,
            gain_db: None,
            tags: None,
        }));
    }
    if pocket_client().is_some() {
        voices.extend(POCKET_VOICES.iter().map(|&(id, name, gain_db)| Voice {
            id: id.to_owned(),
            name: name.to_owned(),
            streaming: true,
            gain_db: Some(json_number(gain_db)),
            tags: None,
        }));
        voices.extend(
            parse_extra_pocket_voices(std::env::var("POCKET_TTS_EXTRA_VOICES").ok().as_deref())
                .unwrap_or_default()
                .into_iter()
                .map(|voice| Voice {
                    id: voice.id,
                    name: voice.name,
                    streaming: true,
                    gain_db: voice.gain_db.map(json_number),
                    tags: None,
                }),
        );
        for entry in pocket_catalog() {
            let voice = Voice {
                id: entry.id.clone(),
                name: entry.name,
                streaming: true,
                gain_db: entry.gain_db.map(json_number),
                tags: Some(entry.tags),
            };
            match voices.iter_mut().find(|existing| existing.id == entry.id) {
                Some(existing) => *existing = voice,
                None => voices.push(voice),
            }
        }
    }
    if raven_client().is_some() {
        voices.extend(RAVEN_VOICES.iter().map(|&(id, name)| Voice {
            id: id.to_owned(),
            name: name.to_owned(),
            streaming: false,
            gain_db: None,
            tags: None,
        }));
    }
    if qwen_client().is_some() {
        voices.extend(
            QWEN_VOICES
                .iter()
                .filter(|(id, _)| qwen_voice_enabled(id))
                .map(|&(id, name)| Voice {
                    id: id.to_owned(),
                    name: name.to_owned(),
                    streaming: true,
                    gain_db: None,
                    tags: None,
                }),
        );
    }
    if moss_client().is_some() {
        voices.extend(MOSS_VOICES.iter().map(|&(id, name)| Voice {
            id: id.to_owned(),
            name: name.to_owned(),
            streaming: false,
            gain_db: None,
            tags: None,
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
    id: String,
    name: String,
    #[serde(skip_serializing_if = "is_false")]
    streaming: bool,
    #[serde(rename = "gainDb", skip_serializing_if = "Option::is_none")]
    gain_db: Option<serde_json::Number>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tags: Option<VoiceTags>,
}

fn json_number(value: f64) -> serde_json::Number {
    if value.fract() == 0.0 {
        serde_json::Number::from(value as i64)
    } else {
        serde_json::Number::from_f64(value).expect("finite voice gain")
    }
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
    POCKET_VOICES.iter().any(|(id, _, _)| *id == voice)
        || pocket_catalog().iter().any(|entry| entry.id == voice)
        || parse_extra_pocket_voices(std::env::var("POCKET_TTS_EXTRA_VOICES").ok().as_deref())
            .is_ok_and(|voices| voices.iter().any(|extra| extra.id == voice))
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
                .chain(RAVEN_VOICES.iter())
                .chain(QWEN_VOICES.iter())
                .chain(MOSS_VOICES.iter())
                .all(|(id, _)| id.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'))
        );
        assert!(POCKET_VOICES.iter().all(|(id, _, gain)| {
            id.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                && (-12.0..=12.0).contains(gain)
        }));
    }

    #[test]
    fn worker_concurrency_validation_has_a_safe_default_and_bounds() {
        let parse = super::parse_worker_concurrency;
        assert_eq!(parse(None), Ok(3));
        assert_eq!(parse(Some("  ")), Ok(3));
        assert_eq!(parse(Some(" 8 ")), Ok(8));
        assert_eq!(parse(Some("3")), Ok(3));
        assert!(parse(Some("0")).is_err());
        assert!(parse(Some("9")).is_err());
        assert!(parse(Some("many")).is_err());
    }

    #[test]
    fn extra_pocket_voices_follow_the_configuration_contract() {
        let parsed = parse_extra_pocket_voices(Some("estelle:Estelle:2.5,lola:Lola"))
            .expect("valid configured voices");
        assert_eq!(parsed[0].id, "pocket_estelle");
        assert_eq!(parsed[0].name, "Estelle · Pocket");
        assert_eq!(parsed[0].gain_db, Some(2.5));
        assert_eq!(parsed[1].gain_db, None);
        for invalid in [
            "Bad:Voice",
            "alba:Built in",
            "lola:Lola,lola:Again",
            "lola:Lola:12.1",
            "lola:Lola:1e2",
            "lola:Lola:+1",
            "lola:Lola:.5",
            "lola:Lola:1.",
            ",lola:Lola",
            "lola:Lola,",
            "lola:Lola,,estelle:Estelle",
            "lola:",
        ] {
            assert!(
                parse_extra_pocket_voices(Some(invalid)).is_err(),
                "{invalid}"
            );
        }
        let too_many = (0..33)
            .map(|index| format!("extra_{index}:Voice"))
            .collect::<Vec<_>>()
            .join(",");
        assert!(parse_extra_pocket_voices(Some(&too_many)).is_err());
        assert!(parse_extra_pocket_voices(Some("  ")).unwrap().is_empty());
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

#[cfg(test)]
mod catalog_tests {
    use super::parse_pocket_catalog;

    #[test]
    fn catalog_parses_tags_and_prefixes_ids() {
        let voices = parse_pocket_catalog(r#"{"version":1,"voices":[
            {"id":"andrea","label":"Andrea","gainDb":6,"gender":"female","accent":"American","style":"warm","source":"kyutai-donation"},
            {"id":"alba","label":"Alba","gainDb":0,"gender":null,"accent":null,"style":null,"source":"pocket-preset"}
        ]}"#)
        .unwrap();
        assert_eq!(voices[0].id, "pocket_andrea");
        assert_eq!(voices[0].name, "Andrea · Pocket");
        assert_eq!(voices[0].gain_db, Some(6.0));
        assert_eq!(voices[0].tags.gender.as_deref(), Some("female"));
        assert_eq!(voices[0].tags.accent.as_deref(), Some("American"));
        assert_eq!(voices[1].tags.gender, None);
        assert_eq!(voices[1].tags.source, "pocket-preset");
    }

    #[test]
    fn catalog_rejects_bad_entries() {
        let one =
            |entry: &str| parse_pocket_catalog(&format!(r#"{{"version":1,"voices":[{entry}]}}"#));
        assert!(one(r#"{"id":"Bad-Id","label":"x","source":"custom"}"#).is_err());
        assert!(one(r#"{"id":"ok","label":"a,b","source":"custom"}"#).is_err());
        assert!(one(r#"{"id":"ok","label":"x","gainDb":13,"source":"custom"}"#).is_err());
        assert!(one(r#"{"id":"ok","label":"x","gender":"robot","source":"custom"}"#).is_err());
        assert!(one(r#"{"id":"ok","label":"x","source":"elsewhere"}"#).is_err());
        assert!(one(r#"{"id":"ok","label":"x","source":"custom","extra":1}"#).is_err());
        assert!(one(r#"{"id":"ok","label":"x","accent":"waaaaaaaaaaaaaaaaaaaaaaaaay too long","source":"custom"}"#).is_err());
        assert!(parse_pocket_catalog(r#"{"version":2,"voices":[]}"#).is_err());
        assert!(parse_pocket_catalog(r#"{"version":1,"voices":[{"id":"a","label":"A","source":"custom"},{"id":"a","label":"B","source":"custom"}]}"#).is_err());
        let many = (0..401)
            .map(|i| format!(r#"{{"id":"v{i}","label":"V","source":"custom"}}"#))
            .collect::<Vec<_>>()
            .join(",");
        assert!(parse_pocket_catalog(&format!(r#"{{"version":1,"voices":[{many}]}}"#)).is_err());
        assert!(one(r#"{"id":"ok","label":"x","source":"custom"}"#).is_ok());
    }
}
