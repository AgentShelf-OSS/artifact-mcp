//! Operator-reviewed, revision-pinned actions over a private loopback worker.
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{path::Path, time::Duration};

/// Operator-reviewed allowlist of investigations the loopback worker supports.
const INVESTIGATION_ACTIONS: &[(&str, &str, &str, &str)] = &[
    (
        "investigate-OS-5082",
        "OS-5082",
        "investigation/OS-5082/latest",
        "investigation/OS-5082/start",
    ),
    (
        "investigate-SFD-686",
        "SFD-686",
        "investigation/SFD-686/latest",
        "investigation/SFD-686/start",
    ),
    (
        "investigate-SFD-703",
        "SFD-703",
        "investigation/SFD-703/latest",
        "investigation/SFD-703/start",
    ),
    (
        "investigate-SFD-709",
        "SFD-709",
        "investigation/SFD-709/latest",
        "investigation/SFD-709/start",
    ),
    (
        "investigate-SFD-724",
        "SFD-724",
        "investigation/SFD-724/latest",
        "investigation/SFD-724/start",
    ),
    (
        "investigate-SFD-757",
        "SFD-757",
        "investigation/SFD-757/latest",
        "investigation/SFD-757/start",
    ),
    (
        "investigate-SFD-842",
        "SFD-842",
        "investigation/SFD-842/latest",
        "investigation/SFD-842/start",
    ),
    (
        "investigate-SFD-887",
        "SFD-887",
        "investigation/SFD-887/latest",
        "investigation/SFD-887/start",
    ),
    (
        "investigate-SFD-978",
        "SFD-978",
        "investigation/SFD-978/latest",
        "investigation/SFD-978/start",
    ),
];

fn investigation_action(action: &str) -> Option<(&'static str, String, String)> {
    INVESTIGATION_ACTIONS
        .iter()
        .find_map(|(name, ticket, latest, start)| {
            (*name == action).then(|| (*ticket, (*latest).to_owned(), (*start).to_owned()))
        })
}

#[derive(Debug, thiserror::Error)]
pub enum ActionError {
    #[error("action unavailable")]
    Unavailable,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActionGrant {
    pub artifact_id: String,
    pub org: String,
    pub revision: u64,
    pub action: String,
    pub worker_url: String,
}

pub fn load_grants(path: Option<&str>) -> Result<Vec<ActionGrant>, crate::error::AppError> {
    let invalid = || crate::error::AppError::Validation("Invalid action grants".into());
    let Some(path) = path else {
        return Ok(vec![]);
    };
    let meta = std::fs::symlink_metadata(Path::new(path)).map_err(|_| invalid())?;
    if !meta.is_file() || meta.len() > 16384 {
        return Err(invalid());
    }
    let grants: Vec<ActionGrant> =
        serde_json::from_slice(&std::fs::read(path).map_err(|_| invalid())?)
            .map_err(|_| invalid())?;
    if grants.len() > 16 {
        return Err(invalid());
    }
    let mut seen = std::collections::HashSet::new();
    for grant in &grants {
        let worker_url = url::Url::parse(&grant.worker_url).map_err(|_| invalid())?;
        if !valid_name(&grant.artifact_id)
            || !valid_name(&grant.org)
            || grant.revision == 0
            || (!matches!(
                grant.action.as_str(),
                "check-live-signals" | "analyze-differences"
            ) && investigation_action(&grant.action).is_none())
            || !seen.insert((&grant.artifact_id, &grant.action))
            || worker_url.scheme() != "http"
            || worker_url.host_str() != Some("127.0.0.1")
            || worker_url.port().is_none()
            || worker_url.path() != "/"
            || worker_url.query().is_some()
            || worker_url.fragment().is_some()
            || !worker_url.username().is_empty()
            || worker_url.password().is_some()
        {
            return Err(invalid());
        }
    }
    Ok(grants)
}

pub fn valid_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"._-".contains(&c))
}

#[derive(Serialize)]
struct Start<'a> {
    request_id: &'a str,
    artifact_id: &'a str,
    revision: u64,
    action: &'a str,
}

/// The worker returns only bounded public run state. Never relay upstream error text.
pub async fn dispatch(grant: &ActionGrant, request_id: Option<&str>) -> Result<Value, ActionError> {
    static CLIENT: std::sync::OnceLock<Option<reqwest::Client>> = std::sync::OnceLock::new();
    let client = CLIENT
        .get_or_init(|| {
            reqwest::Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(Duration::from_secs(5))
                .build()
                .map_err(|error| tracing::warn!(%error, "action client initialization failed"))
                .ok()
        })
        .as_ref()
        .ok_or(ActionError::Unavailable)?;
    let (latest_path, start_path, max_bytes) = if grant.action == "analyze-differences" {
        (
            "advisory/latest".to_owned(),
            "advisory/start".to_owned(),
            65536usize,
        )
    } else if let Some((_, latest, start)) = investigation_action(&grant.action) {
        (latest, start, 65536usize)
    } else {
        ("latest".to_owned(), "start".to_owned(), 16384usize)
    };
    let request = if let Some(id) = request_id {
        client
            .post(format!("{}{}", grant.worker_url, start_path))
            .json(&Start {
                request_id: id,
                artifact_id: &grant.artifact_id,
                revision: grant.revision,
                action: &grant.action,
            })
    } else {
        client.get(format!("{}{}", grant.worker_url, latest_path))
    };
    let mut response = request.send().await.map_err(|error| {
        tracing::warn!(%error, "action worker request failed");
        ActionError::Unavailable
    })?;
    if !response.status().is_success() {
        return Err(ActionError::Unavailable);
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|error| {
        tracing::warn!(%error, "action worker response failed");
        ActionError::Unavailable
    })? {
        if bytes.len() + chunk.len() > max_bytes {
            return Err(ActionError::Unavailable);
        }
        bytes.extend_from_slice(&chunk);
    }
    let value: Value = serde_json::from_slice(&bytes).map_err(|error| {
        tracing::warn!(%error, "action worker reply was invalid JSON");
        ActionError::Unavailable
    })?;
    if grant.action == "analyze-differences" {
        validate_advisory_reply(&value)?;
        return Ok(value);
    }
    if let Some((ticket, _, _)) = investigation_action(&grant.action) {
        validate_investigation_reply(&value, ticket, &grant.action)?;
        return Ok(value);
    }
    if !value.is_object()
        || value.get("schemaVersion").and_then(Value::as_str)
            != Some("org-intelligence/action-run/v1")
    {
        return Err(ActionError::Unavailable);
    }
    validate_reply(&value)?;
    Ok(value)
}

fn timestamp(v: &Value, nullable: bool) -> bool {
    if nullable && v.is_null() {
        return true;
    }
    v.as_str().is_some_and(|value| {
        value.len() <= 64
            && time::OffsetDateTime::parse(value, &time::format_description::well_known::Rfc3339)
                .is_ok()
    })
}
fn count(v: &Value) -> bool {
    v.as_u64().is_some_and(|n| n <= 120)
}
fn keys(v: &Value, expected: &[&str]) -> bool {
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

fn text(v: &Value, max: usize) -> bool {
    v.as_str().is_some_and(|value| {
        !value.is_empty()
            && value.chars().count() <= max
            && !value
                .chars()
                .any(|c| c.is_control() && c != '\n' && c != '\t')
    })
}
fn hash(v: &Value, size: usize) -> bool {
    v.as_str().is_some_and(|value| {
        value.len() == size
            && value
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    })
}
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
fn validate_advisory_result(r: &Value) -> Result<(), ActionError> {
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

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn advisory_reply_rejects_unsafe_fields_sources_and_result_types() {
        let result = serde_json::json!({"summary":"Review the status difference.","items":[{"ticketId":"SFD-703","explanation":"Saved and live status differ.","nextStep":"Review the selected work.","sourceIds":["workspace:SFD-703"]}],"engine":"codex","model":"gpt-6.1-sol","head":"a".repeat(40),"checkedAt":"2026-10-05T02:00:00Z","evidenceHash":"b".repeat(64),"sources":[{"id":"workspace:SFD-703","path":"JIRA/workspaces/SFD/SFD-703/workspace.json","line":1,"sha256":null}],"team":[{"role":"ticket-manager","summary":"Jira review.","definitionHash":"c".repeat(64)},{"role":"org-analyst","summary":"Metadata review.","definitionHash":"c".repeat(64)},{"role":"reflection-agent","summary":"Evidence review.","definitionHash":"c".repeat(64)}]});
        let value = serde_json::json!({"schemaVersion":"org-intelligence/advisory-run/v1","history":[],"availableAt":"2026-10-05T02:05:00Z","workerCheckedAt":"2026-10-05T02:00:00Z","current":{"id":"d".repeat(32),"action":"analyze-differences","state":"succeeded","startedAt":"2026-10-05T02:00:00Z","finishedAt":"2026-10-05T02:00:00Z","reason":null,"successful":3,"failed":0,"checkedAt":"2026-10-05T02:00:00Z","captureRevision":"e".repeat(64),"stages":[{"id":"evidence","state":"completed","completed":1,"total":1,"successful":1,"failed":0},{"id":"specialists","state":"completed","completed":2,"total":2,"successful":2,"failed":0},{"id":"review","state":"completed","completed":1,"total":1,"successful":1,"failed":0},{"id":"save","state":"completed","completed":1,"total":1,"successful":1,"failed":0}],"result":result}});
        assert!(validate_advisory_reply(&value).is_ok());
        let mut bad = value.clone();
        bad["current"]["result"]["command"] = Value::from("run");
        assert!(validate_advisory_reply(&bad).is_err());
        let mut bad = value.clone();
        bad["current"]["result"]["sources"][0]["path"] =
            Value::from("JIRA/workspaces/../../secret");
        assert!(validate_advisory_reply(&bad).is_err());
        let mut bad = value.clone();
        bad["current"]["result"]["items"][0]["sourceIds"][0] = Value::from("invented");
        assert!(validate_advisory_reply(&bad).is_err());
        let mut bad = value.clone();
        bad["current"]["result"]["summary"] = Value::from("a\u{0000}b");
        assert!(validate_advisory_reply(&bad).is_err());
        let mut bad = value.clone();
        bad["current"]["result"]["engine"] = Value::from("arbitrary");
        assert!(validate_advisory_reply(&bad).is_err());
        let mut bad = value.clone();
        bad["current"]["stages"][0]["id"] = Value::from("arbitrary");
        assert!(validate_advisory_reply(&bad).is_err());
        let mut retained = value.clone();
        retained["history"] = serde_json::json!([value["current"].clone()]);
        assert!(validate_advisory_reply(&retained).is_ok());
    }
    #[test]
    fn unsafe_worker_reply_is_rejected() {
        let mut value = serde_json::json!({"schemaVersion":"org-intelligence/action-run/v1","current":null,"history":[],"availableAt":null,"workerCheckedAt":"2026-10-05T02:00:00Z"});
        assert!(validate_reply(&value).is_ok());
        value["secret"] = serde_json::json!("private");
        assert!(validate_reply(&value).is_err());
    }
    #[test]
    fn worker_grants_refuse_remote_urls_and_non_fixed_actions() {
        let path = std::env::temp_dir().join(format!("action-grants-{}.json", nanoid::nanoid!()));
        let mut grant = serde_json::json!({"artifact_id":"abc123def456","org":"homelab","revision":6,"action":"check-live-signals","worker_url":"http://127.0.0.1:8766/"});
        std::fs::write(&path, serde_json::to_vec(&vec![&grant]).unwrap()).unwrap();
        assert!(load_grants(path.to_str()).is_ok());
        let grants: Vec<Value> = ["check-live-signals", "analyze-differences"]
            .into_iter()
            .chain([
                "investigate-OS-5082",
                "investigate-SFD-686",
                "investigate-SFD-703",
                "investigate-SFD-709",
                "investigate-SFD-724",
                "investigate-SFD-757",
                "investigate-SFD-842",
                "investigate-SFD-887",
                "investigate-SFD-978",
            ])
            .map(|action| {
                let mut grant = grant.clone();
                grant["action"] = serde_json::json!(action);
                grant
            })
            .collect();
        std::fs::write(&path, serde_json::to_vec(&grants).unwrap()).unwrap();
        assert_eq!(load_grants(path.to_str()).unwrap().len(), 11);
        for action in ["investigate-SFD-703/../start", "arbitrary"] {
            let mut grant = grant.clone();
            grant["action"] = serde_json::json!(action);
            std::fs::write(&path, serde_json::to_vec(&vec![grant]).unwrap()).unwrap();
            assert!(load_grants(path.to_str()).is_err());
        }
        for worker_url in [
            "http://example.com:8766/",
            "http://127.0.0.1:8766/path",
            "http://127.0.0.1:8766/?q=x",
            "http://127.0.0.1:8766/#x",
            "http://user:password@127.0.0.1:8766/",
        ] {
            grant["worker_url"] = serde_json::json!(worker_url);
            std::fs::write(&path, serde_json::to_vec(&vec![&grant]).unwrap()).unwrap();
            assert!(load_grants(path.to_str()).is_err());
        }
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn investigation_actions_use_literal_ticket_paths() {
        for ticket in [
            "OS-5082", "SFD-686", "SFD-703", "SFD-709", "SFD-724", "SFD-757", "SFD-842", "SFD-887",
            "SFD-978",
        ] {
            let action = format!("investigate-{ticket}");
            assert_eq!(
                investigation_action(&action),
                Some((
                    ticket,
                    format!("investigation/{ticket}/latest"),
                    format!("investigation/{ticket}/start")
                ))
            );
        }
        assert!(investigation_action("investigate-SFD-999").is_none());
        assert!(investigation_action("investigate-").is_none());
        assert!(investigation_action(&format!("investigate-{}", "a".repeat(33))).is_none());
        assert!(investigation_action("investigate-../start").is_none());
    }

    #[test]
    fn investigation_reply_requires_ticket_owned_proposal_evidence() {
        let source = serde_json::json!({
            "id":"workspace:SFD-703",
            "path":"JIRA/workspaces/SFD/SFD-703/workspace.json",
            "line":1,
            "sha256":null
        });
        let result = serde_json::json!({
            "summary":"Review the current ticket evidence.",
            "items":[{"ticketId":"SFD-703","explanation":"The saved and observed records differ.","nextStep":"Review the proposed correction.","sourceIds":["workspace:SFD-703"]}],
            "engine":"codex","model":"gpt-6.1-sol","head":"a".repeat(40),
            "checkedAt":"2026-10-05T02:00:00Z","evidenceHash":"b".repeat(64),
            "sources":[source],
            "team":[
                {"role":"ticket-manager","summary":"Jira review.","definitionHash":"c".repeat(64)},
                {"role":"org-analyst","summary":"Metadata review.","definitionHash":"c".repeat(64)},
                {"role":"reflection-agent","summary":"Evidence review.","definitionHash":"c".repeat(64)}
            ],
            "proposal":{"outcome":"needs_input","changes":[{"target":"workspace_record","path":"JIRA/workspaces/SFD/SFD-703/workspace.json","current":"Saved value","proposed":"Review value","reason":"The records differ.","sourceIds":["workspace:SFD-703"]}],"questions":["Which record is authoritative?"],"limitations":["No write was performed."]}
        });
        let run = serde_json::json!({
            "id":"d".repeat(32),"action":"investigate-SFD-703","state":"succeeded",
            "startedAt":"2026-10-05T02:00:00Z","finishedAt":"2026-10-05T02:00:00Z","reason":null,
            "successful":3,"failed":0,"checkedAt":"2026-10-05T02:00:00Z","captureRevision":"e".repeat(64),
            "stages":[
                {"id":"evidence","state":"completed","completed":1,"total":1,"successful":1,"failed":0},
                {"id":"specialists","state":"completed","completed":2,"total":2,"successful":2,"failed":0},
                {"id":"review","state":"completed","completed":1,"total":1,"successful":1,"failed":0},
                {"id":"save","state":"completed","completed":1,"total":1,"successful":1,"failed":0}
            ],
            "result":result,"ticketId":"SFD-703","originReviewId":"f".repeat(32),
            "originHead":"a".repeat(40),"originEvidenceHash":"b".repeat(64)
        });
        let reply = serde_json::json!({"schemaVersion":"org-intelligence/investigation-run/v1","ticketId":"SFD-703","current":null,"latest":run,"history":[],"availableAt":null,"workerCheckedAt":"2026-10-05T02:00:00Z","teamBusy":false});
        assert!(validate_investigation_reply(&reply, "SFD-703", "investigate-SFD-703").is_ok());
        let mut compact = reply.clone();
        compact["current"] = compact["latest"].clone();
        compact["current"]["result"] = Value::Null;
        compact["history"] = serde_json::json!([compact["current"].clone()]);
        assert!(validate_investigation_reply(&compact, "SFD-703", "investigate-SFD-703").is_ok());
        let mut wrong_action = reply.clone();
        wrong_action["latest"]["action"] = Value::from("investigate-SFD-842");
        assert!(
            validate_investigation_reply(&wrong_action, "SFD-703", "investigate-SFD-703").is_err()
        );
        let mut missing_origin = reply.clone();
        missing_origin["latest"]["originReviewId"] = Value::Null;
        assert!(
            validate_investigation_reply(&missing_origin, "SFD-703", "investigate-SFD-703")
                .is_err()
        );
        let mut wrong_source = reply.clone();
        wrong_source["latest"]["result"]["sources"][0]["path"] =
            Value::from("JIRA/workspaces/SFD/SFD-842/workspace.json");
        assert!(
            validate_investigation_reply(&wrong_source, "SFD-703", "investigate-SFD-703").is_err()
        );
        let mut wrong_path = reply.clone();
        wrong_path["latest"]["result"]["proposal"]["changes"][0]["path"] =
            Value::from("Projects/Active/Other/README.md");
        assert!(
            validate_investigation_reply(&wrong_path, "SFD-703", "investigate-SFD-703").is_err()
        );
        let mut wrong = reply.clone();
        wrong["latest"]["ticketId"] = Value::from("SFD-842");
        assert!(validate_investigation_reply(&wrong, "SFD-703", "investigate-SFD-703").is_err());
        let mut extra = reply;
        extra["latest"]["result"]["proposal"]["command"] = Value::from("write");
        assert!(validate_investigation_reply(&extra, "SFD-703", "investigate-SFD-703").is_err());
    }
}
