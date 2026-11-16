//! Operator-reviewed, revision-pinned actions over a private loopback worker.

mod advisory;
mod investigation;
mod validation;

pub use advisory::validate_advisory_reply;
pub use investigation::validate_investigation_reply;
pub use validation::validate_reply;

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
