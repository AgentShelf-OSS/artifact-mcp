use super::{
    catalog::{MAX_INSTRUCTIONS_CHARS, MAX_TEXT_CHARS},
    clients::{
        is_kokoro_voice, is_moss_voice, is_pocket_voice, is_qwen_voice, is_raven_voice,
        qwen_voice_enabled,
    },
};
use crate::mcp::protocol::OrderedJson;

pub(super) fn normalize_instructions(
    value: &str,
    voice: &str,
) -> Result<Option<String>, &'static str> {
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

pub(super) fn is_disallowed_instruction_char(c: char) -> bool {
    matches!(c as u32, 0..=8 | 11..=12 | 14..=31 | 127)
}

pub(super) fn validate_speech_body(
    body: &OrderedJson,
) -> Result<(&str, &str, Option<String>), &'static str> {
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
