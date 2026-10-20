//! ADR-0023 stale-base guard: pure segment digests and base/stale evaluation.
//!
//! This module performs no I/O. `conformance/stale-guard-reference.py` is the reference; the
//! Node runtime and this module must reproduce `conformance/stale-guard-vectors.json` exactly.

use std::collections::HashSet;

use sha2::{Digest as _, Sha256};

/// The first 8 bytes of SHA-256 over one trimmed segment (with an optional bundle path prefix).
pub type SegmentDigest = [u8; 8];
/// A body's digest set: the union over all of its files.
pub type DigestSet = HashSet<SegmentDigest>;

/// Bytes after which a body is split. The split byte stays at the end of its segment.
const SPLIT: &[u8] = b">;}\n";
/// Bytes trimmed from both ends of each segment: space, tab, LF, CR, FF.
const TRIM: [u8; 5] = [b' ', b'\t', b'\n', b'\r', 0x0C];
/// A trimmed segment must have at least this many bytes to count.
pub const MIN_SEGMENT_BYTES: usize = 12;

fn trim(segment: &[u8]) -> &[u8] {
    let mut start = 0;
    let mut end = segment.len();
    while start < end && TRIM.contains(&segment[start]) {
        start += 1;
    }
    while end > start && TRIM.contains(&segment[end - 1]) {
        end -= 1;
    }
    &segment[start..end]
}

fn add_segment(path: Option<&str>, segment: &[u8], out: &mut DigestSet) {
    let trimmed = trim(segment);
    if trimmed.len() < MIN_SEGMENT_BYTES {
        return;
    }
    let mut hasher = Sha256::new();
    if let Some(path) = path {
        hasher.update(path.as_bytes());
        hasher.update([0_u8]);
    }
    hasher.update(trimmed);
    let full = hasher.finalize();
    let mut digest = [0_u8; 8];
    digest.copy_from_slice(&full[..8]);
    out.insert(digest);
}

/// Add the segment digests of one file to `out`. `path` is `None` for a single-file body and
/// the bundle-relative path for a bundle file.
pub fn add_file_digests(path: Option<&str>, data: &[u8], out: &mut DigestSet) {
    let mut start = 0;
    for (index, byte) in data.iter().enumerate() {
        if SPLIT.contains(byte) {
            add_segment(path, &data[start..=index], out);
            start = index + 1;
        }
    }
    if start < data.len() {
        add_segment(path, &data[start..], out);
    }
}

/// The digest set of a body given as `(path, bytes)` files.
pub fn digests<'a, I>(files: I) -> DigestSet
where
    I: IntoIterator<Item = (Option<&'a str>, &'a [u8])>,
{
    let mut out = DigestSet::new();
    for (path, data) in files {
        add_file_digests(path, data, &mut out);
    }
    out
}

/// Configurable thresholds (`STALE_GUARD_MIN_SEGMENTS`, `STALE_GUARD_MIN_SHARE`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Thresholds {
    pub min_segments: u64,
    pub min_share: f64,
}

impl Default for Thresholds {
    fn default() -> Self {
        Self {
            min_segments: 20,
            min_share: 0.10,
        }
    }
}

/// The outcome of one evaluation, with the same fields as the reference result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Evaluation {
    pub stale: bool,
    pub base: u64,
    pub newer_segments: u64,
    pub lost_segments: u64,
    pub message: Option<String>,
}

impl Evaluation {
    const fn not_stale(base: u64) -> Self {
        Self {
            stale: false,
            base,
            newer_segments: 0,
            lost_segments: 0,
            message: None,
        }
    }
}

fn range_text(base: u64, current: u64) -> String {
    if base + 1 == current {
        format!("revision {current}")
    } else {
        format!("revisions {}-{current}", base + 1)
    }
}

/// The fixed ADR-0023 text for a stale update.
#[must_use]
pub fn message(base: u64, current: u64) -> String {
    format!(
        "Update is based on revision {base}, but the artifact is at revision {current}. \
It would discard changes from {}. Read the current revision and apply your change to it, use \
restore_artifact to roll back on purpose, or pass discard_newer: true.",
        range_text(base, current)
    )
}

fn count(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

/// Find the base revision of `new` and decide whether the update is stale.
///
/// `history` holds the retained revisions whose bodies were readable (each `rev < current_rev`).
#[must_use]
pub fn evaluate(
    new: &DigestSet,
    current: &DigestSet,
    current_rev: u64,
    history: &[(u64, DigestSet)],
    thresholds: Thresholds,
) -> Evaluation {
    if new.is_empty() || current.is_empty() {
        return Evaluation::not_stale(current_rev);
    }
    let mut candidates: Vec<(u64, &DigestSet)> = history
        .iter()
        .filter(|(revision, _)| *revision != current_rev)
        .map(|(revision, set)| (*revision, set))
        .collect();
    candidates.push((current_rev, current));
    // Ascending, so a later (higher) revision wins a tie.
    candidates.sort_by_key(|(revision, _)| *revision);

    let mut best: Option<(u64, &DigestSet)> = None;
    let (mut best_i, mut best_u) = (0_u128, 1_u128);
    for (revision, set) in candidates {
        let intersection = u128::from(count(new.intersection(set).count()));
        let union = u128::from(count(new.len() + set.len())) - intersection;
        if best.is_none() || intersection * best_u >= best_i * union {
            best = Some((revision, set));
            best_i = intersection;
            best_u = union;
        }
    }
    let Some((base, base_set)) = best else {
        return Evaluation::not_stale(current_rev);
    };
    if base == current_rev {
        return Evaluation::not_stale(base);
    }
    let newer: DigestSet = current.difference(base_set).copied().collect();
    let lost = newer.iter().filter(|digest| !new.contains(*digest)).count();
    let (newer, lost) = (count(newer.len()), count(lost));
    #[allow(clippy::cast_precision_loss)]
    let stale =
        lost >= thresholds.min_segments && (lost as f64) >= thresholds.min_share * (newer as f64);
    Evaluation {
        stale,
        base,
        newer_segments: newer,
        lost_segments: lost,
        message: stale.then(|| message(base, current_rev)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn range_text_uses_singular_for_one_revision() {
        assert!(message(3, 4).contains("changes from revision 4."));
        assert!(message(1, 4).contains("changes from revisions 2-4."));
    }

    #[test]
    fn short_segments_and_trim_bytes_are_ignored() {
        assert!(digests([(None, b"  short;\n<p>x</p>".as_slice())]).is_empty());
        let one = digests([(None, b"\t <main>first page body</main>\r\n".as_slice())]);
        let two = digests([(None, b"<main>first page body</main>".as_slice())]);
        assert_eq!(one, two);
        assert_eq!(one.len(), 1);
    }
}
