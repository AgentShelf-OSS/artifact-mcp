//! Named, tenant-scoped live sources. Credentials and network URLs stay in the server.
use crate::{
    error::AppError,
    persistence::db::{DbPool, interact},
    security::audit::{AuditEvent, MutationAudit, append_in_transaction},
};
use reqwest::{
    Url,
    header::{HeaderMap, HeaderName, HeaderValue},
};
use rusqlite::{OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    sync::Arc,
    time::Duration,
};
use tokio::sync::{Mutex, OwnedSemaphorePermit, RwLock, Semaphore, mpsc};

pub const MAX_SOURCES: usize = 32;
pub const MAX_BINDINGS: usize = 8;
pub const MAX_EVENTS: usize = 1000;
const MAX_BYTES: usize = 1024 * 1024;
pub const MAX_VIEWER_STREAMS: usize = 64;
fn default_kind() -> String {
    "http".into()
}
fn default_max_bytes() -> usize {
    MAX_BYTES
}
fn default_timeout() -> u64 {
    10_000
}
fn default_poll() -> u64 {
    2000
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceConfig {
    pub id: String,
    pub org: String,
    #[serde(default = "default_kind")]
    pub kind: String,
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub headers_env: BTreeMap<String, String>,
    #[serde(default)]
    pub operations: BTreeMap<String, OperationConfig>,
    #[serde(default)]
    pub subscriptions: BTreeMap<String, SubscriptionConfig>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OperationConfig {
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub key: Option<String>,
    #[serde(default)]
    pub params: BTreeMap<String, ParamConfig>,
    #[serde(default = "default_max_bytes")]
    pub max_bytes: usize,
    #[serde(default = "default_timeout")]
    pub timeout_ms: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ParamConfig {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub minimum: Option<i64>,
    #[serde(default)]
    pub maximum: Option<i64>,
    #[serde(default)]
    pub max_length: Option<usize>,
    #[serde(rename = "enum", default)]
    pub enum_values: Option<Vec<Value>>,
    #[serde(default)]
    pub default: Option<Value>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubscriptionConfig {
    pub transport: String,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub events: Vec<String>,
    #[serde(default)]
    pub operation: Option<String>,
    #[serde(default = "default_poll")]
    pub interval_ms: u64,
}
#[derive(Clone, Debug, Serialize)]
pub struct PublicSource {
    pub id: String,
    pub kind: String,
    pub operations: BTreeMap<String, Value>,
    pub subscriptions: BTreeMap<String, Value>,
}
#[derive(Clone, Debug, Serialize)]
pub struct SourceHealth {
    pub state: String,
    pub last_success_at: Option<String>,
    pub last_query_at: Option<String>,
    pub last_event_at: Option<String>,
    pub retry_count: u64,
    pub error: Option<String>,
}
impl Default for SourceHealth {
    fn default() -> Self {
        Self {
            state: "idle".into(),
            last_success_at: None,
            last_query_at: None,
            last_event_at: None,
            retry_count: 0,
            error: None,
        }
    }
}
#[derive(Clone, Debug, Serialize)]
pub struct SourceView {
    pub id: String,
    pub org: String,
    pub origin: String,
    pub enabled: bool,
    pub version: u64,
    pub definition: SourceConfig,
    pub missing_references: Vec<String>,
    pub health: SourceHealth,
    pub artifact_count: u64,
}
#[derive(Clone, Debug, Serialize)]
pub struct ImpactArtifact {
    pub id: String,
    pub title: String,
    pub org: String,
    pub url: String,
    pub bindings: Vec<Value>,
}
#[derive(Clone, Debug, Serialize)]
pub struct TestResult {
    pub ok: bool,
    pub source_id: String,
    pub elapsed_ms: u64,
    pub summary: Value,
    pub error: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize, Default, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Binding {
    pub source: String,
    #[serde(default)]
    pub operations: Vec<String>,
    #[serde(default)]
    pub subscriptions: Vec<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct BindingManifest {
    pub bindings: BTreeMap<String, Binding>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Event {
    pub id: String,
    pub event: String,
    pub data: Value,
}
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequestedSubscription {
    pub binding: String,
    pub subscription: String,
}
#[derive(Clone, Debug, Serialize)]
pub struct DataEnvelope {
    pub binding: String,
    pub subscription: String,
    pub event: String,
    pub id: String,
    pub data: Value,
}
#[derive(Default)]
struct Memory {
    bindings: HashMap<String, (String, BindingManifest)>,
    snapshots: HashMap<(String, String, String), (u64, Value)>,
    events: HashMap<(String, String, String), Vec<Event>>,
}
#[derive(Clone)]
pub struct DataBroker {
    pub sources: Arc<BTreeMap<String, SourceConfig>>,
    headers: Arc<BTreeMap<String, HeaderMap>>,
    client: reqwest::Client,
    memory: Arc<RwLock<Memory>>,
    writes: Arc<Mutex<()>>,
    stream_slots: Arc<Semaphore>,
    pub pool: Option<DbPool>,
    health: Arc<RwLock<BTreeMap<String, SourceHealth>>>,
    audit_key: Arc<std::sync::RwLock<Option<[u8; 32]>>>,
}
pub struct DataStream {
    _permit: OwnedSemaphorePermit,
    pub receiver: mpsc::Receiver<DataEnvelope>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}
impl Drop for DataStream {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}
impl DataBroker {
    pub fn empty() -> Self {
        Self {
            sources: Arc::new(BTreeMap::new()),
            headers: Arc::new(BTreeMap::new()),
            client: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .connect_timeout(Duration::from_secs(10))
                .build()
                .expect("HTTP client"),
            memory: Arc::new(RwLock::new(Memory::default())),
            writes: Arc::new(Mutex::new(())),
            stream_slots: Arc::new(Semaphore::new(MAX_VIEWER_STREAMS)),
            pool: None,
            health: Arc::new(RwLock::new(BTreeMap::new())),
            audit_key: Arc::new(std::sync::RwLock::new(None)),
        }
    }
    pub fn with_pool(mut self, pool: DbPool) -> Self {
        self.pool = Some(pool);
        self
    }
    pub fn with_audit_key(self, key: [u8; 32]) -> Self {
        if let Ok(mut slot) = self.audit_key.write() {
            *slot = Some(key);
        }
        self
    }
    fn audit_key(&self) -> Result<[u8; 32], String> {
        self.audit_key
            .read()
            .ok()
            .and_then(|slot| *slot)
            .ok_or_else(|| "data_unavailable".into())
    }
    pub async fn audit_mutation(
        &self,
        audit: MutationAudit,
        operation: &str,
        id: &str,
        _org: &str,
        result: &str,
        revision: Option<u64>,
    ) -> Result<(), String> {
        let Some(pool) = &self.pool else {
            return Ok(());
        };
        let org = self.source_record(id).await?.0.org;
        let audit = audit
            .for_target_tenant(&org)
            .map_err(|_| "data_unavailable".to_owned())?;
        let key = self.audit_key()?;
        let event = AuditEvent {
            operation: operation.into(),
            target_type: "data_source".into(),
            target_id: id.into(),
            result: result.into(),
            classification: "admin".into(),
            revision,
        };
        interact(pool, move |c| {
            let tx = c
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(|_| AppError::Internal)?;
            append_in_transaction(
                &tx,
                &key,
                &audit.event_id().map_err(|_| AppError::Internal)?,
                audit.context(),
                &event,
            )?;
            tx.commit().map_err(|_| AppError::Internal)
        })
        .await
        .map_err(|_| "data_unavailable".into())
    }
    pub fn from_env() -> Result<Self, String> {
        let Some(path) = std::env::var_os("ARTIFACT_DATA_SOURCES_FILE").filter(|p| !p.is_empty())
        else {
            return Ok(Self::empty());
        };
        let raw = std::fs::read_to_string(path).map_err(|_| "invalid_data_sources")?;
        Self::from_json(&raw)
    }
    pub fn from_json(raw: &str) -> Result<Self, String> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Config {
            sources: Vec<SourceConfig>,
        }
        let config: Config = serde_json::from_str(raw).map_err(|_| "invalid_data_sources")?;
        if config.sources.len() > MAX_SOURCES {
            return Err("invalid_data_sources".into());
        }
        let mut broker = Self::empty();
        let mut sources = BTreeMap::new();
        let mut headers = BTreeMap::new();
        for source in config.sources {
            validate_source(&source)?;
            if sources.contains_key(&source.id) {
                return Err("invalid_data_sources".into());
            }
            let mut resolved = HeaderMap::new();
            for (name, env) in &source.headers_env {
                let name =
                    HeaderName::from_bytes(name.as_bytes()).map_err(|_| "invalid_data_sources")?;
                if matches!(
                    name.as_str(),
                    "host" | "cookie" | "connection" | "content-length" | "transfer-encoding"
                ) {
                    return Err("invalid_data_sources".into());
                }
                let value = std::env::var(env).map_err(|_| "invalid_data_sources")?;
                let mut value =
                    HeaderValue::from_str(&value).map_err(|_| "invalid_data_sources")?;
                value.set_sensitive(true);
                resolved.insert(name, value);
            }
            headers.insert(source.id.clone(), resolved);
            sources.insert(source.id.clone(), source);
        }
        broker.sources = Arc::new(sources);
        broker.headers = Arc::new(headers);
        Ok(broker)
    }
    async fn managed_rows(&self) -> Result<Vec<(SourceConfig, bool, u64)>, String> {
        let Some(pool) = &self.pool else {
            return Ok(Vec::new());
        };
        let rows = interact(pool, |c| {
            let mut stmt = c
                .prepare("SELECT definition,enabled,version FROM data_sources ORDER BY id")
                .map_err(|_| AppError::Internal)?;
            let mapped = stmt
                .query_map([], |row| {
                    let raw: String = row.get(0)?;
                    let definition =
                        serde_json::from_str(&raw).map_err(|_| rusqlite::Error::InvalidQuery)?;
                    Ok((
                        definition,
                        row.get::<_, i64>(1)? != 0,
                        row.get::<_, i64>(2)? as u64,
                    ))
                })
                .map_err(|_| AppError::Internal)?;
            mapped
                .collect::<Result<Vec<_>, _>>()
                .map_err(|_| AppError::Internal)
        })
        .await
        .map_err(|_| "data_unavailable".to_owned())?;
        Ok(rows)
    }
    async fn source_record(&self, id: &str) -> Result<(SourceConfig, bool, u64, String), String> {
        if let Some(source) = self.sources.get(id) {
            return Ok((source.clone(), true, 0, "operator".into()));
        }
        let Some(pool) = &self.pool else {
            return Err("not_found".into());
        };
        let id_owned = id.to_owned();
        interact(pool, move |c| {
            c.query_row(
                "SELECT definition,enabled,version FROM data_sources WHERE id=?1",
                [&id_owned],
                |row| {
                    let raw: String = row.get(0)?;
                    let source =
                        serde_json::from_str(&raw).map_err(|_| rusqlite::Error::InvalidQuery)?;
                    Ok((
                        source,
                        row.get::<_, i64>(1)? != 0,
                        row.get::<_, i64>(2)? as u64,
                        "managed".to_owned(),
                    ))
                },
            )
            .optional()
            .map_err(|_| AppError::Internal)
        })
        .await
        .map_err(|_| "data_unavailable".to_owned())?
        .ok_or_else(|| "not_found".into())
    }
    fn missing_references(source: &SourceConfig) -> Vec<String> {
        source
            .headers_env
            .values()
            .filter(|name| std::env::var_os(name).is_none())
            .cloned()
            .collect()
    }
    fn resolved_headers(source: &SourceConfig) -> Result<HeaderMap, String> {
        let mut headers = HeaderMap::new();
        for (name, env) in &source.headers_env {
            let name = HeaderName::from_bytes(name.as_bytes()).map_err(|_| "config_error")?;
            let raw = std::env::var(env).map_err(|_| "config_error")?;
            let mut value = HeaderValue::from_str(&raw).map_err(|_| "config_error")?;
            value.set_sensitive(true);
            headers.insert(name, value);
        }
        Ok(headers)
    }
    async fn source_view(
        &self,
        source: SourceConfig,
        enabled: bool,
        version: u64,
        origin: &str,
    ) -> SourceView {
        let id = source.id.clone();
        let mut health = self
            .health
            .read()
            .await
            .get(&id)
            .cloned()
            .unwrap_or_else(|| {
                let mut h = SourceHealth::default();
                if !enabled {
                    h.state = "disabled".into();
                } else if !Self::missing_references(&source).is_empty() {
                    h.state = "config_error".into();
                }
                h
            });
        if !enabled {
            health.state = "disabled".into();
        } else if !Self::missing_references(&source).is_empty() {
            health.state = "config_error".into();
        }
        let artifact_count = self
            .impact(&id)
            .await
            .map(|v| v.len() as u64)
            .unwrap_or_default();
        SourceView {
            id,
            org: source.org.clone(),
            origin: origin.into(),
            enabled,
            version,
            missing_references: Self::missing_references(&source),
            health,
            artifact_count,
            definition: source,
        }
    }
    pub async fn source_views(&self, org: Option<&str>) -> Result<Vec<SourceView>, String> {
        self.validate_registry().await?;
        let mut views = Vec::new();
        for source in self
            .sources
            .values()
            .filter(|s| org.is_none_or(|o| o == s.org))
        {
            views.push(self.source_view(source.clone(), true, 1, "operator").await);
        }
        for (source, enabled, version) in self.managed_rows().await? {
            if org.is_none_or(|o| o == source.org) {
                views.push(self.source_view(source, enabled, version, "managed").await);
            }
        }
        views.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(views)
    }
    pub async fn source_view_by_id(&self, id: &str) -> Result<SourceView, String> {
        if let Some(source) = self.sources.get(id) {
            return Ok(self.source_view(source.clone(), true, 1, "operator").await);
        }
        let (source, enabled, version, origin) = self.source_record(id).await?;
        Ok(self.source_view(source, enabled, version, &origin).await)
    }
    pub async fn create_managed_source(
        &self,
        source: SourceConfig,
        enabled: bool,
        audit: MutationAudit,
    ) -> Result<SourceView, String> {
        let _guard = self.writes.lock().await;
        validate_source(&source)?;
        if self.sources.contains_key(&source.id) {
            return Err("source_conflict".into());
        }
        let Some(pool) = &self.pool else {
            return Err("data_unavailable".into());
        };
        let id = source.id.clone();
        let org = source.org.clone();
        let audit = audit
            .for_target_tenant(&org)
            .map_err(|_| "data_unavailable".to_owned())?;
        let raw = serde_json::to_string(&source).map_err(|_| "bad_params")?;
        let audit_key = self.audit_key()?;
        interact(pool, move |c| {
            let tx = c
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(|_| AppError::Internal)?;
            let exists: Option<i64> = tx
                .query_row("SELECT 1 FROM data_sources WHERE id=?1", [&id], |r| {
                    r.get(0)
                })
                .optional()
                .map_err(|_| AppError::Internal)?;
            if exists.is_some() {
                return Err(AppError::Conflict("source_conflict".into()));
            }
            let org_exists: Option<i64> = tx
                .query_row("SELECT 1 FROM orgs WHERE name=?1", [&org], |r| r.get(0))
                .optional()
                .map_err(|_| AppError::Internal)?;
            if org_exists.is_none() {
                return Err(AppError::Validation("unknown_org".into()));
            }
            tx.execute(
                "INSERT INTO data_sources(id,org,definition,enabled,version) VALUES(?1,?2,?3,?4,1)",
                rusqlite::params![id, org, raw, enabled as i64],
            )
            .map_err(|_| AppError::Internal)?;
            let event = AuditEvent {
                operation: "data_source.create".into(),
                target_type: "data_source".into(),
                target_id: id.clone(),
                result: "success".into(),
                classification: "admin".into(),
                revision: Some(1),
            };
            append_in_transaction(
                &tx,
                &audit_key,
                &audit.event_id().map_err(|_| AppError::Internal)?,
                audit.context(),
                &event,
            )?;
            tx.commit().map_err(|_| AppError::Internal)
        })
        .await
        .map_err(|e| match e {
            AppError::Conflict(_) => String::from("source_conflict"),
            AppError::Validation(_) => String::from("unknown_org"),
            _ => String::from("data_unavailable"),
        })?;
        self.source_view_by_id(&source.id).await
    }
    pub async fn set_managed_source(
        &self,
        id: &str,
        source: SourceConfig,
        enabled: bool,
        expected: u64,
        audit: MutationAudit,
    ) -> Result<SourceView, String> {
        let _guard = self.writes.lock().await;
        validate_source(&source)?;
        if id != source.id {
            return Err("invalid_source".into());
        }
        if self.sources.contains_key(id) {
            return Err("operator_read_only".into());
        }
        let Some(pool) = &self.pool else {
            return Err("data_unavailable".into());
        };
        let raw = serde_json::to_string(&source).map_err(|_| "bad_params")?;
        let id_owned = id.to_owned();
        let org = source.org.clone();
        let audit = audit
            .for_target_tenant(&org)
            .map_err(|_| "data_unavailable".to_owned())?;
        let audit_key = self.audit_key()?;
        interact(pool, move |c| {
            let tx=c.transaction_with_behavior(TransactionBehavior::Immediate).map_err(|_| AppError::Internal)?;
            let row: Option<(String,i64,i64)> = tx.query_row("SELECT org,version,enabled FROM data_sources WHERE id=?1", [&id_owned], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional().map_err(|_| AppError::Internal)?;
            let Some((old_org,version,_))=row else { return Err(AppError::NotFound("not_found".into())); };
            if version as u64 != expected { return Err(AppError::Conflict("stale_version".into())); }
            let org_exists: Option<i64> = tx.query_row("SELECT 1 FROM orgs WHERE name=?1", [&org], |r| r.get(0)).optional().map_err(|_| AppError::Internal)?;
            if org_exists.is_none() { return Err(AppError::Validation("unknown_org".into())); }
            if old_org != org { let count: i64=bound_source_count(&tx,&id_owned).map_err(|_| AppError::Internal)?; if count>0 { return Err(AppError::Conflict("org_in_use".into())); } }
            let changed: i64=bound_capability_count(&tx,&id_owned,&raw).map_err(|_| AppError::Internal)?; if changed>0 { return Err(AppError::Conflict("binding_in_use".into())); }
            tx.execute("UPDATE data_sources SET org=?2,definition=?3,enabled=?4,version=version+1,updated_at=datetime('now') WHERE id=?1",rusqlite::params![id_owned,org,raw,enabled as i64]).map_err(|_|AppError::Internal)?;
            let event = AuditEvent { operation: "data_source.update".into(), target_type: "data_source".into(), target_id: id_owned.clone(), result: "success".into(), classification: "admin".into(), revision: Some((version + 1) as u64) };
            append_in_transaction(&tx, &audit_key, &audit.event_id().map_err(|_| AppError::Internal)?, audit.context(), &event)?;
            tx.commit().map_err(|_|AppError::Internal)
        }).await.map_err(|e| match e { AppError::Conflict(x)=>x,AppError::NotFound(_)=>"not_found".into(),AppError::Validation(x)=>x,_=>"data_unavailable".into() })?;
        self.source_view_by_id(id).await
    }
    pub async fn set_managed_enabled(
        &self,
        id: &str,
        enabled: bool,
        expected: u64,
        audit: MutationAudit,
    ) -> Result<SourceView, String> {
        let _guard = self.writes.lock().await;
        if self.sources.contains_key(id) {
            return Err("operator_read_only".into());
        }
        let Some(pool) = &self.pool else {
            return Err("data_unavailable".into());
        };
        let id_owned = id.to_owned();
        let org = self
            .source_record(id)
            .await
            .map_err(|e| e.to_owned())?
            .0
            .org;
        let audit = audit
            .for_target_tenant(&org)
            .map_err(|_| "data_unavailable".to_owned())?;
        let audit_key = self.audit_key()?;
        interact(pool, move |c| {
            let tx = c.transaction_with_behavior(TransactionBehavior::Immediate).map_err(|_| AppError::Internal)?;
            let changed = tx.execute("UPDATE data_sources SET enabled=?2,version=version+1,updated_at=datetime('now') WHERE id=?1 AND version=?3", rusqlite::params![id_owned, enabled as i64, expected as i64]).map_err(|_| AppError::Internal)?;
            if changed == 0 { return Err(AppError::Conflict("stale_version".into())); }
            let event = AuditEvent { operation: if enabled { "data_source.enable" } else { "data_source.disable" }.into(), target_type: "data_source".into(), target_id: id_owned.clone(), result: "success".into(), classification: "admin".into(), revision: Some(expected + 1) };
            append_in_transaction(&tx, &audit_key, &audit.event_id().map_err(|_| AppError::Internal)?, audit.context(), &event)?;
            tx.commit().map_err(|_| AppError::Internal)
        }).await.map_err(|e| match e { AppError::Conflict(_) => String::from("stale_version"), _ => String::from("data_unavailable") })?;
        self.source_view_by_id(id).await
    }
    pub async fn delete_managed(
        &self,
        id: &str,
        expected: u64,
        audit: MutationAudit,
    ) -> Result<(), String> {
        let _guard = self.writes.lock().await;
        if self.sources.contains_key(id) {
            return Err("operator_read_only".into());
        }
        let Some(pool) = &self.pool else {
            return Err("data_unavailable".into());
        };
        let id_owned = id.to_owned();
        let org = self
            .source_record(id)
            .await
            .map_err(|e| e.to_owned())?
            .0
            .org;
        let audit = audit
            .for_target_tenant(&org)
            .map_err(|_| "data_unavailable".to_owned())?;
        let audit_key = self.audit_key()?;
        interact(pool, move |c| {
            let tx = c
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(|_| AppError::Internal)?;
            let row: Option<i64> = tx
                .query_row(
                    "SELECT version FROM data_sources WHERE id=?1",
                    [&id_owned],
                    |r| r.get(0),
                )
                .optional()
                .map_err(|_| AppError::Internal)?;
            let Some(version) = row else {
                return Err(AppError::NotFound("not_found".into()));
            };
            if version as u64 != expected {
                return Err(AppError::Conflict("stale_version".into()));
            }
            if bound_source_count(&tx, &id_owned).map_err(|_| AppError::Internal)? > 0 {
                return Err(AppError::Conflict("source_in_use".into()));
            }
            tx.execute("DELETE FROM data_sources WHERE id=?1", [&id_owned])
                .map_err(|_| AppError::Internal)?;
            let event = AuditEvent {
                operation: "data_source.delete".into(),
                target_type: "data_source".into(),
                target_id: id_owned.clone(),
                result: "success".into(),
                classification: "admin".into(),
                revision: Some(expected),
            };
            append_in_transaction(
                &tx,
                &audit_key,
                &audit.event_id().map_err(|_| AppError::Internal)?,
                audit.context(),
                &event,
            )?;
            tx.commit().map_err(|_| AppError::Internal)
        })
        .await
        .map_err(|e| match e {
            AppError::Conflict(x) => x,
            AppError::NotFound(_) => "not_found".into(),
            _ => "data_unavailable".into(),
        })
    }
    pub async fn impact(&self, source_id: &str) -> Result<Vec<ImpactArtifact>, String> {
        let Some(pool) = &self.pool else {
            return Ok(Vec::new());
        };
        let source = source_id.to_owned();
        interact(pool,move|c|{let mut stmt=c.prepare("SELECT a.id,a.title,a.org,b.bindings FROM artifact_data_bindings b JOIN artifacts a ON a.id=b.artifact_id WHERE a.org=b.org").map_err(|_|AppError::Internal)?;let rows=stmt.query_map([],|r|{let raw:String=r.get(3)?;Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,raw))}).map_err(|_|AppError::Internal)?;let mut out=Vec::new();for row in rows{let(id,title,org,raw)=row.map_err(|_|AppError::Internal)?;let m:BindingManifest=serde_json::from_str(&raw).map_err(|_|AppError::Internal)?;let bindings=m.bindings.into_iter().filter(|(_,b)|b.source==source).map(|(name,b)|json!({"name":name,"operations":b.operations,"subscriptions":b.subscriptions})).collect::<Vec<_>>();if !bindings.is_empty(){out.push(ImpactArtifact{id:id.clone(),title,org,url:format!("/{id}"),bindings});}}Ok(out)}).await.map_err(|_|"data_unavailable".into())
    }
    pub async fn test_source(
        &self,
        id: &str,
        operation: &str,
        params: Value,
    ) -> Result<TestResult, String> {
        let (source, enabled, _, _) = self.source_record(id).await?;
        if !enabled {
            return Err("test_unavailable".into());
        }
        if !Self::missing_references(&source).is_empty() {
            return Err("test_unavailable".into());
        }
        if source.kind == "push" {
            if !operation.is_empty() {
                return Err("bad_params".into());
            }
            if !params.as_object().is_some_and(|v| v.is_empty()) {
                return Err("bad_params".into());
            }
            self.record_query_health(id, Ok(())).await;
            return Ok(TestResult {
                ok: true,
                source_id: id.into(),
                elapsed_ms: 0,
                summary: json!({"kind":"push"}),
                error: None,
            });
        }
        if !source.operations.contains_key(operation) {
            return Err("bad_params".into());
        }
        let op = source.operations.get(operation).ok_or("not_found")?.clone();
        build_url(&source, &op, &params)?;
        let started = std::time::Instant::now();
        let result = self.http_query(&source, &op, &params).await;
        self.record_query_health(
            &source.id,
            result.as_ref().map(|_| ()).map_err(|e| e.as_str()),
        )
        .await;
        let elapsed_ms = started.elapsed().as_millis().min(u64::MAX as u128) as u64;
        match result {
            Ok(value) => Ok(TestResult {
                ok: true,
                source_id: id.into(),
                elapsed_ms,
                summary: json!({"kind":"http","response_type":if value.is_object(){"object"}else if value.is_array(){"array"}else{"scalar"},"bytes":serde_json::to_vec(&value).map(|v|v.len()).unwrap_or(0)}),
                error: None,
            }),
            Err(error) => Ok(TestResult {
                ok: false,
                source_id: id.into(),
                elapsed_ms,
                summary: json!({"kind":"http"}),
                error: Some(
                    match error.as_str() {
                        "too_large" => "too_large",
                        "bad_params" => "bad_params",
                        "not_found" => "not_found",
                        _ => "data_unavailable",
                    }
                    .into(),
                ),
            }),
        }
    }
    pub async fn public_sources(&self, org: Option<&str>) -> Vec<PublicSource> {
        let mut sources = self
            .sources
            .values()
            .filter(|s| org.is_none_or(|o| o == s.org))
            .cloned()
            .collect::<Vec<_>>();
        if let Ok(rows) = self.managed_rows().await {
            sources.extend(
                rows.into_iter()
                    .filter(|(s, enabled, _)| {
                        *enabled
                            && Self::missing_references(s).is_empty()
                            && org.is_none_or(|o| o == s.org)
                    })
                    .map(|(s, _, _)| s),
            );
        }
        sources.sort_by(|a, b| a.id.cmp(&b.id));
        sources
            .into_iter()
            .map(|s| PublicSource {
                id: s.id.clone(),
                kind: s.kind.clone(),
                operations: s
                    .operations
                    .iter()
                    .map(|(n, o)| (n.clone(), json!({"params":public_params(&o.params)})))
                    .collect(),
                subscriptions: s
                    .subscriptions
                    .iter()
                    .map(|(n, o)| (n.clone(), json!({"transport":o.transport})))
                    .collect(),
            })
            .collect()
    }
    async fn all_source_configs(&self) -> Result<BTreeMap<String, SourceConfig>, String> {
        let mut map = (*self.sources).clone();
        for (source, _, _) in self.managed_rows().await? {
            if map.contains_key(&source.id) {
                return Err("source_conflict".into());
            }
            map.insert(source.id.clone(), source);
        }
        Ok(map)
    }
    pub async fn validate_registry(&self) -> Result<(), String> {
        let mut ids = HashSet::new();
        for source in self.sources.values() {
            validate_source(source)?;
            ids.insert(source.id.clone());
        }
        let rows = self.managed_rows().await?;
        if self.sources.len() + rows.len() > MAX_SOURCES {
            return Err("invalid_data_sources".into());
        }
        for (source, _, _) in rows {
            validate_source(&source)?;
            if !ids.insert(source.id) {
                return Err("source_conflict".into());
            }
        }
        Ok(())
    }
    async fn load_bindings(&self, id: &str) -> Result<(String, BindingManifest), String> {
        if let Some(pool) = &self.pool {
            let id = id.to_owned();
            let row=interact(pool,move|c| c.query_row("SELECT b.org,b.bindings,a.org FROM artifact_data_bindings b JOIN artifacts a ON a.id=b.artifact_id WHERE b.artifact_id=?1",[id],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?))).optional().map_err(|_|AppError::Internal)).await.map_err(|_|"data_unavailable")?;
            let Some((org, raw, current)) = row else {
                return Ok((String::new(), BindingManifest::default()));
            };
            if org != current {
                return Err("not_found".into());
            }
            let manifest: BindingManifest =
                serde_json::from_str(&raw).map_err(|_| "data_unavailable")?;
            return Ok((org, manifest));
        }
        Ok(self
            .memory
            .read()
            .await
            .bindings
            .get(id)
            .cloned()
            .unwrap_or_default())
    }
    pub async fn try_bindings(&self, id: &str) -> Result<BindingManifest, String> {
        let (org, m) = self.load_bindings(id).await?;
        let sources = self.all_source_configs().await?;
        validate_bindings(&m, &sources, &org).map_err(|_| "not_found")?;
        Ok(m)
    }
    pub async fn get_bindings(&self, id: &str) -> BindingManifest {
        self.try_bindings(id).await.unwrap_or_default()
    }
    pub async fn set_bindings(
        &self,
        id: &str,
        manifest: BindingManifest,
        org: &str,
    ) -> Result<(), String> {
        self.set_bindings_inner(id, manifest, org, None).await
    }
    pub async fn set_bindings_audited(
        &self,
        id: &str,
        manifest: BindingManifest,
        org: &str,
        audit: MutationAudit,
    ) -> Result<(), String> {
        let audit = audit
            .for_target_tenant(org)
            .map_err(|_| "data_unavailable".to_owned())?;
        self.set_bindings_inner(id, manifest, org, Some(audit))
            .await
    }
    async fn set_bindings_inner(
        &self,
        id: &str,
        manifest: BindingManifest,
        org: &str,
        audit: Option<MutationAudit>,
    ) -> Result<(), String> {
        let _guard = self.writes.lock().await;
        let sources = self.all_source_configs().await?;
        validate_bindings(&manifest, &sources, org)?;
        if let Some(pool) = &self.pool {
            let id = id.to_owned();
            let org = org.to_owned();
            let raw = serde_json::to_string(&manifest).map_err(|_| "bad_params")?;
            let audit_key = if audit.is_some() {
                Some(self.audit_key()?)
            } else {
                None
            };
            interact(pool,move|c|{
                // Reserve the writer before reading so other WAL writers cannot stale our snapshot.
                let t=c.transaction_with_behavior(TransactionBehavior::Immediate).map_err(|_|AppError::Internal)?;
                let previous=t.query_row("SELECT org,bindings FROM artifact_data_bindings WHERE artifact_id=?1",[&id],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?))).optional().map_err(|_|AppError::Internal)?;
                let mut removed=Vec::new();
                if let Some((previous_org,previous_raw))=previous {
                    let old:BindingManifest=serde_json::from_str(&previous_raw).map_err(|_|AppError::Internal)?;
                    let next:BindingManifest=serde_json::from_str(&raw).map_err(|_|AppError::Internal)?;
                    removed=old.bindings.iter().filter(|(name,b)|previous_org!=org || next.bindings.get(*name)!=Some(*b)).map(|(n,_)|n.clone()).collect();
                }
                for binding in removed {
                    t.execute("DELETE FROM artifact_data_snapshots WHERE artifact_id=?1 AND binding=?2",(&id,&binding)).map_err(|_|AppError::Internal)?;
                    t.execute("DELETE FROM artifact_data_events WHERE artifact_id=?1 AND binding=?2",(&id,&binding)).map_err(|_|AppError::Internal)?;
                }
                t.execute("INSERT INTO artifact_data_bindings(artifact_id,org,bindings,updated_at) VALUES(?1,?2,?3,datetime('now')) ON CONFLICT(artifact_id) DO UPDATE SET org=excluded.org,bindings=excluded.bindings,updated_at=excluded.updated_at",(&id,&org,&raw)).map_err(|_|AppError::Internal)?;
                if let (Some(audit), Some(key)) = (audit, audit_key) {
                    let event = AuditEvent { operation: "data_bindings.update".into(), target_type: "artifact".into(), target_id: id.clone(), result: "success".into(), classification: "admin".into(), revision: None };
                    append_in_transaction(&t, &key, &audit.event_id().map_err(|_| AppError::Internal)?, audit.context(), &event)?;
                }
                t.commit().map_err(|_|AppError::Internal)
            }).await.map_err(|_|"data_unavailable")?;
        } else {
            let mut memory = self.memory.write().await;
            let (old_org, old) = memory.bindings.get(id).cloned().unwrap_or_default();
            let removed: Vec<String> = old
                .bindings
                .iter()
                .filter(|(n, b)| old_org != org || manifest.bindings.get(*n) != Some(*b))
                .map(|(n, _)| n.clone())
                .collect();

            memory
                .snapshots
                .retain(|(a, b, _), _| a != id || !removed.contains(b));
            memory
                .events
                .retain(|(a, b, _), _| a != id || !removed.contains(b));
            memory.bindings.insert(id.into(), (org.into(), manifest));
        }
        Ok(())
    }
    async fn binding_source(
        &self,
        id: &str,
        binding: &str,
    ) -> Result<(Binding, SourceConfig), String> {
        let (org, manifest) = self.load_bindings(id).await?;
        let b = manifest.bindings.get(binding).ok_or("not_found")?.clone();
        let (s, enabled, _, _) = self.source_record(&b.source).await?;
        if s.org != org {
            return Err("not_found".into());
        }
        if !enabled {
            return Err("data_unavailable".into());
        }
        Ok((b, s))
    }
    pub async fn query(
        &self,
        id: &str,
        binding: &str,
        operation: &str,
        params: Value,
    ) -> Result<Value, String> {
        let (b, s) = self.binding_source(id, binding).await?;
        if !b.operations.iter().any(|n| n == operation) {
            return Err("not_found".into());
        }
        let o = s.operations.get(operation).ok_or("not_found")?;
        if s.kind == "push" {
            if !params.as_object().is_some_and(|p| p.is_empty()) {
                return Err("bad_params".into());
            }
            let key = o.key.as_deref().unwrap_or(operation);
            if let Some(pool) = &self.pool {
                let a = id.to_owned();
                let b = binding.to_owned();
                let k = key.to_owned();
                let raw=interact(pool,move|c|c.query_row("SELECT value FROM artifact_data_snapshots WHERE artifact_id=?1 AND binding=?2 AND key=?3",(&a,&b,&k),|r|r.get::<_,String>(0)).optional().map_err(|_|AppError::Internal)).await.map_err(|_|"data_unavailable")?.ok_or("not_found")?;
                return serde_json::from_str(&raw).map_err(|_| "data_unavailable".into());
            }
            return self
                .memory
                .read()
                .await
                .snapshots
                .get(&(id.into(), binding.into(), key.into()))
                .map(|(_, v)| v.clone())
                .ok_or("not_found".into());
        }
        let result = self.http_query(&s, o, &params).await;
        self.record_query_health(&s.id, result.as_ref().map(|_| ()).map_err(|e| e.as_str()))
            .await;
        result
    }
    async fn record_query_health(&self, id: &str, result: Result<(), &str>) {
        let mut health = self.health.write().await;
        let entry = health.entry(id.to_owned()).or_default();
        entry.last_query_at = Some(
            time::OffsetDateTime::now_utc()
                .format(&time::format_description::well_known::Rfc3339)
                .unwrap_or_else(|_| "".into()),
        );
        match result {
            Ok(()) => {
                entry.state = "connected".into();
                entry.last_success_at = entry.last_query_at.clone();
                entry.error = None;
                entry.retry_count = 0;
            }
            Err(error) => {
                entry.state = "unavailable".into();
                entry.error = Some(
                    match error {
                        "too_large" => "too_large",
                        _ => "data_unavailable",
                    }
                    .into(),
                );
                entry.retry_count = entry.retry_count.saturating_add(1);
            }
        }
    }
    async fn record_event_health(&self, id: &str, result: Result<(), &str>) {
        let mut health = self.health.write().await;
        let entry = health.entry(id.to_owned()).or_default();
        let now = time::OffsetDateTime::now_utc()
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap_or_default();
        match result {
            Ok(()) => {
                entry.state = "connected".into();
                entry.last_event_at = Some(now.clone());
                entry.last_success_at = Some(now);
                entry.error = None;
                entry.retry_count = 0;
            }
            Err(error) => {
                entry.state = "unavailable".into();
                entry.error = Some(if error == "too_large" {
                    "too_large".into()
                } else {
                    "data_unavailable".into()
                });
                entry.retry_count = entry.retry_count.saturating_add(1);
            }
        }
    }
    async fn http_query(
        &self,
        s: &SourceConfig,
        o: &OperationConfig,
        params: &Value,
    ) -> Result<Value, String> {
        let url = build_url(s, o, params)?;
        let mut response = self
            .client
            .get(url)
            .headers(Self::resolved_headers(s)?)
            .timeout(Duration::from_millis(o.timeout_ms))
            .send()
            .await
            .map_err(|_| "data_unavailable")?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Err("not_found".into());
        }
        if !response.status().is_success() {
            return Err("data_unavailable".into());
        }
        if response
            .content_length()
            .is_some_and(|n| n > o.max_bytes as u64)
        {
            return Err("too_large".into());
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| "data_unavailable")? {
            if bytes.len().saturating_add(chunk.len()) > o.max_bytes {
                return Err("too_large".into());
            }
            bytes.extend_from_slice(&chunk);
        }
        serde_json::from_slice(&bytes).map_err(|_| "data_unavailable".into())
    }
    pub async fn set_snapshot(
        &self,
        id: &str,
        binding: &str,
        key: &str,
        value: Value,
    ) -> Result<u64, String> {
        let raw = serde_json::to_string(&value).map_err(|_| "bad_params")?;
        if raw.len() > 256 * 1024 {
            return Err("too_large".into());
        }
        let _guard = self.writes.lock().await;
        let (b, s) = self.binding_source(id, binding).await?;
        if s.kind != "push"
            || !b.operations.iter().any(|n| {
                s.operations
                    .get(n)
                    .is_some_and(|o| o.key.as_deref().unwrap_or(n) == key)
            })
        {
            return Err("not_found".into());
        }
        if let Some(pool) = &self.pool {
            let a = id.to_owned();
            let b = binding.to_owned();
            let k = key.to_owned();
            return interact(pool,move|c|{
                c.query_row("INSERT INTO artifact_data_snapshots(artifact_id,binding,key,value,revision,updated_at) VALUES(?1,?2,?3,?4,1,datetime('now')) ON CONFLICT(artifact_id,binding,key) DO UPDATE SET value=excluded.value,revision=revision+1,updated_at=excluded.updated_at RETURNING revision",(&a,&b,&k,&raw),|r|r.get::<_,i64>(0).map(|v|v as u64)).map_err(|_|AppError::Internal)
            }).await.map_err(|_|"data_unavailable".into());
        }
        let mut memory = self.memory.write().await;
        let k = (id.into(), binding.into(), key.into());
        let revision = memory.snapshots.get(&k).map_or(1, |(r, _)| r + 1);
        memory.snapshots.insert(k, (revision, value));
        Ok(revision)
    }
    pub async fn append_events(
        &self,
        id: &str,
        binding: &str,
        sub: &str,
        items: Vec<Event>,
    ) -> Result<(usize, usize), String> {
        if items.len() > 100
            || items
                .iter()
                .any(|e| !valid_event_text(&e.id) || !valid_event_text(&e.event))
        {
            return Err("bad_params".into());
        }
        if serde_json::to_vec(&items).map_err(|_| "bad_params")?.len() > MAX_BYTES {
            return Err("too_large".into());
        }
        let _guard = self.writes.lock().await;
        let (b, s) = self.binding_source(id, binding).await?;
        if s.kind != "push" || !b.subscriptions.iter().any(|n| n == sub) {
            return Err("not_found".into());
        }
        if let Some(pool) = &self.pool {
            let a = id.to_owned();
            let b = binding.to_owned();
            let sub = sub.to_owned();
            return interact(pool,move|c|{
                let t=c.transaction().map_err(|_|AppError::Internal)?;let mut accepted=0;
                for e in &items {
                    let raw=serde_json::to_string(&e.data).map_err(|_|AppError::Internal)?;
                    accepted+=t.execute("INSERT OR IGNORE INTO artifact_data_events(artifact_id,binding,subscription,event_id,event_name,data,created_at) VALUES(?1,?2,?3,?4,?5,?6,datetime('now'))",(&a,&b,&sub,&e.id,&e.event,&raw)).map_err(|_|AppError::Internal)?;
                }
                t.execute("DELETE FROM artifact_data_events WHERE artifact_id=?1 AND binding=?2 AND subscription=?3 AND rowid NOT IN(SELECT rowid FROM artifact_data_events WHERE artifact_id=?1 AND binding=?2 AND subscription=?3 ORDER BY rowid DESC LIMIT 1000)",(&a,&b,&sub)).map_err(|_|AppError::Internal)?;
                t.commit().map_err(|_|AppError::Internal)?;Ok((accepted,items.len()-accepted))
            }).await.map_err(|_|"data_unavailable".into());
        }
        let mut memory = self.memory.write().await;
        let list = memory
            .events
            .entry((id.into(), binding.into(), sub.into()))
            .or_default();
        let mut accepted = 0;
        for e in &items {
            if !list.iter().any(|x| x.id == e.id) {
                list.push(e.clone());
                accepted += 1;
            }
        }
        if list.len() > MAX_EVENTS {
            list.drain(..list.len() - MAX_EVENTS);
        }
        Ok((accepted, items.len() - accepted))
    }
    async fn stored_events(
        &self,
        id: &str,
        binding: &str,
        sub: &str,
    ) -> Result<Vec<Event>, String> {
        if let Some(pool) = &self.pool {
            let a = id.to_owned();
            let b = binding.to_owned();
            let s = sub.to_owned();
            return interact(pool,move|c|{
                let mut stmt=c.prepare("SELECT event_id,event_name,data FROM artifact_data_events WHERE artifact_id=?1 AND binding=?2 AND subscription=?3 ORDER BY rowid").map_err(|_|AppError::Internal)?;
                let rows=stmt.query_map((&a,&b,&s),|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?))).map_err(|_|AppError::Internal)?;
                let mut out=Vec::new();for row in rows { let (id,event,raw)=row.map_err(|_|AppError::Internal)?;out.push(Event{id,event,data:serde_json::from_str(&raw).map_err(|_|AppError::Internal)?}); } Ok(out)
            }).await.map_err(|_|"data_unavailable".into());
        }
        Ok(self
            .memory
            .read()
            .await
            .events
            .get(&(id.into(), binding.into(), sub.into()))
            .cloned()
            .unwrap_or_default())
    }
    pub async fn stream(
        &self,
        id: &str,
        topics: Vec<RequestedSubscription>,
        cursor: BTreeMap<String, String>,
    ) -> Result<DataStream, String> {
        if topics.is_empty() || topics.len() > 16 {
            return Err("bad_params".into());
        }
        let mut unique = HashSet::new();
        let mut selected = Vec::new();
        for topic in topics {
            if !valid_name(&topic.binding)
                || !valid_name(&topic.subscription)
                || !unique.insert(format!("{}:{}", topic.binding, topic.subscription))
            {
                return Err("bad_params".into());
            }
            let (binding, source) = self.binding_source(id, &topic.binding).await?;
            if !binding.subscriptions.contains(&topic.subscription) {
                return Err("not_found".into());
            }
            let sub = source
                .subscriptions
                .get(&topic.subscription)
                .ok_or("not_found")?
                .clone();
            selected.push((topic, binding, source, sub));
        }
        let permit = self
            .stream_slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| "data_unavailable")?;
        let (tx, receiver) = mpsc::channel(32);
        let mut tasks = Vec::new();
        for (topic, binding, source, sub) in selected {
            let broker = self.clone();
            let tx = tx.clone();
            let id = id.to_owned();
            let previous = cursor
                .get(&format!("{}:{}", topic.binding, topic.subscription))
                .cloned();
            tasks.push(tokio::spawn(async move {
                broker
                    .run_subscription(id, topic, binding, source, sub, previous, tx)
                    .await;
            }));
        }
        Ok(DataStream {
            receiver,
            tasks,
            _permit: permit,
        })
    }
    async fn current_binding(
        &self,
        id: &str,
        topic: &RequestedSubscription,
        expected: &Binding,
    ) -> bool {
        let Ok((org, manifest)) = self.load_bindings(id).await else {
            return false;
        };
        let Some(current) = manifest.bindings.get(&topic.binding) else {
            return false;
        };
        self.source_record(&current.source)
            .await
            .is_ok_and(|(source, _, _, _)| {
                source.org == org
                    && current == expected
                    && current.subscriptions.contains(&topic.subscription)
            })
    }
    async fn current_source(
        &self,
        id: &str,
        binding: &Binding,
        subscription: &str,
        source: &SourceConfig,
    ) -> Option<(SourceConfig, SubscriptionConfig)> {
        let current = self.source_record(&binding.source).await.ok()?;
        if current.0.org != self.load_bindings(id).await.ok()?.0 {
            return None;
        }
        if serde_json::to_vec(&current.0).ok() == serde_json::to_vec(source).ok() {
            return None;
        }
        Some((
            current.0.clone(),
            current.0.subscriptions.get(subscription).cloned()?,
        ))
    }
    #[allow(clippy::too_many_arguments)] // One resolved subscription and its cancellation channel.
    async fn run_subscription(
        &self,
        id: String,
        topic: RequestedSubscription,
        binding: Binding,
        mut source: SourceConfig,
        mut sub: SubscriptionConfig,
        mut cursor: Option<String>,
        tx: mpsc::Sender<DataEnvelope>,
    ) {
        let mut backoff = 1;
        loop {
            if tx.is_closed() || !self.current_binding(&id, &topic, &binding).await {
                break;
            }
            if let Ok((_, enabled, _, _)) = self.source_record(&binding.source).await
                && !enabled
            {
                if emit(&tx, &topic, "data:status", "", json!({"state":"disabled"}))
                    .await
                    .is_err()
                {
                    break;
                }
                tokio::select! { _=tx.closed()=>break, _=tokio::time::sleep(Duration::from_secs(2))=>{} }
                continue;
            }
            if let Some((changed_source, changed_sub)) = self
                .current_source(&id, &binding, &topic.subscription, &source)
                .await
            {
                source = changed_source;
                sub = changed_sub;
                cursor = None;
                if emit(
                    &tx,
                    &topic,
                    "data:resync",
                    "",
                    json!({"reason":"source_changed"}),
                )
                .await
                .is_err()
                {
                    break;
                }
            }
            let result = match sub.transport.as_str() {
                "push" => {
                    self.push_stream(&id, &topic, &binding, &mut cursor, &tx)
                        .await
                }
                "poll" => {
                    self.poll_stream(&id, &topic, &binding, &source, &sub, &tx)
                        .await
                }
                _ => {
                    self.sse_stream(&id, &topic, &binding, &source, &sub, &mut cursor, &tx)
                        .await
                }
            };
            if sub.transport == "sse"
                && let Err(error) = &result
            {
                self.record_event_health(&binding.source, Err(error)).await;
            }
            if tx.is_closed() || !self.current_binding(&id, &topic, &binding).await {
                break;
            }
            let state = if result.is_err() {
                "unavailable"
            } else {
                "reconnecting"
            };
            if emit(&tx, &topic, "data:status", "", json!({"state":state}))
                .await
                .is_err()
            {
                break;
            }
            tokio::select! { _=tx.closed()=>break, _=tokio::time::sleep(Duration::from_secs(backoff))=>{} }
            backoff = (backoff * 2).min(30);
        }
    }
    async fn push_stream(
        &self,
        id: &str,
        topic: &RequestedSubscription,
        binding: &Binding,
        cursor: &mut Option<String>,
        tx: &mpsc::Sender<DataEnvelope>,
    ) -> Result<(), String> {
        emit(tx, topic, "data:status", "", json!({"state":"connected"})).await?;
        loop {
            if !self.current_binding(id, topic, binding).await {
                return Ok(());
            }
            if !self
                .source_record(&binding.source)
                .await
                .is_ok_and(|(_, enabled, _, _)| enabled)
            {
                return Ok(());
            }
            let events = self
                .stored_events(id, &topic.binding, &topic.subscription)
                .await?;
            let start = if let Some(previous) = cursor.as_ref() {
                if let Some(pos) = events.iter().position(|e| &e.id == previous) {
                    pos + 1
                } else {
                    emit(tx, topic, "data:resync", "", json!({"reason":"gap"})).await?;
                    *cursor = None;
                    0
                }
            } else {
                0
            };
            for e in events.into_iter().skip(start) {
                self.record_event_health(&binding.source, Ok(())).await;
                emit(tx, topic, &e.event, &e.id, e.data).await?;
                *cursor = Some(e.id);
            }
            tokio::select! { _=tx.closed()=>return Ok(()), _=tokio::time::sleep(Duration::from_millis(250))=>{} }
        }
    }
    async fn poll_stream(
        &self,
        id: &str,
        topic: &RequestedSubscription,
        binding: &Binding,
        source: &SourceConfig,
        sub: &SubscriptionConfig,
        tx: &mpsc::Sender<DataEnvelope>,
    ) -> Result<(), String> {
        let operation = source
            .operations
            .get(sub.operation.as_deref().ok_or("data_unavailable")?)
            .ok_or("data_unavailable")?;
        loop {
            if !self.current_binding(id, topic, binding).await {
                return Ok(());
            }
            if !self
                .source_record(&binding.source)
                .await
                .is_ok_and(|(_, enabled, _, _)| enabled)
            {
                return Ok(());
            }
            let result = self.http_query(source, operation, &json!({})).await;
            self.record_query_health(
                &binding.source,
                result.as_ref().map(|_| ()).map_err(|error| error.as_str()),
            )
            .await;
            let value = result?;
            emit(tx, topic, "data:status", "", json!({"state":"connected"})).await?;
            self.record_event_health(&binding.source, Ok(())).await;
            emit(tx, topic, "message", "", value).await?;
            tokio::select! { _=tx.closed()=>return Ok(()), _=tokio::time::sleep(Duration::from_millis(sub.interval_ms))=>{} }
        }
    }
    #[allow(clippy::too_many_arguments)] // The connector needs its binding for revocation checks.
    async fn sse_stream(
        &self,
        id: &str,
        topic: &RequestedSubscription,
        binding: &Binding,
        source: &SourceConfig,
        sub: &SubscriptionConfig,
        cursor: &mut Option<String>,
        tx: &mpsc::Sender<DataEnvelope>,
    ) -> Result<(), String> {
        let mut url = Url::parse(source.base_url.as_deref().ok_or("data_unavailable")?)
            .map_err(|_| "data_unavailable")?;
        url.set_path(&format!(
            "{}{}",
            url.path().trim_end_matches('/'),
            sub.path.as_deref().ok_or("data_unavailable")?
        ));
        let mut request = self
            .client
            .get(url)
            .headers(Self::resolved_headers(source)?)
            .header("accept", "text/event-stream");
        if let Some(previous) = cursor.as_ref() {
            request = request.header("last-event-id", previous);
        }
        let mut response = tokio::time::timeout(Duration::from_secs(10), request.send())
            .await
            .map_err(|_| "data_unavailable")?
            .map_err(|_| "data_unavailable")?;
        if !response.status().is_success()
            || !response
                .headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .is_some_and(|s| s.starts_with("text/event-stream"))
        {
            return Err("data_unavailable".into());
        }
        emit(tx, topic, "data:status", "", json!({"state":"connected"})).await?;
        let mut activity = tokio::time::Instant::now();
        let mut pending = Vec::new();
        let mut event = String::new();
        let mut data = String::new();
        let mut event_id = String::new();
        let mut frame_bytes = 0;
        let mut check = tokio::time::interval(Duration::from_secs(2));
        loop {
            let chunk = tokio::select! {
                _=tx.closed()=>return Ok(()),
                _=check.tick()=>{if !self.current_binding(id,topic,binding).await || !self.source_record(&binding.source).await.is_ok_and(|(_, enabled, _, _)| enabled) {return Ok(())}
                    if self.current_source(id,binding,&topic.subscription,source).await.is_some() {return Ok(())}
                    if activity.elapsed()>Duration::from_secs(45){return Err("data_unavailable".into())}continue;},
                chunk=response.chunk()=>chunk.map_err(|_|"data_unavailable")?,
            };
            let Some(chunk) = chunk else { return Ok(()) };
            activity = tokio::time::Instant::now();
            if pending.len() + chunk.len() > MAX_BYTES {
                return Err("too_large".into());
            }
            pending.extend_from_slice(&chunk);
            while let Some(pos) = pending.iter().position(|b| *b == b'\n') {
                let bytes: Vec<u8> = pending.drain(..=pos).collect();
                frame_bytes += bytes.len();
                if frame_bytes > MAX_BYTES {
                    return Err("too_large".into());
                }
                let line = std::str::from_utf8(&bytes[..pos])
                    .map_err(|_| "data_unavailable")?
                    .trim_end_matches('\r');
                if line.is_empty() {
                    let kind = if event.is_empty() { "message" } else { &event };
                    if !data.is_empty()
                        && (sub.events.is_empty() && kind == "message"
                            || sub.events.iter().any(|s| s == kind))
                    {
                        let text = data.strip_suffix('\n').unwrap_or(&data);
                        let value = serde_json::from_str(text)
                            .unwrap_or_else(|_| Value::String(text.to_owned()));
                        self.record_event_health(&binding.source, Ok(())).await;
                        emit(tx, topic, kind, &event_id, value).await?;
                        if !event_id.is_empty() {
                            *cursor = Some(event_id.clone());
                        }
                    }
                    event.clear();
                    data.clear();
                    event_id.clear();
                    frame_bytes = 0;
                } else if !line.starts_with(':') {
                    let (field, value) = line.split_once(':').unwrap_or((line, ""));
                    let value = value.strip_prefix(' ').unwrap_or(value);
                    match field {
                        "event" => event = value.into(),
                        "data" => {
                            data.push_str(value);
                            data.push('\n');
                        }
                        "id" if valid_event_text(value) => event_id = value.into(),
                        _ => {}
                    }
                }
            }
        }
    }
}
async fn emit(
    tx: &mpsc::Sender<DataEnvelope>,
    topic: &RequestedSubscription,
    event: &str,
    id: &str,
    data: Value,
) -> Result<(), String> {
    tx.send(DataEnvelope {
        binding: topic.binding.clone(),
        subscription: topic.subscription.clone(),
        event: event.into(),
        id: id.into(),
        data,
    })
    .await
    .map_err(|_| "data_unavailable".into())
}
fn public_params(params: &BTreeMap<String, ParamConfig>) -> BTreeMap<String, Value> {
    params
        .iter()
        .map(|(name, p)| {
            let mut schema = serde_json::Map::new();
            schema.insert("type".into(), json!(p.kind));
            if p.required {
                schema.insert("required".into(), json!(true));
            }
            if let Some(value) = p.minimum {
                schema.insert("minimum".into(), json!(value));
            }
            if let Some(value) = p.maximum {
                schema.insert("maximum".into(), json!(value));
            }
            if p.kind == "string" {
                schema.insert("max_length".into(), json!(p.max_length.unwrap_or(256)));
            }
            if let Some(value) = &p.enum_values {
                schema.insert("enum".into(), json!(value));
            }
            if let Some(value) = &p.default {
                schema.insert("default".into(), value.clone());
            }
            (name.clone(), Value::Object(schema))
        })
        .collect()
}
fn valid_event_text(s: &str) -> bool {
    !s.is_empty() && s.len() <= 128 && !s.chars().any(char::is_control)
}
fn bound_source_count(tx: &rusqlite::Transaction<'_>, source: &str) -> rusqlite::Result<i64> {
    let mut stmt = tx.prepare("SELECT bindings FROM artifact_data_bindings b JOIN artifacts a ON a.id=b.artifact_id WHERE a.org=b.org")?;
    let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
    let mut count = 0;
    for row in rows {
        let raw = row?;
        if serde_json::from_str::<BindingManifest>(&raw)
            .ok()
            .is_some_and(|m| m.bindings.values().any(|b| b.source == source))
        {
            count += 1;
        }
    }
    Ok(count)
}
fn bound_capability_count(
    tx: &rusqlite::Transaction<'_>,
    source: &str,
    next_raw: &str,
) -> rusqlite::Result<i64> {
    let next: SourceConfig =
        serde_json::from_str(next_raw).map_err(|_| rusqlite::Error::InvalidQuery)?;
    let mut stmt = tx.prepare("SELECT bindings FROM artifact_data_bindings b JOIN artifacts a ON a.id=b.artifact_id WHERE a.org=b.org")?;
    let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
    let mut count = 0;
    for row in rows {
        let raw = row?;
        let Ok(manifest) = serde_json::from_str::<BindingManifest>(&raw) else {
            continue;
        };
        for binding in manifest.bindings.values().filter(|b| b.source == source) {
            if binding
                .operations
                .iter()
                .any(|n| !next.operations.contains_key(n))
                || binding
                    .subscriptions
                    .iter()
                    .any(|n| !next.subscriptions.contains_key(n))
            {
                count += 1;
            }
        }
    }
    Ok(count)
}
fn valid_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
}
fn valid_path(s: &str) -> bool {
    s.starts_with('/')
        && !s.starts_with("//")
        && !s.contains('\\')
        && !s.contains('?')
        && !s.contains('#')
        && !s.chars().any(char::is_control)
        && !s.split('/').any(|p| p == "." || p == "..")
        && !s.to_ascii_lowercase().contains("%2e")
        && !s.to_ascii_lowercase().contains("%2f")
        && !s.to_ascii_lowercase().contains("%5c")
}
fn validate_source(s: &SourceConfig) -> Result<(), String> {
    let error = || "invalid_data_sources".to_owned();
    if !valid_name(&s.id)
        || s.org.trim().is_empty()
        || !matches!(s.kind.as_str(), "http" | "push")
        || s.operations.len() > 16
        || s.subscriptions.len() > 16
    {
        return Err(error());
    }
    if s.kind == "http" {
        let u = Url::parse(s.base_url.as_deref().ok_or_else(error)?).map_err(|_| error())?;
        if !matches!(u.scheme(), "http" | "https")
            || u.host_str().is_none()
            || !u.username().is_empty()
            || u.password().is_some()
            || u.query().is_some()
            || u.fragment().is_some()
        {
            return Err(error());
        }
    } else if s.base_url.is_some() || !s.headers_env.is_empty() {
        return Err(error());
    }
    for (n, o) in &s.operations {
        if !valid_name(n)
            || o.max_bytes == 0
            || o.max_bytes > MAX_BYTES
            || !(100..=30000).contains(&o.timeout_ms)
            || o.params.len() > 16
        {
            return Err(error());
        }
        if s.kind == "push" {
            if o.path.is_some()
                || !o.params.is_empty()
                || o.key.as_ref().is_some_and(|k| !valid_name(k))
            {
                return Err(error());
            }
            continue;
        }
        if o.key.is_some() {
            return Err(error());
        }
        let path = o
            .path
            .as_deref()
            .filter(|p| valid_path(p))
            .ok_or_else(error)?;
        let mut remaining = path.to_owned();
        for (p, c) in &o.params {
            if !valid_name(p)
                || !matches!(c.kind.as_str(), "string" | "integer" | "boolean")
                || c.max_length.is_some_and(|m| m == 0 || m > 256)
                || c.minimum.zip(c.maximum).is_some_and(|(a, b)| a > b)
            {
                return Err(error());
            }
            if c.kind != "string" && c.max_length.is_some()
                || c.kind != "integer" && (c.minimum.is_some() || c.maximum.is_some())
            {
                return Err(error());
            }
            let marker = format!("{{{p}}}");
            if remaining.contains(&marker) {
                if c.kind != "string" || !c.required {
                    return Err(error());
                }
                remaining = remaining.replace(&marker, "");
            }
            if let Some(values) = &c.enum_values
                && (values.is_empty()
                    || values.len() > 100
                    || values.iter().any(|v| validate_param(c, v, false).is_err()))
            {
                return Err(error());
            }
            if let Some(v) = &c.default {
                validate_param(c, v, true).map_err(|_| error())?;
            }
        }
        if remaining.contains('{') || remaining.contains('}') {
            return Err(error());
        }
    }
    for (n, sub) in &s.subscriptions {
        if !valid_name(n)
            || sub.events.len() > 16
            || sub.events.iter().any(|e| !valid_event_text(e))
        {
            return Err(error());
        }
        match sub.transport.as_str() {
            "push" if s.kind == "push" => {
                if sub.path.is_some() || sub.operation.is_some() || !sub.events.is_empty() {
                    return Err(error());
                }
            }
            "sse" if s.kind == "http" => {
                if !sub
                    .path
                    .as_deref()
                    .is_some_and(|p| valid_path(p) && !p.contains('{') && !p.contains('}'))
                    || sub.operation.is_some()
                {
                    return Err(error());
                }
            }
            "poll" if s.kind == "http" => {
                let op = sub
                    .operation
                    .as_ref()
                    .and_then(|n| s.operations.get(n))
                    .ok_or_else(error)?;
                if sub.path.is_some()
                    || !sub.events.is_empty()
                    || !(1000..=60000).contains(&sub.interval_ms)
                    || op
                        .params
                        .values()
                        .any(|p| p.required && p.default.is_none())
                {
                    return Err(error());
                }
            }
            _ => return Err(error()),
        }
    }
    Ok(())
}
fn validate_param(c: &ParamConfig, v: &Value, check_enum: bool) -> Result<String, String> {
    let value = match c.kind.as_str() {
        "string" => {
            let s = v.as_str().ok_or("bad_params")?;
            if s.chars().count() > c.max_length.unwrap_or(256) {
                return Err("bad_params".into());
            }
            s.to_owned()
        }
        "integer" => {
            let i = v.as_i64().ok_or("bad_params")?;
            if c.minimum.is_some_and(|m| i < m) || c.maximum.is_some_and(|m| i > m) {
                return Err("bad_params".into());
            }
            i.to_string()
        }
        "boolean" => v.as_bool().ok_or("bad_params")?.to_string(),
        _ => return Err("bad_params".into()),
    };
    if check_enum && c.enum_values.as_ref().is_some_and(|e| !e.contains(v)) {
        return Err("bad_params".into());
    }
    Ok(value)
}
fn validate_bindings(
    m: &BindingManifest,
    sources: &BTreeMap<String, SourceConfig>,
    org: &str,
) -> Result<(), String> {
    if m.bindings.len() > MAX_BINDINGS {
        return Err("bad_params".into());
    }
    for (n, b) in &m.bindings {
        let s = sources
            .get(&b.source)
            .filter(|s| s.org == org)
            .ok_or("bad_params")?;
        if !valid_name(n)
            || b.operations.iter().collect::<HashSet<_>>().len() != b.operations.len()
            || b.subscriptions.iter().collect::<HashSet<_>>().len() != b.subscriptions.len()
            || b.operations.iter().any(|n| !s.operations.contains_key(n))
            || b.subscriptions
                .iter()
                .any(|n| !s.subscriptions.contains_key(n))
        {
            return Err("bad_params".into());
        }
    }
    Ok(())
}
fn build_url(s: &SourceConfig, o: &OperationConfig, params: &Value) -> Result<Url, String> {
    let object = params.as_object().ok_or("bad_params")?;
    if object.keys().any(|p| !o.params.contains_key(p)) {
        return Err("bad_params".into());
    }
    let mut url = Url::parse(s.base_url.as_deref().ok_or("data_unavailable")?)
        .map_err(|_| "data_unavailable")?;
    let mut path = o.path.clone().ok_or("data_unavailable")?;
    let original = path.clone();
    let mut query = Vec::new();
    for (p, c) in &o.params {
        let Some(v) = object.get(p).or(c.default.as_ref()) else {
            if c.required {
                return Err("bad_params".into());
            }
            continue;
        };
        let value = validate_param(c, v, true)?;
        let marker = format!("{{{p}}}");
        if original.contains(&marker) {
            if value == "."
                || value == ".."
                || value.contains('/')
                || value.contains('\\')
                || value.chars().any(char::is_control)
            {
                return Err("bad_params".into());
            }
            let encoded: String = value
                .as_bytes()
                .iter()
                .map(|b| {
                    if b.is_ascii_alphanumeric() || b"-._~".contains(b) {
                        (*b as char).to_string()
                    } else {
                        format!("%{b:02X}")
                    }
                })
                .collect();
            path = path.replace(&marker, &encoded);
        } else {
            query.push((p, value));
        }
    }
    url.set_path(&format!("{}{}", url.path().trim_end_matches('/'), path));
    for (p, v) in query {
        url.query_pairs_mut().append_pair(p, &v);
    }
    Ok(url)
}
