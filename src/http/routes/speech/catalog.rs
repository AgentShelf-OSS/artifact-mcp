//! Optional, authenticated speech synthesis for selected artifacts.

use std::time::Duration;

use serde::Serialize;

pub(super) const MAX_TEXT_CHARS: usize = 1_500;
pub(super) const MAX_INSTRUCTIONS_CHARS: usize = 500;
pub(super) const MAX_JSON_BYTES: usize = 8_192;
pub(super) const MAX_AUDIO_BYTES: u64 = 4 * 1024 * 1024;
pub(super) const MAX_STREAM_BYTES: u64 = 4_100_000;
pub(super) const WORKER_TIMEOUT: Duration = Duration::from_secs(60);
pub(super) const SOUNDTOUCH_PROCESSOR: &str =
    include_str!("../../../../assets/vendor/soundtouch-processor.js");
pub(super) const SOUNDTOUCH_SOURCE_MAP: &str =
    include_str!("../../../../assets/vendor/soundtouch-processor.js.map");
pub(super) const SOUNDTOUCH_LICENSE: &str =
    include_str!("../../../../assets/vendor/soundtouch-LICENSE.txt");
pub(super) const READER_PITCH_WORKLET: &str =
    include_str!("../../../../assets/reader-pitch-worklet.js");

pub(super) const VOICES: &[(&str, &str)] = &[
    ("bm_george", "George · Kokoro (British)"),
    ("bf_emma", "Emma · Kokoro (British)"),
    ("af_heart", "Heart · Kokoro (American)"),
];
pub(super) const POCKET_VOICES: &[(&str, &str, f64)] = &[
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

#[cfg(test)]
mod tests {
    use super::*;

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
