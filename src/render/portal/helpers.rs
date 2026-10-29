use super::*;

pub(super) fn modified(artifact: &ArtifactMeta) -> &str {
    if artifact.updated_at.0.is_empty() {
        &artifact.created_at.0
    } else {
        &artifact.updated_at.0
    }
}

pub(super) fn category_cmp(left: &str, right: &str) -> Ordering {
    match (left.is_empty(), right.is_empty()) {
        (true, false) => Ordering::Greater,
        (false, true) => Ordering::Less,
        _ => left
            .to_lowercase()
            .encode_utf16()
            .cmp(right.to_lowercase().encode_utf16()),
    }
}

pub(super) fn color_for(
    org: &OrgId,
    colors: &std::collections::BTreeMap<OrgId, Option<String>>,
) -> String {
    org_color(&org.0, colors.get(org).and_then(Option::as_deref))
}

pub fn org_color(name: &str, color: Option<&str>) -> String {
    if let Some(color) = color.filter(|color| valid_hex_color(color)) {
        return color.to_owned();
    }
    if name == "admin" {
        return "#66578B".to_owned();
    }
    let mut hash = 2_166_136_261_u32;
    for character in name.chars() {
        let mut units = [0_u16; 2];
        let first = character.encode_utf16(&mut units)[0];
        hash ^= u32::from(first);
        hash = hash.wrapping_mul(16_777_619);
    }
    format!("hsl({} 68% 52%)", hash % 360)
}

pub(super) fn valid_hex_color(value: &str) -> bool {
    matches!(value.len(), 4 | 7)
        && value.starts_with('#')
        && value.as_bytes()[1..].iter().all(u8::is_ascii_hexdigit)
}

pub(super) fn body_version(artifact: &ArtifactMeta) -> String {
    let value = if artifact.body_sha256.is_empty() {
        if artifact.revision == 0 {
            String::new()
        } else {
            artifact.revision.to_string()
        }
    } else {
        artifact.body_sha256.clone()
    };
    utf16_slice(&value, 12)
}

pub(super) fn utf16_slice(value: &str, units: usize) -> String {
    String::from_utf16_lossy(&value.encode_utf16().take(units).collect::<Vec<_>>())
}

pub(super) fn format_date(value: &str) -> String {
    let bytes = value.as_bytes();
    if bytes.len() < 10
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || !bytes[..4].iter().all(u8::is_ascii_digit)
        || !bytes[5..7].iter().all(u8::is_ascii_digit)
        || !bytes[8..10].iter().all(u8::is_ascii_digit)
    {
        return String::new();
    }
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let month = usize::from(bytes[5] - b'0') * 10 + usize::from(bytes[6] - b'0');
    let day = usize::from(bytes[8] - b'0') * 10 + usize::from(bytes[9] - b'0');
    let month_name = month
        .checked_sub(1)
        .and_then(|index| MONTHS.get(index))
        .copied()
        .unwrap_or("undefined");
    format!("{month_name} {day}, {}", &value[..4])
}

pub(super) fn relative_time(value: &str, now: i64) -> String {
    let parsed = if value.len() == 10 {
        format!("{value}T00:00:00Z")
    } else {
        format!("{}Z", value.replacen(' ', "T", 1))
    };
    let Some(timestamp) = OffsetDateTime::parse(&parsed, &Rfc3339)
        .ok()
        .map(OffsetDateTime::unix_timestamp)
    else {
        return "Just now".to_owned();
    };
    let seconds = now.saturating_sub(timestamp).max(0);
    match seconds {
        0..=59 => "Just now".to_owned(),
        60..=3_599 => format!("{}m ago", seconds / 60),
        3_600..=86_399 => format!("{}h ago", seconds / 3_600),
        86_400..=604_799 => format!("{}d ago", seconds / 86_400),
        _ => format_date(value),
    }
}

pub(super) fn format_bytes(bytes: u64) -> String {
    let value = bytes as f64;
    if value < 1_024.0 {
        return format!("{bytes} B");
    }
    if value < 1_048_576.0 {
        return format!("{} KB", (value / 1_024.0).round().max(1.0) as u64);
    }
    let tenths = (value / 1_048_576.0 * 10.0).round() as u64;
    format!("{}.{:01} MB", tenths / 10, tenths % 10)
}

pub(super) fn notification_snippet(value: &str) -> String {
    let mut collapsed = String::with_capacity(value.len());
    let mut whitespace = false;
    for character in value.chars() {
        if is_js_whitespace(character) {
            whitespace = true;
        } else {
            if whitespace && !collapsed.is_empty() {
                collapsed.push(' ');
            }
            whitespace = false;
            collapsed.push(character);
        }
    }
    utf16_slice(js_trim(&collapsed), 120)
}

pub(super) fn js_trim(value: &str) -> &str {
    value.trim_matches(is_js_whitespace)
}

pub(super) fn is_js_whitespace(character: char) -> bool {
    matches!(
        character,
        '\u{0009}'
            | '\u{000A}'
            | '\u{000B}'
            | '\u{000C}'
            | '\u{000D}'
            | '\u{0020}'
            | '\u{00A0}'
            | '\u{1680}'
            | '\u{2000}'
            ..='\u{200A}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202F}'
                | '\u{205F}'
                | '\u{3000}'
                | '\u{FEFF}'
    )
}

pub(super) fn encode_uri_component(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        if byte.is_ascii_alphanumeric()
            || matches!(
                *byte,
                b'-' | b'_' | b'.' | b'!' | b'~' | b'*' | b'\'' | b'(' | b')'
            )
        {
            encoded.push(char::from(*byte));
        } else {
            use fmt::Write as _;
            let _ = write!(encoded, "%{byte:02X}");
        }
    }
    encoded
}
