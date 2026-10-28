//! Source registry, binding, and stream data types.
use super::MAX_BYTES;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

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
    /// ADR-0017: `GET` (query string) or `POST` (typed JSON body).
    #[serde(default = "default_method")]
    pub method: String,
    /// ADR-0017: the upstream answers newline-delimited JSON relayed line by line.
    #[serde(default)]
    pub stream: bool,
}
fn default_method() -> String {
    "GET".into()
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
