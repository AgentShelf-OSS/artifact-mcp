//! `ARTIFACT_CAST_IDS` parsing.

use std::collections::BTreeSet;

use crate::error::AppError;

use super::{ids::is_valid_artifact_id, parse::invalid};

pub(super) fn parse_cast_ids(raw: Option<&str>) -> Result<BTreeSet<(String, u64)>, AppError> {
    let mut ids = BTreeSet::new();
    let Some(raw) = raw else { return Ok(ids) };
    for grant in raw.split(',').map(str::trim) {
        let Some((id, revision)) = grant.split_once('@') else {
            return Err(invalid(
                "ARTIFACT_CAST_IDS",
                raw,
                "comma-separated artifactId@revision grants",
            ));
        };
        let parsed_revision = revision.parse::<u64>().ok().filter(|value| *value > 0);
        if !is_valid_artifact_id(id)
            || revision.is_empty()
            || revision.starts_with('0')
            || !revision.bytes().all(|b| b.is_ascii_digit())
            || parsed_revision.is_none()
            || parsed_revision.is_some_and(|value| value > 9_007_199_254_740_991)
        {
            return Err(invalid(
                "ARTIFACT_CAST_IDS",
                raw,
                "comma-separated artifactId@positiveRevision grants",
            ));
        }
        if let Some(parsed_revision) = parsed_revision {
            ids.insert((id.to_owned(), parsed_revision));
        }
    }
    Ok(ids)
}
