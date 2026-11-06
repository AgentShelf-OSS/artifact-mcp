pub(crate) const POCKET_VOICES: &[(&str, &str, f64)] = &[
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
    fn worker_concurrency_validation_has_a_safe_default_and_bounds() {
        let parse = parse_worker_concurrency;
        assert_eq!(parse(None), Ok(3));
        assert_eq!(parse(Some("  ")), Ok(3));
        assert_eq!(parse(Some(" 8 ")), Ok(8));
        assert_eq!(parse(Some("3")), Ok(3));
        assert!(parse(Some("0")).is_err());
        assert!(parse(Some("9")).is_err());
        assert!(parse(Some("many")).is_err());
    }
}
