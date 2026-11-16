use serde_json::Value;

use super::ActionError;

pub(super) fn timestamp(v: &Value, nullable: bool) -> bool {
    if nullable && v.is_null() {
        return true;
    }
    v.as_str().is_some_and(|value| {
        value.len() <= 64
            && time::OffsetDateTime::parse(value, &time::format_description::well_known::Rfc3339)
                .is_ok()
    })
}
pub(super) fn count(v: &Value) -> bool {
    v.as_u64().is_some_and(|n| n <= 120)
}
pub(super) fn keys(v: &Value, expected: &[&str]) -> bool {
    v.as_object()
        .is_some_and(|o| o.len() == expected.len() && expected.iter().all(|k| o.contains_key(*k)))
}
// The boundary exposes only valid/invalid, without details from an untrusted worker reply.
pub fn validate_reply(v: &Value) -> Result<(), ActionError> {
    if !keys(
        v,
        &[
            "schemaVersion",
            "current",
            "history",
            "availableAt",
            "workerCheckedAt",
        ],
    ) || !timestamp(&v["workerCheckedAt"], false)
        || !timestamp(&v["availableAt"], true)
    {
        return Err(ActionError::Unavailable);
    }
    let history = v["history"].as_array().ok_or(ActionError::Unavailable)?;
    if history.len() > 5 {
        return Err(ActionError::Unavailable);
    }
    let mut runs = history.iter().collect::<Vec<_>>();
    if !v["current"].is_null() {
        runs.push(&v["current"]);
    }
    for r in runs {
        if !keys(
            r,
            &[
                "id",
                "action",
                "state",
                "startedAt",
                "finishedAt",
                "reason",
                "successful",
                "failed",
                "checkedAt",
                "captureRevision",
                "stages",
            ],
        ) || !r["id"].as_str().is_some_and(|value| {
            value.len() == 32
                && value
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        }) || r["action"] != "check-live-signals"
            || !matches!(
                r["state"].as_str(),
                Some("queued" | "running" | "succeeded" | "partial" | "failed")
            )
            || !timestamp(&r["startedAt"], false)
            || !timestamp(&r["finishedAt"], true)
            || !timestamp(&r["checkedAt"], true)
            || !count(&r["successful"])
            || !count(&r["failed"])
            || !matches!(
                r["reason"].as_str(),
                None | Some("worker_restarted" | "check_or_publication_failed")
            )
            || !(r["reason"].is_null() || r["reason"].is_string())
            || !(r["captureRevision"].is_null()
                || r["captureRevision"].as_str().is_some_and(|value| {
                    value.len() == 64
                        && value
                            .bytes()
                            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
                }))
        {
            return Err(ActionError::Unavailable);
        }
        let stages = r["stages"].as_array().ok_or(ActionError::Unavailable)?;
        if stages.len() != 4 {
            return Err(ActionError::Unavailable);
        }
        for (value, id) in stages
            .iter()
            .zip(["validate", "jira", "salesforce", "publish"])
        {
            if !keys(
                value,
                &["id", "state", "completed", "total", "successful", "failed"],
            ) || value["id"] != id
                || !matches!(
                    value["state"].as_str(),
                    Some("waiting" | "running" | "completed" | "failed" | "skipped")
                )
                || ["completed", "total", "successful", "failed"]
                    .iter()
                    .any(|k| !count(&value[*k]))
                || value["completed"].as_u64() > value["total"].as_u64()
                || value["successful"]
                    .as_u64()
                    .zip(value["failed"].as_u64())
                    .map(|(successful, failed)| successful + failed)
                    != value["completed"].as_u64()
            {
                return Err(ActionError::Unavailable);
            }
        }
    }
    Ok(())
}

pub(super) fn text(v: &Value, max: usize) -> bool {
    v.as_str().is_some_and(|value| {
        !value.is_empty()
            && value.chars().count() <= max
            && !value
                .chars()
                .any(|c| c.is_control() && c != '\n' && c != '\t')
    })
}
pub(super) fn hash(v: &Value, size: usize) -> bool {
    v.as_str().is_some_and(|value| {
        value.len() == size
            && value
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    })
}
