//! Advisory action replies: the result shape every advisory worker must return.

use serde_json::Value;

use super::{
    ActionError,
    validation::{hash, keys, text, timestamp, validate_reply},
};

// Keep the same binary validation result for both fixed worker reply contracts.
pub fn validate_advisory_reply(v: &Value) -> Result<(), ActionError> {
    if v["schemaVersion"] != "org-intelligence/advisory-run/v1" {
        return Err(ActionError::Unavailable);
    }
    let mut converted = v.clone();
    converted["schemaVersion"] = Value::from("org-intelligence/action-run/v1");
    let history = converted["history"]
        .as_array()
        .ok_or(ActionError::Unavailable)?
        .clone();
    let original_runs = std::iter::once(&v["current"]).chain(
        v["history"]
            .as_array()
            .ok_or(ActionError::Unavailable)?
            .iter(),
    );
    for run in original_runs.filter(|r| !r.is_null()) {
        if run["action"] != "analyze-differences"
            || run.as_object().ok_or(ActionError::Unavailable)?.len() != 12
            || !run
                .as_object()
                .is_some_and(|object| object.contains_key("result"))
        {
            return Err(ActionError::Unavailable);
        }
        let result = &run["result"];
        if !result.is_null() {
            validate_advisory_result(result)?;
        }
    }
    let mut runs = history;
    if !converted["current"].is_null() {
        runs.push(converted["current"].clone());
    }
    for run in &mut runs {
        run.as_object_mut()
            .ok_or(ActionError::Unavailable)?
            .remove("result");
        run["action"] = Value::from("check-live-signals");
        if run["reason"] == "analysis_unavailable" {
            run["reason"] = Value::from("check_or_publication_failed");
        }
        if run["reason"] == "no_differences" {
            run["reason"] = Value::Null;
        }
        let stages = run["stages"]
            .as_array_mut()
            .ok_or(ActionError::Unavailable)?;
        if stages.len() != 4 {
            return Err(ActionError::Unavailable);
        }
        for ((stage, expected), replacement) in stages
            .iter_mut()
            .zip(["evidence", "specialists", "review", "save"])
            .zip(["validate", "jira", "salesforce", "publish"])
        {
            if stage["id"] != expected {
                return Err(ActionError::Unavailable);
            }
            stage["id"] = Value::from(replacement);
        }
    }
    if !converted["current"].is_null() {
        converted["current"] = runs.pop().ok_or(ActionError::Unavailable)?;
    }
    converted["history"] = Value::Array(runs);
    validate_reply(&converted)
}
pub(super) fn validate_advisory_result(r: &Value) -> Result<(), ActionError> {
    if !keys(
        r,
        &[
            "summary",
            "items",
            "engine",
            "model",
            "head",
            "checkedAt",
            "evidenceHash",
            "sources",
            "team",
        ],
    ) || !text(&r["summary"], 800)
        || !matches!(r["engine"].as_str(), Some("codex" | "claude"))
        || !text(&r["model"], 96)
        || !hash(&r["head"], 40)
        || !hash(&r["evidenceHash"], 64)
        || !timestamp(&r["checkedAt"], false)
    {
        return Err(ActionError::Unavailable);
    }
    let sources = r["sources"].as_array().ok_or(ActionError::Unavailable)?;
    if sources.len() > 54 {
        return Err(ActionError::Unavailable);
    }
    let mut ids = std::collections::HashSet::new();
    for value in sources {
        let p = value["path"].as_str().ok_or(ActionError::Unavailable)?;
        if !keys(value, &["id", "path", "line", "sha256"])
            || !text(&value["id"], 100)
            || !ids.insert(value["id"].as_str().ok_or(ActionError::Unavailable)?)
            || p.len() > 320
            || !(p.starts_with("JIRA/workspaces/") || p.starts_with("Projects/Active/"))
            || p.split('/').any(|part| part == ".." || part.is_empty())
            || p.contains('\\')
            || p.chars().any(char::is_control)
            || !value["line"]
                .as_u64()
                .is_some_and(|n| n > 0 && n <= 1000000)
            || !(value["sha256"].is_null() || hash(&value["sha256"], 64))
        {
            return Err(ActionError::Unavailable);
        }
    }
    let items = r["items"].as_array().ok_or(ActionError::Unavailable)?;
    if items.len() > 9 {
        return Err(ActionError::Unavailable);
    }
    for item in items {
        let refs = item["sourceIds"]
            .as_array()
            .ok_or(ActionError::Unavailable)?;
        if !keys(item, &["ticketId", "explanation", "nextStep", "sourceIds"])
            || !text(&item["ticketId"], 32)
            || !text(&item["explanation"], 600)
            || !text(&item["nextStep"], 400)
            || refs.is_empty()
            || refs.len() > 6
            || refs
                .iter()
                .any(|value| !value.as_str().is_some_and(|value| ids.contains(value)))
        {
            return Err(ActionError::Unavailable);
        }
    }
    let team = r["team"].as_array().ok_or(ActionError::Unavailable)?;
    if team.len() != 3 {
        return Err(ActionError::Unavailable);
    }
    for (entry, role) in team
        .iter()
        .zip(["ticket-manager", "org-analyst", "reflection-agent"])
    {
        if !keys(entry, &["role", "summary", "definitionHash"])
            || entry["role"] != role
            || !text(&entry["summary"], 800)
            || !hash(&entry["definitionHash"], 64)
        {
            return Err(ActionError::Unavailable);
        }
    }
    Ok(())
}
