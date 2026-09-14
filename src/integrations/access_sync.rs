//! Reconciles the durable organization email membership set with one owned
//! Cloudflare Access policy.
//!
//! The policy is deliberately configured by id.  This adapter never searches for,
//! creates, or changes a different policy.

use std::{
    collections::BTreeSet,
    sync::{Arc, Mutex, RwLock, Weak},
    time::Duration,
};

use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use tokio::{task::JoinHandle, time};

use crate::{
    config::Secret,
    error::AppError,
    persistence::db::{self, DbPool},
};

const CLOUDFLARE_API: &str = "https://api.cloudflare.com/client/v4";
const RUN_INTERVAL: Duration = Duration::from_secs(60);
const HTTP_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_RESPONSE_BYTES: usize = 1024 * 1024;
const OWNED_POLICY_NAME: &str = "Artifact member emails";

/// Configuration for the single Cloudflare policy owned by this application.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AccessSyncConfig {
    pub account_id: String,
    pub application_id: String,
    pub policy_id: String,
    pub hostname: String,
    pub api_token: Secret,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SyncStatus {
    pub state: String,
    pub message: String,
}

impl Default for SyncStatus {
    fn default() -> Self {
        Self {
            state: "disabled".to_owned(),
            message: String::new(),
        }
    }
}

#[derive(Clone)]
pub struct AccessSync {
    inner: Arc<Inner>,
}

struct Inner {
    config: Option<AccessSyncConfig>,
    pool: DbPool,
    client: Client,
    status: RwLock<SyncStatus>,
    wake: tokio::sync::mpsc::Sender<()>,
    generation: std::sync::atomic::AtomicU64,
    task: Mutex<Option<JoinHandle<()>>>,
}

impl Drop for Inner {
    fn drop(&mut self) {
        if let Ok(mut task) = self.task.lock()
            && let Some(task) = task.take()
        {
            task.abort();
        }
    }
}

impl AccessSync {
    /// Start the reconciliation worker. `None` intentionally leaves the feature disabled.
    pub fn start(config: Option<AccessSyncConfig>, pool: DbPool) -> Result<Self, AppError> {
        let Some(config) = config else {
            return Ok(Self::disabled(pool));
        };
        validate_config(&config)?;
        let client = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(HTTP_TIMEOUT)
            .build()
            .map_err(|_| AppError::Unavailable("access sync HTTP client unavailable".to_owned()))?;
        Self::start_with_client(
            config,
            pool,
            client,
            CLOUDFLARE_API.to_owned(),
            RUN_INTERVAL,
        )
    }

    fn start_with_client(
        config: AccessSyncConfig,
        pool: DbPool,
        client: Client,
        base: String,
        period: Duration,
    ) -> Result<Self, AppError> {
        let (wake, receiver) = tokio::sync::mpsc::channel(1);
        let inner = Arc::new(Inner {
            config: Some(config),
            pool,
            client,
            status: RwLock::new(SyncStatus {
                state: "pending".to_owned(),
                message: String::new(),
            }),
            wake,
            generation: std::sync::atomic::AtomicU64::new(0),
            task: Mutex::new(None),
        });
        let weak = Arc::downgrade(&inner);
        let task = tokio::spawn(worker(weak, base, receiver, period));
        *inner.task.lock().map_err(|_| AppError::Internal)? = Some(task);
        Ok(Self { inner })
    }

    fn disabled(pool: DbPool) -> Self {
        let inner = Arc::new(Inner {
            config: None,
            pool,
            client: Client::new(),
            status: RwLock::new(SyncStatus::default()),
            wake: tokio::sync::mpsc::channel(1).0,
            generation: std::sync::atomic::AtomicU64::new(0),
            task: Mutex::new(None),
        });
        Self { inner }
    }

    /// Current operational state, safe to expose to a health/status route.
    #[must_use]
    pub fn status(&self) -> SyncStatus {
        self.inner
            .status
            .read()
            .map(|status| status.clone())
            .unwrap_or_else(|_| SyncStatus {
                state: "error".to_owned(),
                message: "status unavailable".to_owned(),
            })
    }

    /// Request an immediate reconciliation. Calls are coalesced by the worker.
    pub fn wake(&self) {
        if self.inner.config.is_none() {
            return;
        }
        if let Ok(mut status) = self.inner.status.write() {
            self.inner
                .generation
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            status.state = "pending".to_owned();
            status.message = "Explicit email changes are queued for Cloudflare.".to_owned();
        }
        let _ = self.inner.wake.try_send(());
    }
}

async fn worker(
    weak: Weak<Inner>,
    base: String,
    mut wake: tokio::sync::mpsc::Receiver<()>,
    period: Duration,
) {
    let mut interval = time::interval(period);
    interval.set_missed_tick_behavior(time::MissedTickBehavior::Delay);
    interval.tick().await; // Consume the immediate first tick; reconcile once at startup.
    loop {
        while wake.try_recv().is_ok() {}
        let Some(inner) = weak.upgrade() else { return };
        let _ = reconcile(&inner, &base).await;
        drop(inner);
        tokio::select! { _ = interval.tick() => {}, _ = wake.recv() => {} }
    }
}

async fn reconcile(inner: &Inner, base: &str) -> Result<(), AppError> {
    let generation = inner.generation.load(std::sync::atomic::Ordering::Relaxed);
    let result = async {
        let desired = db::interact(&inner.pool, |conn| {
            let mut statement = conn
                .prepare("SELECT email FROM org_email_members")
                .map_err(|_| AppError::Internal)?;
            let rows = statement
                .query_map([], |row| row.get::<_, String>(0))
                .map_err(|_| AppError::Internal)?;
            let mut emails = BTreeSet::new();
            for row in rows {
                let email = row.map_err(|_| AppError::Internal)?.trim().to_lowercase();
                if !email.is_empty() {
                    emails.insert(email);
                }
            }
            Ok(emails)
        })
        .await?;
        let config = inner.config.as_ref().ok_or(AppError::Internal)?;
        reconcile_http(&inner.client, config, base, &desired).await?;
        Ok::<usize, AppError>(desired.len())
    }
    .await;
    // Share the status lock with wake(), so an old result cannot overwrite a newer wake.
    if let Ok(mut status) = inner.status.write() {
        if inner.generation.load(std::sync::atomic::Ordering::Relaxed) != generation {
            *status = SyncStatus {
                state: "pending".into(),
                message: "Membership changed during synchronization; another update is queued."
                    .into(),
            };
        } else {
            *status = match &result {
                Ok(count) => SyncStatus {
                    state: "synced".into(),
                    message: format!(
                        "Cloudflare policy verified for {count} explicit email addresses."
                    ),
                },
                Err(error) => SyncStatus {
                    state: "error".into(),
                    message: format!(
                        "{}; retrying automatically within 60 seconds.",
                        safe_error_message(error)
                    ),
                },
            };
        }
    }
    result.map(|_| ())
}

fn safe_error_message(error: &AppError) -> String {
    match error {
        AppError::Unavailable(message) | AppError::Validation(message) => message.clone(),
        _ => "access policy synchronization failed".to_owned(),
    }
}

async fn reconcile_http(
    client: &Client,
    config: &AccessSyncConfig,
    base: &str,
    desired: &BTreeSet<String>,
) -> Result<(), AppError> {
    let app_url = format!(
        "{base}/accounts/{}/access/apps/{}",
        config.account_id, config.application_id
    );
    let app = request_json(client.get(&app_url).bearer_auth(config.api_token.expose())).await?;
    validate_app(&app, &config.application_id, &config.hostname)?;
    let url = format!("{app_url}/policies/{}", config.policy_id);
    let policy = request_json(client.get(&url).bearer_auth(config.api_token.expose())).await?;
    validate_policy(&policy, config)?;
    if matches_membership(&policy, desired) {
        return Ok(());
    }
    let update = build_update(policy, desired)?;
    let put = client
        .put(&url)
        .bearer_auth(config.api_token.expose())
        .json(&update);
    request_json(put).await?;
    let readback = request_json(client.get(&url).bearer_auth(config.api_token.expose())).await?;
    validate_policy(&readback, config)?;
    verify_membership(&readback, desired)?;
    for (key, value) in &update {
        if key != "include" && readback["result"].get(key).unwrap_or(&Value::Null) != value {
            return Err(AppError::Unavailable(
                "Cloudflare policy readback changed policy settings".into(),
            ));
        }
    }
    Ok(())
}

fn validate_app(app: &Value, expected_id: &str, hostname: &str) -> Result<(), AppError> {
    if app.get("success").and_then(Value::as_bool) != Some(true) {
        return Err(AppError::Unavailable(
            "Cloudflare application request failed".to_owned(),
        ));
    }
    let domain = app["result"]["domain"].as_str().ok_or_else(|| {
        AppError::Unavailable("Cloudflare application response malformed".to_owned())
    })?;
    if app["result"]["id"].as_str() != Some(expected_id)
        || app["result"]["type"].as_str() != Some("self_hosted")
    {
        return Err(AppError::Validation(
            "Cloudflare application identity mismatch".to_owned(),
        ));
    }
    if domain != hostname {
        return Err(AppError::Validation(
            "Cloudflare application hostname mismatch".to_owned(),
        ));
    }
    Ok(())
}

fn validate_policy_rules(policy: &Value) -> Result<(), AppError> {
    let result = &policy["result"];
    let emails_only = result["include"].as_array().is_some_and(|rules| {
        rules.iter().all(|rule| {
            rule["email"]["email"]
                .as_str()
                .is_some_and(|email| *rule == json!({"email": {"email": email}}))
        })
    });
    let sentinel = result["include"] == json!([{"everyone": {}}])
        && result["exclude"] == json!([{"everyone": {}}]);
    if result["require"] != json!([])
        || !(sentinel || (emails_only && result["exclude"] == json!([])))
    {
        return Err(AppError::Validation(
            "Owned Cloudflare policy has unmanaged access rules".into(),
        ));
    }
    Ok(())
}

fn validate_policy(policy: &Value, config: &AccessSyncConfig) -> Result<(), AppError> {
    let object = policy
        .get("result")
        .and_then(Value::as_object)
        .ok_or_else(|| AppError::Unavailable("Cloudflare policy response malformed".to_owned()))?;
    if policy.get("success").and_then(Value::as_bool) != Some(true) {
        return Err(AppError::Unavailable(
            "Cloudflare policy request failed".to_owned(),
        ));
    }
    if object.get("id").and_then(Value::as_str) != Some(config.policy_id.as_str()) {
        return Err(AppError::Validation(
            "Cloudflare policy id mismatch".to_owned(),
        ));
    }
    if object.get("name").and_then(Value::as_str) != Some(OWNED_POLICY_NAME) {
        return Err(AppError::Validation(
            "Cloudflare policy name mismatch".to_owned(),
        ));
    }
    if object.get("decision").and_then(Value::as_str) != Some("allow") {
        return Err(AppError::Validation(
            "owned Cloudflare policy must use allow decision".to_owned(),
        ));
    }
    if object.get("reusable").and_then(Value::as_bool) == Some(true)
        || object
            .get("app_count")
            .and_then(Value::as_u64)
            .is_some_and(|count| count > 1)
    {
        return Err(AppError::Validation(
            "Cloudflare policy must not be reusable or shared".into(),
        ));
    }
    for (field, expected) in [
        ("app_id", &config.application_id),
        ("account_id", &config.account_id),
    ] {
        if object
            .get(field)
            .is_some_and(|value| !value.is_null() && value.as_str() != Some(expected.as_str()))
        {
            return Err(AppError::Validation(
                "Cloudflare policy owner mismatch".into(),
            ));
        }
    }
    for field in ["include", "exclude", "require"] {
        if !object.get(field).map(Value::is_array).unwrap_or(false) {
            return Err(AppError::Validation(format!(
                "Cloudflare policy field {field} is malformed"
            )));
        }
    }
    validate_policy_rules(policy)
}

fn build_update(policy: Value, desired: &BTreeSet<String>) -> Result<Map<String, Value>, AppError> {
    let mut object = policy
        .get("result")
        .and_then(Value::as_object)
        .cloned()
        .ok_or_else(|| AppError::Unavailable("Cloudflare policy response malformed".to_owned()))?;
    for field in [
        "id",
        "created_at",
        "updated_at",
        "app_id",
        "account_id",
        "uid",
        "reusable",
        "app_count",
    ] {
        object.remove(field);
    }
    object.insert("decision".to_owned(), Value::String("allow".to_owned()));
    let include = if desired.is_empty() {
        vec![json!({"everyone": {}})]
    } else {
        desired
            .iter()
            .map(|email| json!({"email": {"email": email}}))
            .collect()
    };
    object.insert("include".to_owned(), Value::Array(include));
    object.insert("require".to_owned(), Value::Array(Vec::new()));
    object.insert(
        "exclude".to_owned(),
        if desired.is_empty() {
            Value::Array(vec![json!({"everyone": {}})])
        } else {
            Value::Array(Vec::new())
        },
    );
    Ok(object)
}

fn verify_membership(policy: &Value, desired: &BTreeSet<String>) -> Result<(), AppError> {
    if !matches_membership(policy, desired) {
        return Err(AppError::Unavailable(
            "Cloudflare policy readback mismatch".to_owned(),
        ));
    }
    Ok(())
}

fn matches_membership(policy: &Value, desired: &BTreeSet<String>) -> bool {
    if validate_policy_rules(policy).is_err() {
        return false;
    }
    let result = &policy["result"];
    if desired.is_empty() {
        result["include"] == json!([{"everyone": {}}])
            && result["exclude"] == json!([{"everyone": {}}])
    } else {
        let Some(rules) = result["include"].as_array() else {
            return false;
        };
        let actual: BTreeSet<String> = rules
            .iter()
            .filter_map(|v| v["email"]["email"].as_str().map(str::to_owned))
            .collect();
        actual == *desired && rules.len() == desired.len() && result["exclude"] == json!([])
    }
}

async fn request_json(request: reqwest::RequestBuilder) -> Result<Value, AppError> {
    let response = request
        .send()
        .await
        .map_err(|_| AppError::Unavailable("Cloudflare policy request failed".to_owned()))?;
    let status = response.status();
    if response
        .content_length()
        .is_some_and(|length| length > MAX_RESPONSE_BYTES as u64)
        || !status.is_success()
    {
        return Err(AppError::Unavailable(
            "Cloudflare policy request failed".to_owned(),
        ));
    }
    let mut bytes = Vec::new();
    let mut response = response;
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| AppError::Unavailable("Cloudflare policy response unreadable".to_owned()))?
    {
        if bytes.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
            return Err(AppError::Unavailable(
                "Cloudflare policy response too large".to_owned(),
            ));
        }
        bytes.extend_from_slice(&chunk);
    }
    let value: Value = serde_json::from_slice(&bytes)
        .map_err(|_| AppError::Unavailable("Cloudflare policy response malformed".to_owned()))?;
    if value["success"] != true {
        return Err(AppError::Unavailable(
            "Cloudflare API rejected the request".into(),
        ));
    }
    Ok(value)
}

fn validate_config(config: &AccessSyncConfig) -> Result<(), AppError> {
    for (name, value) in [
        ("account", &config.account_id),
        ("application", &config.application_id),
        ("policy", &config.policy_id),
    ] {
        if value.is_empty()
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte))
        {
            return Err(AppError::Validation(format!(
                "invalid access sync {name} id"
            )));
        }
    }
    if config.hostname.is_empty() || config.hostname.contains('/') {
        return Err(AppError::Validation(
            "invalid access sync hostname".to_owned(),
        ));
    }
    if config.api_token.expose().is_empty() {
        return Err(AppError::Validation(
            "access sync API token is empty".to_owned(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Json, Router, extract::State, routing::get};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    #[derive(Clone)]
    struct MockState {
        policy: Arc<Mutex<Value>>,
        puts: Arc<AtomicUsize>,
        fail_put: Arc<AtomicBool>,
        hold_put: Arc<AtomicBool>,
        started: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Notify>,
        corrupt_readback: Arc<AtomicBool>,
    }

    async fn app() -> Json<Value> {
        Json(
            json!({"success": true, "result": {"id": "app", "type": "self_hosted", "name": "Artifact member emails", "domain": "club.example"}}),
        )
    }

    async fn policy(
        State(state): State<MockState>,
        method: axum::http::Method,
        body: Option<Json<Value>>,
    ) -> Json<Value> {
        if method == axum::http::Method::PUT {
            state.puts.fetch_add(1, Ordering::Relaxed);
            if state.hold_put.swap(false, Ordering::SeqCst) {
                state.started.notify_one();
                state.release.notified().await;
            }
            if state.fail_put.load(Ordering::SeqCst) {
                return Json(
                    json!({"success":false,"errors":[{"message":"test-token private server detail"}]}),
                );
            }
            if let Some(Json(body)) = body {
                let mut body = body;
                body["id"] = json!("p");
                if state.corrupt_readback.load(Ordering::SeqCst) {
                    body["include"]
                        .as_array_mut()
                        .unwrap()
                        .push(json!({"everyone":{}}));
                }
                *state.policy.lock().expect("mock lock") = body;
            }
        }
        Json(json!({"success": true, "result": state.policy.lock().expect("mock lock").clone()}))
    }

    async fn mock() -> (String, MockState, tokio::task::JoinHandle<()>) {
        let state = MockState {
            policy: Arc::new(Mutex::new(
                json!({"id":"p", "name":"Artifact member emails", "decision":"allow", "include":[], "exclude":[], "require":[], "precedence":1}),
            )),
            puts: Arc::new(AtomicUsize::new(0)),
            fail_put: Arc::new(AtomicBool::new(false)),
            hold_put: Arc::new(AtomicBool::new(false)),
            started: Arc::new(tokio::sync::Notify::new()),
            release: Arc::new(tokio::sync::Notify::new()),
            corrupt_readback: Arc::new(AtomicBool::new(false)),
        };
        let router = Router::new()
            .route("/accounts/a/access/apps/app", get(app))
            .route(
                "/accounts/a/access/apps/app/policies/p",
                get(policy).put(policy),
            )
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock");
        let address = listener.local_addr().expect("mock address");
        let task = tokio::spawn(async move {
            axum::serve(listener, router).await.expect("mock server");
        });
        (format!("http://{address}"), state, task)
    }

    fn config() -> AccessSyncConfig {
        AccessSyncConfig {
            account_id: "a".into(),
            application_id: "app".into(),
            policy_id: "p".into(),
            hostname: "club.example".into(),
            api_token: Secret::new("test-token"),
        }
    }

    #[tokio::test]
    async fn reconciles_explicit_emails_and_verifies_readback() {
        let (base, state, task) = mock().await;
        let desired = BTreeSet::from(["a@example.com".to_owned(), "b@example.com".to_owned()]);
        reconcile_http(&Client::new(), &config(), &base, &desired)
            .await
            .expect("sync");
        assert_eq!(state.puts.load(Ordering::Relaxed), 1);
        let policy = state.policy.lock().expect("mock lock").clone();
        assert_eq!(policy["include"].as_array().map(Vec::len), Some(2));
        task.abort();
    }

    #[tokio::test]
    async fn empty_reconciliation_closes_both_sides() {
        let (base, state, task) = mock().await;
        reconcile_http(&Client::new(), &config(), &base, &BTreeSet::new())
            .await
            .expect("sync");
        let policy = state.policy.lock().expect("mock lock").clone();
        assert_eq!(policy["include"][0]["everyone"], json!({}));
        assert_eq!(policy["exclude"][0]["everyone"], json!({}));
        task.abort();
    }
    fn pool() -> DbPool {
        let pool = r2d2::Pool::builder()
            .max_size(1)
            .build(r2d2_sqlite::SqliteConnectionManager::memory())
            .unwrap();
        pool.get()
            .unwrap()
            .execute_batch(
                "CREATE TABLE org_email_members(email TEXT PRIMARY KEY, org TEXT NOT NULL);",
            )
            .unwrap();
        pool
    }

    async fn wait_state(sync: &AccessSync, expected: &str) {
        time::timeout(Duration::from_secs(3), async {
            while sync.status().state != expected {
                time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("expected {expected}, got {:?}", sync.status()));
    }

    #[tokio::test]
    async fn disabled_stays_disabled_after_membership_wake() {
        let sync = AccessSync::start(None, pool()).unwrap();
        sync.wake();
        assert_eq!(sync.status().state, "disabled");
    }

    #[tokio::test]
    async fn removes_last_member_and_can_add_first_member_again() {
        let (base, state, task) = mock().await;
        let client = Client::new();
        let desired = BTreeSet::from(["a@example.com".to_owned()]);
        state.policy.lock().unwrap()["session_duration"] = json!("8h");
        reconcile_http(&client, &config(), &base, &desired)
            .await
            .unwrap();
        reconcile_http(&client, &config(), &base, &BTreeSet::new())
            .await
            .unwrap();
        assert_eq!(
            state.policy.lock().unwrap()["exclude"],
            json!([{"everyone":{}}])
        );
        reconcile_http(&client, &config(), &base, &desired)
            .await
            .unwrap();
        assert_eq!(state.policy.lock().unwrap()["exclude"], json!([]));
        assert_eq!(state.policy.lock().unwrap()["session_duration"], "8h");
        assert_eq!(state.policy.lock().unwrap()["precedence"], 1);
        reconcile_http(&client, &config(), &base, &desired)
            .await
            .unwrap();
        assert_eq!(
            state.puts.load(Ordering::Relaxed),
            3,
            "matching policy must not be written again"
        );
        task.abort();
    }

    #[tokio::test]
    async fn refuses_wrong_targets_shared_policies_and_unmanaged_rules() {
        let (base, state, task) = mock().await;
        let desired = BTreeSet::from(["a@example.com".into()]);
        let original = state.policy.lock().unwrap().clone();
        for (key, value) in [
            ("name", json!("Other policy")),
            ("decision", json!("bypass")),
            ("reusable", json!(true)),
            ("app_count", json!(2)),
            ("app_id", json!("another-app")),
            ("account_id", json!("another-account")),
            (
                "require",
                json!([{"email_domain":{"domain":"example.com"}}]),
            ),
            ("exclude", json!([{"email":{"email":"a@example.com"}}])),
            ("include", json!([{"everyone":{}}])),
            (
                "include",
                json!([{"email":{"email":"a@example.com"},"everyone":{}}]),
            ),
        ] {
            let mut changed = original.clone();
            changed[key] = value;
            *state.policy.lock().unwrap() = changed;
            assert!(
                reconcile_http(&Client::new(), &config(), &base, &desired)
                    .await
                    .is_err(),
                "accepted {key}"
            );
        }
        *state.policy.lock().unwrap() = original;
        let mut wrong = config();
        wrong.hostname = "different.example".into();
        assert!(
            reconcile_http(&Client::new(), &wrong, &base, &desired)
                .await
                .is_err()
        );
        assert_eq!(state.puts.load(Ordering::Relaxed), 0);
        task.abort();
    }

    #[tokio::test]
    async fn does_not_report_success_when_readback_contains_extra_grants() {
        let (base, state, task) = mock().await;
        state.corrupt_readback.store(true, Ordering::SeqCst);
        let desired = BTreeSet::from(["a@example.com".into()]);
        assert!(
            reconcile_http(&Client::new(), &config(), &base, &desired)
                .await
                .is_err()
        );
        task.abort();
    }

    #[tokio::test]
    async fn startup_retries_failures_and_reloads_durable_union_on_wake() {
        let (base, state, task) = mock().await;
        let pool = pool();
        pool.get().unwrap().execute_batch("INSERT INTO org_email_members VALUES ('A@example.com','bookclub'),('a@example.com','another'),('b@example.com','another');").unwrap();
        state.fail_put.store(true, Ordering::SeqCst);
        let sync = AccessSync::start_with_client(
            config(),
            pool.clone(),
            Client::new(),
            base,
            Duration::from_millis(80),
        )
        .unwrap();
        wait_state(&sync, "error").await;
        assert!(!sync.status().message.contains("test-token"));
        assert_eq!(
            pool.get()
                .unwrap()
                .query_row("SELECT count(*) FROM org_email_members", [], |r| r
                    .get::<_, i64>(0))
                .unwrap(),
            3
        );
        state.fail_put.store(false, Ordering::SeqCst);
        wait_state(&sync, "synced").await; // Timer retries without another mutation.
        assert_eq!(
            state.policy.lock().unwrap()["include"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
        pool.get()
            .unwrap()
            .execute("DELETE FROM org_email_members WHERE org='bookclub'", [])
            .unwrap();
        sync.wake();
        wait_state(&sync, "synced").await;
        assert_eq!(
            state.policy.lock().unwrap()["include"]
                .as_array()
                .unwrap()
                .len(),
            2,
            "other org still needs a@example.com"
        );
        pool.get()
            .unwrap()
            .execute("DELETE FROM org_email_members", [])
            .unwrap();
        sync.wake();
        wait_state(&sync, "synced").await;
        assert_eq!(
            state.policy.lock().unwrap()["exclude"],
            json!([{"everyone":{}}])
        );
        let weak = Arc::downgrade(&sync.inner);
        drop(sync);
        time::timeout(Duration::from_secs(2), async {
            while weak.upgrade().is_some() {
                time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        task.abort();
    }

    #[tokio::test]
    async fn wake_during_http_update_is_not_lost_or_reported_as_synced() {
        let (base, state, task) = mock().await;
        let pool = pool();
        pool.get()
            .unwrap()
            .execute(
                "INSERT INTO org_email_members VALUES ('a@example.com','bookclub')",
                [],
            )
            .unwrap();
        state.hold_put.store(true, Ordering::SeqCst);
        let sync = AccessSync::start_with_client(
            config(),
            pool.clone(),
            Client::new(),
            base,
            RUN_INTERVAL,
        )
        .unwrap();
        time::timeout(Duration::from_secs(3), state.started.notified())
            .await
            .unwrap();
        pool.get()
            .unwrap()
            .execute("DELETE FROM org_email_members", [])
            .unwrap();
        sync.wake();
        assert_eq!(sync.status().state, "pending");
        state.release.notify_one();
        wait_state(&sync, "synced").await;
        assert_eq!(
            state.policy.lock().unwrap()["exclude"],
            json!([{"everyone":{}}])
        );
        assert_eq!(state.puts.load(Ordering::Relaxed), 2);
        drop(sync);
        task.abort();
    }
}
