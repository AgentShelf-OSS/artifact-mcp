use std::{collections::BTreeSet, sync::OnceLock};

use serde::Serialize;
use tokio::sync::Semaphore;

use super::catalog::{
    POCKET_VOICES, VOICES, VoiceTags, WORKER_TIMEOUT, parse_extra_pocket_voices, pocket_catalog,
};

pub(super) const RAVEN_VOICES: &[(&str, &str)] = &[
    ("raven_alba", "Alba · RAVEN (trial)"),
    ("raven_marius", "Marius · RAVEN (trial)"),
];
pub(super) const QWEN_VOICES: &[(&str, &str)] = &[
    ("qwen_reference", "Reference voice · Qwen"),
    ("qwen_ryan", "Ryan · Qwen"),
    ("qwen_aiden", "Aiden · Qwen"),
];
pub(super) const MOSS_VOICES: &[(&str, &str)] = &[
    ("moss_trump", "Trump · MOSS-TTS Nano"),
    ("moss_ava", "Ava · MOSS-TTS Nano"),
    ("moss_bella", "Bella · MOSS-TTS Nano"),
    ("moss_adam", "Adam · MOSS-TTS Nano"),
    ("moss_nathan", "Nathan · MOSS-TTS Nano"),
];

pub(super) struct TtsClient {
    pub(super) endpoint: String,
    pub(super) stream_endpoint: String,
    pub(super) timed_stream_endpoint: String,
    pub(super) token: String,
    pub(super) http: reqwest::Client,
    pub(super) permits: Semaphore,
}

static CLIENT: OnceLock<Option<TtsClient>> = OnceLock::new();
static POCKET_CLIENT: OnceLock<Option<TtsClient>> = OnceLock::new();
static RAVEN_CLIENT: OnceLock<Option<TtsClient>> = OnceLock::new();
static QWEN_CLIENT: OnceLock<Option<TtsClient>> = OnceLock::new();
static MOSS_CLIENT: OnceLock<Option<TtsClient>> = OnceLock::new();
static ARTIFACTS: OnceLock<BTreeSet<String>> = OnceLock::new();

pub(super) fn client() -> Option<&'static TtsClient> {
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

pub(super) fn pocket_client() -> Option<&'static TtsClient> {
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

pub(super) fn qwen_client() -> Option<&'static TtsClient> {
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

pub(super) fn raven_client() -> Option<&'static TtsClient> {
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

pub(super) fn moss_client() -> Option<&'static TtsClient> {
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

pub(super) fn voices_for_request() -> Vec<Voice> {
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

pub(super) fn enabled_for(id: &str) -> bool {
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

pub(super) fn speech_enabled_for(id: &str, globally_enabled: bool, ids: &BTreeSet<String>) -> bool {
    (globally_enabled || !ids.is_empty()) && (ids.is_empty() || ids.contains(id))
}

#[derive(Serialize)]
pub(super) struct Voice {
    id: String,
    name: String,
    #[serde(skip_serializing_if = "is_false")]
    streaming: bool,
    #[serde(rename = "gainDb", skip_serializing_if = "Option::is_none")]
    gain_db: Option<serde_json::Number>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tags: Option<VoiceTags>,
}

pub(super) fn json_number(value: f64) -> serde_json::Number {
    if value.fract() == 0.0 {
        serde_json::Number::from(value as i64)
    } else {
        serde_json::Number::from_f64(value).expect("finite voice gain")
    }
}

pub(super) fn is_false(value: &bool) -> bool {
    !*value
}

#[derive(Serialize)]
pub(super) struct VoicesResponse {
    pub(super) enabled: bool,
    pub(super) voices: Vec<Voice>,
    #[serde(rename = "maxChars")]
    pub(super) max_chars: usize,
}

pub(super) fn client_for_voice(voice: &str) -> Option<&'static TtsClient> {
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

pub(super) fn stream_client_for_voice(voice: &str) -> Option<&'static TtsClient> {
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

pub(super) fn is_kokoro_voice(voice: &str) -> bool {
    VOICES.iter().any(|(id, _)| *id == voice)
}

pub(super) fn is_pocket_voice(voice: &str) -> bool {
    POCKET_VOICES.iter().any(|(id, _, _)| *id == voice)
        || pocket_catalog().iter().any(|entry| entry.id == voice)
        || parse_extra_pocket_voices(std::env::var("POCKET_TTS_EXTRA_VOICES").ok().as_deref())
            .is_ok_and(|voices| voices.iter().any(|extra| extra.id == voice))
}

pub(super) fn is_raven_voice(voice: &str) -> bool {
    RAVEN_VOICES.iter().any(|(id, _)| *id == voice)
}

pub(super) fn is_qwen_voice(voice: &str) -> bool {
    QWEN_VOICES.iter().any(|(id, _)| *id == voice)
}

pub(super) fn is_moss_voice(voice: &str) -> bool {
    MOSS_VOICES.iter().any(|(id, _)| *id == voice)
}

pub(super) fn qwen_voice_enabled(voice: &str) -> bool {
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
    fn enablement_supports_global_allowlist_legacy_and_disabled_modes() {
        let ids = BTreeSet::from(["book".to_owned(), "report".to_owned()]);
        assert!(speech_enabled_for("book", true, &BTreeSet::new()));
        assert!(speech_enabled_for("book", true, &ids));
        assert!(!speech_enabled_for("other", true, &ids));
        assert!(speech_enabled_for("book", false, &ids));
        assert!(!speech_enabled_for("other", false, &ids));
        assert!(!speech_enabled_for("book", false, &BTreeSet::new()));
    }
}
