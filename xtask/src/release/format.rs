//! The input formats the release commands accept.
//!
//! Each function is the exact language of one regular expression from the Node scripts. The
//! JavaScript expressions had no `u` or `m` flag, so `\d` is ASCII and `$` anchors only at the
//! end of the input.

use super::js;

fn is_lower_hex(value: &str, length: usize) -> bool {
    value.len() == length
        && value
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// `^[a-f0-9]{40}$`
pub fn is_commit(value: &str) -> bool {
    is_lower_hex(value, 40)
}

/// `^[a-f0-9]{64}$`
pub fn is_sha256_hex(value: &str) -> bool {
    is_lower_hex(value, 64)
}

/// `^sha256:[a-f0-9]{64}$`
pub fn is_image_digest(value: &str) -> bool {
    value.strip_prefix("sha256:").is_some_and(is_sha256_hex)
}

/// `^[0-9]+\.[0-9]+\.[0-9]+(?:-[0-9A-Za-z.-]+)?$`
pub fn is_version(value: &str) -> bool {
    let (core, prerelease) = match value.split_once('-') {
        Some((core, prerelease)) => (core, Some(prerelease)),
        None => (value, None),
    };
    let mut parts = 0;
    for part in core.split('.') {
        parts += 1;
        if part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()) {
            return false;
        }
    }
    parts == 3
        && prerelease.is_none_or(|prerelease| {
            !prerelease.is_empty()
                && prerelease
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-')
        })
}

/// `^v[0-9]+\.[0-9]+\.[0-9]+(?:-[0-9A-Za-z.-]+)?$`
pub fn is_tag(value: &str) -> bool {
    value.strip_prefix('v').is_some_and(is_version)
}

/// `^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}Z$`
pub fn is_created_shape(value: &str) -> bool {
    const SHAPE: &[u8; 20] = b"dddd-dd-ddTdd:dd:ddZ";
    let bytes = value.as_bytes();
    bytes.len() == SHAPE.len()
        && bytes.iter().zip(SHAPE).all(|(byte, shape)| match shape {
            b'd' => byte.is_ascii_digit(),
            literal => byte == literal,
        })
}

/// The `created` check of `verify-manifest.mjs`: the shape above, and
/// `new Date(created).toISOString().replace(".000Z", "Z") === created`, which holds exactly
/// when every field is in range for its calendar month.
pub fn is_canonical_created(value: &str) -> bool {
    if !is_created_shape(value) {
        return false;
    }
    let field = |range: std::ops::Range<usize>| {
        value[range]
            .parse::<i64>()
            .expect("the shape check admits only digits")
    };
    let (year, month, day) = (field(0..4), field(5..7), field(8..10));
    let (hour, minute, second) = (field(11..13), field(14..16), field(17..19));
    (1..=12).contains(&month)
        && (1..=js::days_in_month(year, month)).contains(&day)
        && hour < 24
        && minute < 60
        && second < 60
}

fn is_name_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'-')
}

/// `^[A-Za-z0-9_.-]+\/[A-Za-z0-9_.-]+$`
pub fn is_repository(value: &str) -> bool {
    value.split_once('/').is_some_and(|(owner, name)| {
        !owner.is_empty()
            && !name.is_empty()
            && owner.bytes().all(is_name_byte)
            && name.bytes().all(is_name_byte)
    })
}

/// `^https:\/\/github\.com\/.+$` (`.` excludes the ECMAScript line terminators).
pub fn is_github_url(value: &str) -> bool {
    value
        .strip_prefix("https://github.com/")
        .is_some_and(|rest| {
            !rest.is_empty() && !rest.contains(['\n', '\r', '\u{2028}', '\u{2029}'])
        })
}

/// `^[A-Za-z0-9][A-Za-z0-9._-]{0,127}$`: a file name beside the manifest.
pub fn is_safe_artifact_path(value: &str) -> bool {
    let bytes = value.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 128
        && bytes[0].is_ascii_alphanumeric()
        && bytes[1..].iter().all(|&b| is_name_byte(b))
}

/// `^ghcr\.io\/([a-z0-9][a-z0-9._-]*)\/([a-z0-9][a-z0-9._-]*):([A-Za-z0-9_][A-Za-z0-9_.-]{0,127})$`
///
/// Returns the owner and image name.
pub fn ghcr_reference(value: &str) -> Option<(&str, &str)> {
    let rest = value.strip_prefix("ghcr.io/")?;
    let (owner, rest) = rest.split_once('/')?;
    let (name, tag) = rest.split_once(':')?;
    let is_component = |part: &str| {
        let bytes = part.as_bytes();
        !bytes.is_empty()
            && matches!(bytes[0], b'a'..=b'z' | b'0'..=b'9')
            && bytes
                .iter()
                .all(|&b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'.' | b'_' | b'-'))
    };
    let tag_bytes = tag.as_bytes();
    let tag_ok = !tag_bytes.is_empty()
        && tag_bytes.len() <= 128
        && (tag_bytes[0].is_ascii_alphanumeric() || tag_bytes[0] == b'_')
        && tag_bytes
            .iter()
            .all(|&b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'));
    (is_component(owner) && is_component(name) && tag_ok).then_some((owner, name))
}

/// `"<image>:<tag>".replace(/:[^/:]+$/, "")`: drop a trailing tag that has no `/` or `:`.
pub fn strip_image_tag(reference: &str) -> &str {
    match reference.rfind(':') {
        Some(index) => {
            let tag = &reference[index + 1..];
            if tag.is_empty() || tag.contains('/') {
                reference
            } else {
                &reference[..index]
            }
        }
        None => reference,
    }
}

/// `https://github.com/<repository>/attestations/<digits>`, with the repository matched
/// literally. The Node verifier built a regular expression from the repository without
/// escaping `.`, so a `.` there matched any character; this check is stricter.
pub fn is_attestation_reference(reference: &str, repository: &str) -> bool {
    reference
        .strip_prefix("https://github.com/")
        .and_then(|rest| rest.strip_prefix(repository))
        .and_then(|rest| rest.strip_prefix("/attestations/"))
        .is_some_and(|id| !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_and_tags() {
        for value in ["1.2.3", "10.0.0-rc.1", "1.2.3-a-b.c", "0.0.0-0"] {
            assert!(is_version(value), "{value}");
            assert!(is_tag(&format!("v{value}")), "{value}");
        }
        for value in [
            "1.2", "1.2.3.4", "1.2.3-", "1..3", "a.b.c", "1.2.3+b", "1.2.3-é", "١.2.3", "1.2.3\n",
        ] {
            assert!(!is_version(value), "{value:?}");
        }
        assert!(!is_tag("1.2.3"));
        assert!(!is_tag("V1.2.3"));
    }

    #[test]
    fn created_timestamps() {
        assert!(is_canonical_created("2026-07-29T18:00:00Z"));
        assert!(is_canonical_created("2024-02-29T23:59:59Z"));
        assert!(is_canonical_created("0000-01-01T00:00:00Z"));
        for value in [
            "2026-02-29T00:00:00Z",
            "2026-13-01T00:00:00Z",
            "2026-00-01T00:00:00Z",
            "2026-04-31T00:00:00Z",
            "2026-01-01T24:00:00Z",
            "2026-01-01T00:60:00Z",
            "2026-01-01T00:00:60Z",
            "2026-01-01T00:00:00.000Z",
            "2026-01-01 00:00:00Z",
            "2026-01-01T00:00:00Z\n",
        ] {
            assert!(!is_canonical_created(value), "{value}");
        }
    }

    #[test]
    fn repositories_paths_and_references() {
        assert!(is_repository("AgentShelf-OSS/artifact-mcp"));
        assert!(!is_repository("a/b/c"));
        assert!(!is_repository("/b"));
        assert!(!is_repository("a/"));
        assert!(is_safe_artifact_path("release-manifest.json"));
        assert!(!is_safe_artifact_path(".hidden"));
        assert!(!is_safe_artifact_path("a/b"));
        assert!(!is_safe_artifact_path(&"a".repeat(129)));
        assert!(is_safe_artifact_path(&"a".repeat(128)));
        assert_eq!(
            ghcr_reference("ghcr.io/agentshelf-oss/artifact-mcp:v1.25.0"),
            Some(("agentshelf-oss", "artifact-mcp"))
        );
        for value in [
            "ghcr.io/AgentShelf-OSS/artifact-mcp:v1",
            "docker.io/a/b:v1",
            "ghcr.io/a/b",
            "ghcr.io/a/b:",
            "ghcr.io/a/b:.v1",
            "ghcr.io/a/b/c:v1",
            "ghcr.io/a/b:v1:v2",
        ] {
            assert_eq!(ghcr_reference(value), None, "{value}");
        }
        assert_eq!(strip_image_tag("ghcr.io/a/b:v1.0.0"), "ghcr.io/a/b");
        assert_eq!(strip_image_tag("localhost:5000/a/b"), "localhost:5000/a/b");
        assert_eq!(
            strip_image_tag("localhost:5000/a/b:v1"),
            "localhost:5000/a/b"
        );
        assert_eq!(strip_image_tag("a/b:"), "a/b:");
        assert_eq!(strip_image_tag("a/b"), "a/b");
        assert!(is_attestation_reference(
            "https://github.com/a.b/c/attestations/12",
            "a.b/c"
        ));
        assert!(!is_attestation_reference(
            "https://github.com/aXb/c/attestations/12",
            "a.b/c"
        ));
        assert!(!is_attestation_reference(
            "https://github.com/a.b/c/attestations/",
            "a.b/c"
        ));
        assert!(is_github_url("https://github.com/x"));
        assert!(!is_github_url("https://github.com/"));
        assert!(!is_github_url("https://github.com/x\ny"));
    }
}
