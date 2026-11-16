//! Investigation action replies: the advisory result plus run metadata, source IDs and ticket paths.

use serde_json::Value;

use super::{
    ActionError,
    advisory::validate_advisory_result,
    validation::{count, hash, keys, text, timestamp},
};

fn investigation_run_keys() -> [&'static str; 16] {
    [
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
        "result",
        "ticketId",
        "originReviewId",
        "originHead",
        "originEvidenceHash",
    ]
}

fn safe_ticket_path(path: &str, ticket: &str) -> bool {
    let parts: Vec<&str> = path.split('/').collect();
    if parts.iter().any(|part| part.is_empty() || *part == "..") || path.contains('\\') {
        return false;
    }
    let ticket = ticket.to_ascii_uppercase();
    if parts.len() >= 4
        && parts[0] == "JIRA"
        && parts[1] == "workspaces"
        && parts[3].eq_ignore_ascii_case(&ticket)
    {
        return true;
    }
    path.starts_with("Projects/Active/")
}

fn validate_investigation_source_ids(result: &Value, ticket: &str) -> Result<(), ActionError> {
    let sources = result["sources"]
        .as_array()
        .ok_or(ActionError::Unavailable)?;
    let mut ids = std::collections::HashSet::new();
    for source in sources {
        if !source.is_object() || !keys(source, &["id", "path", "line", "sha256"]) {
            return Err(ActionError::Unavailable);
        }
        let id = source["id"].as_str().ok_or(ActionError::Unavailable)?;
        let path = source["path"].as_str().ok_or(ActionError::Unavailable)?;
        if !ids.insert(id)
            || !text(&source["id"], 100)
            || path.len() > 320
            || path.chars().any(|c| c.is_control())
            || !safe_ticket_path(path, ticket)
        {
            return Err(ActionError::Unavailable);
        }
    }
    let item = &result["items"][0];
    let refs = item["sourceIds"]
        .as_array()
        .ok_or(ActionError::Unavailable)?;
    if refs
        .iter()
        .any(|id| !id.as_str().is_some_and(|id| ids.contains(id)))
    {
        return Err(ActionError::Unavailable);
    }
    let proposal = &result["proposal"];
    let changes = proposal["changes"]
        .as_array()
        .ok_or(ActionError::Unavailable)?;
    for change in changes {
        let refs = change["sourceIds"]
            .as_array()
            .ok_or(ActionError::Unavailable)?;
        if refs
            .iter()
            .any(|id| !id.as_str().is_some_and(|id| ids.contains(id)))
            || !refs.iter().any(|id| {
                id.as_str().is_some_and(|id| {
                    sources
                        .iter()
                        .any(|source| source["id"] == id && source["path"] == change["path"])
                })
            })
        {
            return Err(ActionError::Unavailable);
        }
    }
    Ok(())
}

fn validate_investigation_result(result: &Value, ticket: &str) -> Result<(), ActionError> {
    let object = result.as_object().ok_or(ActionError::Unavailable)?;
    if object.len() != 10 || !object.contains_key("proposal") {
        return Err(ActionError::Unavailable);
    }
    let mut advisory = result.clone();
    advisory
        .as_object_mut()
        .ok_or(ActionError::Unavailable)?
        .remove("proposal");
    validate_advisory_result(&advisory)?;
    let items = advisory["items"]
        .as_array()
        .ok_or(ActionError::Unavailable)?;
    if items.len() != 1 || items[0]["ticketId"] != ticket {
        return Err(ActionError::Unavailable);
    }
    let proposal = &result["proposal"];
    if !keys(
        proposal,
        &["outcome", "changes", "questions", "limitations"],
    ) || !matches!(
        proposal["outcome"].as_str(),
        Some("proposed_correction" | "expected_difference" | "needs_input")
    ) {
        return Err(ActionError::Unavailable);
    }
    let changes = proposal["changes"]
        .as_array()
        .ok_or(ActionError::Unavailable)?;
    if changes.len() > 4 {
        return Err(ActionError::Unavailable);
    }
    for change in changes {
        if !keys(
            change,
            &[
                "target",
                "path",
                "current",
                "proposed",
                "reason",
                "sourceIds",
            ],
        ) || !matches!(
            change["target"].as_str(),
            Some("workspace_record" | "jira_status" | "baseline_review")
        ) || !text(&change["path"], 320)
            || !safe_ticket_path(
                change["path"].as_str().ok_or(ActionError::Unavailable)?,
                ticket,
            )
            || !text(&change["current"], 400)
            || !text(&change["proposed"], 600)
            || !text(&change["reason"], 400)
        {
            return Err(ActionError::Unavailable);
        }
        let refs = change["sourceIds"]
            .as_array()
            .ok_or(ActionError::Unavailable)?;
        if refs.is_empty() || refs.len() > 6 {
            return Err(ActionError::Unavailable);
        }
    }
    let questions = proposal["questions"]
        .as_array()
        .ok_or(ActionError::Unavailable)?;
    if questions.len() > 6 || questions.iter().any(|q| !text(q, 400)) {
        return Err(ActionError::Unavailable);
    }
    let limitations = proposal["limitations"]
        .as_array()
        .ok_or(ActionError::Unavailable)?;
    if limitations.is_empty() || limitations.len() > 6 || limitations.iter().any(|q| !text(q, 300))
    {
        return Err(ActionError::Unavailable);
    }
    validate_investigation_source_ids(result, ticket)
}

fn validate_investigation_run(run: &Value, ticket: &str, action: &str) -> Result<(), ActionError> {
    if !keys(run, &investigation_run_keys())
        || run["action"] != action
        || run["ticketId"] != ticket
        || !hash(&run["id"], 32)
        || !timestamp(&run["startedAt"], false)
        || !timestamp(&run["finishedAt"], true)
        || !timestamp(&run["checkedAt"], true)
        || !count(&run["successful"])
        || !count(&run["failed"])
        || !matches!(
            run["state"].as_str(),
            Some("queued" | "running" | "succeeded" | "failed")
        )
        || !matches!(
            run["reason"].as_str(),
            None | Some("worker_restarted" | "investigation_unavailable" | "no_current_difference")
        )
        || !(run["reason"].is_null() || run["reason"].is_string())
        || !(run["captureRevision"].is_null() || hash(&run["captureRevision"], 64))
        || !(run["originReviewId"].is_null() || hash(&run["originReviewId"], 32))
        || !(run["originHead"].is_null() || hash(&run["originHead"], 40))
        || !(run["originEvidenceHash"].is_null() || hash(&run["originEvidenceHash"], 64))
    {
        return Err(ActionError::Unavailable);
    }
    let stages = run["stages"].as_array().ok_or(ActionError::Unavailable)?;
    if stages.len() != 4 {
        return Err(ActionError::Unavailable);
    }
    for (stage, expected) in stages
        .iter()
        .zip(["evidence", "specialists", "review", "save"])
    {
        if !keys(
            stage,
            &["id", "state", "completed", "total", "successful", "failed"],
        ) || stage["id"] != expected
            || !matches!(
                stage["state"].as_str(),
                Some("waiting" | "running" | "completed" | "failed" | "skipped")
            )
            || ["completed", "total", "successful", "failed"]
                .iter()
                .any(|k| !count(&stage[*k]))
            || stage["completed"].as_u64() > stage["total"].as_u64()
            || stage["successful"]
                .as_u64()
                .zip(stage["failed"].as_u64())
                .map(|(successful, failed)| successful + failed)
                != stage["completed"].as_u64()
        {
            return Err(ActionError::Unavailable);
        }
    }
    if let Some(result) = (!run["result"].is_null()).then_some(&run["result"]) {
        validate_investigation_result(result, ticket)?;
        if run["originReviewId"].is_null()
            || run["originHead"].is_null()
            || run["originEvidenceHash"].is_null()
        {
            return Err(ActionError::Unavailable);
        }
    }
    if run["action"] != action || action != format!("investigate-{ticket}") {
        return Err(ActionError::Unavailable);
    }
    Ok(())
}

// Keep the same binary validation result for the fixed investigation reply contract.
pub fn validate_investigation_reply(
    v: &Value,
    ticket: &str,
    action: &str,
) -> Result<(), ActionError> {
    if !keys(
        v,
        &[
            "schemaVersion",
            "ticketId",
            "current",
            "latest",
            "history",
            "availableAt",
            "workerCheckedAt",
            "teamBusy",
        ],
    ) || v["schemaVersion"] != "org-intelligence/investigation-run/v1"
        || v["ticketId"] != ticket
        || !timestamp(&v["availableAt"], true)
        || !timestamp(&v["workerCheckedAt"], false)
        || !v["teamBusy"].is_boolean()
    {
        return Err(ActionError::Unavailable);
    }
    let history = v["history"].as_array().ok_or(ActionError::Unavailable)?;
    if history.len() > 5 {
        return Err(ActionError::Unavailable);
    }
    if !v["current"].is_null() {
        validate_investigation_run(&v["current"], ticket, action)?;
    }
    for run in history {
        validate_investigation_run(run, ticket, action)?;
    }
    if !v["latest"].is_null() {
        validate_investigation_run(&v["latest"], ticket, action)?;
        if v["latest"]["state"] != "succeeded" || v["latest"]["result"].is_null() {
            return Err(ActionError::Unavailable);
        }
    }
    Ok(())
}
