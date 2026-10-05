//! Operator-reviewed, revision-pinned actions over a private loopback worker.
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{path::Path, time::Duration};

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
    for g in &grants {
        let u = url::Url::parse(&g.worker_url).map_err(|_| invalid())?;
        if !valid_name(&g.artifact_id)
            || !valid_name(&g.org)
            || g.revision == 0
            || !matches!(
                g.action.as_str(),
                "check-live-signals" | "analyze-differences"
            )
            || !seen.insert((&g.artifact_id, &g.action))
            || u.scheme() != "http"
            || u.host_str() != Some("127.0.0.1")
            || u.port().is_none()
            || u.path() != "/"
            || u.query().is_some()
            || u.fragment().is_some()
            || !u.username().is_empty()
            || u.password().is_some()
        {
            return Err(invalid());
        }
    }
    Ok(grants)
}

pub fn valid_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.bytes()
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
pub async fn dispatch(g: &ActionGrant, request_id: Option<&str>) -> Result<Value, ()> {
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(5))
        .build()
        .map_err(|_| ())?;
    let prefix = if g.action == "analyze-differences" {
        "advisory/"
    } else {
        ""
    };
    let request = if let Some(id) = request_id {
        client
            .post(format!("{}{prefix}start", g.worker_url))
            .json(&Start {
                request_id: id,
                artifact_id: &g.artifact_id,
                revision: g.revision,
                action: &g.action,
            })
    } else {
        client.get(format!("{}{prefix}latest", g.worker_url))
    };
    let mut response = request.send().await.map_err(|_| ())?;
    if !response.status().is_success() {
        return Err(());
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| ())? {
        if bytes.len() + chunk.len()
            > if g.action == "analyze-differences" {
                65536
            } else {
                16384
            }
        {
            return Err(());
        }
        bytes.extend_from_slice(&chunk);
    }
    let value: Value = serde_json::from_slice(&bytes).map_err(|_| ())?;
    if g.action == "analyze-differences" {
        validate_advisory_reply(&value)?;
        return Ok(value);
    }
    if !value.is_object()
        || value.get("schemaVersion").and_then(Value::as_str)
            != Some("org-intelligence/action-run/v1")
    {
        return Err(());
    }
    validate_reply(&value)?;
    Ok(value)
}

fn timestamp(v: &Value, nullable: bool) -> bool {
    if nullable && v.is_null() {
        return true;
    }
    v.as_str().is_some_and(|s| {
        s.len() <= 64
            && time::OffsetDateTime::parse(s, &time::format_description::well_known::Rfc3339)
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
#[allow(clippy::result_unit_err)]
pub fn validate_reply(v: &Value) -> Result<(), ()> {
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
        return Err(());
    }
    let history = v["history"].as_array().ok_or(())?;
    if history.len() > 5 {
        return Err(());
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
        ) || !r["id"].as_str().is_some_and(|s| {
            s.len() == 32
                && s.bytes()
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
                || r["captureRevision"].as_str().is_some_and(|s| {
                    s.len() == 64
                        && s.bytes()
                            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
                }))
        {
            return Err(());
        }
        let stages = r["stages"].as_array().ok_or(())?;
        if stages.len() != 4 {
            return Err(());
        }
        for (s, id) in stages
            .iter()
            .zip(["validate", "jira", "salesforce", "publish"])
        {
            if !keys(
                s,
                &["id", "state", "completed", "total", "successful", "failed"],
            ) || s["id"] != id
                || !matches!(
                    s["state"].as_str(),
                    Some("waiting" | "running" | "completed" | "failed" | "skipped")
                )
                || ["completed", "total", "successful", "failed"]
                    .iter()
                    .any(|k| !count(&s[*k]))
                || s["completed"].as_u64() > s["total"].as_u64()
                || s["successful"].as_u64().unwrap() + s["failed"].as_u64().unwrap()
                    != s["completed"].as_u64().unwrap()
            {
                return Err(());
            }
        }
    }
    Ok(())
}

fn text(v: &Value, max: usize) -> bool {
    v.as_str().is_some_and(|s| {
        !s.is_empty()
            && s.chars().count() <= max
            && !s.chars().any(|c| c.is_control() && c != '\n' && c != '\t')
    })
}
fn hash(v: &Value, size: usize) -> bool {
    v.as_str().is_some_and(|s| {
        s.len() == size
            && s.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    })
}
// Keep the same binary validation result for both fixed worker reply contracts.
#[allow(clippy::result_unit_err)]
pub fn validate_advisory_reply(v: &Value) -> Result<(), ()> {
    if v["schemaVersion"] != "org-intelligence/advisory-run/v1" {
        return Err(());
    }
    let mut converted = v.clone();
    converted["schemaVersion"] = Value::from("org-intelligence/action-run/v1");
    let history = converted["history"].as_array().ok_or(())?.clone();
    let original_runs =
        std::iter::once(&v["current"]).chain(v["history"].as_array().ok_or(())?.iter());
    for run in original_runs.filter(|r| !r.is_null()) {
        if run["action"] != "analyze-differences"
            || run.as_object().ok_or(())?.len() != 12
            || !run.as_object().unwrap().contains_key("result")
        {
            return Err(());
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
        run.as_object_mut().ok_or(())?.remove("result");
        run["action"] = Value::from("check-live-signals");
        if run["reason"] == "analysis_unavailable" {
            run["reason"] = Value::from("check_or_publication_failed");
        }
        if run["reason"] == "no_differences" {
            run["reason"] = Value::Null;
        }
        let stages = run["stages"].as_array_mut().ok_or(())?;
        if stages.len() != 4 {
            return Err(());
        }
        for ((stage, expected), replacement) in stages
            .iter_mut()
            .zip(["evidence", "specialists", "review", "save"])
            .zip(["validate", "jira", "salesforce", "publish"])
        {
            if stage["id"] != expected {
                return Err(());
            }
            stage["id"] = Value::from(replacement);
        }
    }
    if !converted["current"].is_null() {
        converted["current"] = runs.pop().ok_or(())?;
    }
    converted["history"] = Value::Array(runs);
    validate_reply(&converted)
}
fn validate_advisory_result(r: &Value) -> Result<(), ()> {
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
        return Err(());
    }
    let sources = r["sources"].as_array().ok_or(())?;
    if sources.len() > 54 {
        return Err(());
    }
    let mut ids = std::collections::HashSet::new();
    for s in sources {
        let p = s["path"].as_str().ok_or(())?;
        if !keys(s, &["id", "path", "line", "sha256"])
            || !text(&s["id"], 100)
            || !ids.insert(s["id"].as_str().unwrap())
            || p.len() > 320
            || !(p.starts_with("JIRA/workspaces/") || p.starts_with("Projects/Active/"))
            || p.split('/').any(|part| part == ".." || part.is_empty())
            || p.contains('\\')
            || p.chars().any(char::is_control)
            || !s["line"].as_u64().is_some_and(|n| n > 0 && n <= 1000000)
            || !(s["sha256"].is_null() || hash(&s["sha256"], 64))
        {
            return Err(());
        }
    }
    let items = r["items"].as_array().ok_or(())?;
    if items.len() > 9 {
        return Err(());
    }
    for item in items {
        let refs = item["sourceIds"].as_array().ok_or(())?;
        if !keys(item, &["ticketId", "explanation", "nextStep", "sourceIds"])
            || !text(&item["ticketId"], 32)
            || !text(&item["explanation"], 600)
            || !text(&item["nextStep"], 400)
            || refs.is_empty()
            || refs.len() > 6
            || refs
                .iter()
                .any(|s| !s.as_str().is_some_and(|s| ids.contains(s)))
        {
            return Err(());
        }
    }
    let team = r["team"].as_array().ok_or(())?;
    if team.len() != 3 {
        return Err(());
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
            return Err(());
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
        let mut g = serde_json::json!({"artifact_id":"abc123def456","org":"homelab","revision":6,"action":"check-live-signals","worker_url":"http://127.0.0.1:8766/"});
        std::fs::write(&path, serde_json::to_vec(&vec![&g]).unwrap()).unwrap();
        assert!(load_grants(path.to_str()).is_ok());
        for u in [
            "http://example.com:8766/",
            "http://127.0.0.1:8766/path",
            "http://127.0.0.1:8766/?q=x",
            "http://127.0.0.1:8766/#x",
            "http://user:password@127.0.0.1:8766/",
        ] {
            g["worker_url"] = serde_json::json!(u);
            std::fs::write(&path, serde_json::to_vec(&vec![&g]).unwrap()).unwrap();
            assert!(load_grants(path.to_str()).is_err());
        }
        std::fs::remove_file(path).unwrap();
    }
}
