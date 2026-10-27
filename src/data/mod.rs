//! Named, tenant-scoped live sources. Credentials and network URLs stay in the server.
//!
//! `DataBroker` is one struct whose `impl` blocks live in the submodules by concern:
//! registry loading, managed-source administration, bindings, the outbound HTTP transport,
//! the push event store, and viewer streams. HTTP and MCP callers reach it through
//! [`crate::ports::data::DataService`].
mod bindings;
mod events;
mod managed;
mod registry;
mod service;
mod stream;
mod transport;
mod types;
mod validate;

pub use types::{
    Binding, BindingManifest, DataEnvelope, Event, ImpactArtifact, OperationConfig, ParamConfig,
    PublicSource, RequestedSubscription, SourceConfig, SourceHealth, SourceView,
    SubscriptionConfig, TestResult,
};

use crate::persistence::db::DbPool;
use reqwest::header::HeaderMap;
use serde_json::Value;
use std::{
    collections::{BTreeMap, HashMap},
    sync::Arc,
    time::Duration,
};
use tokio::sync::{Mutex, OwnedSemaphorePermit, RwLock, Semaphore, mpsc};

pub const MAX_SOURCES: usize = 32;
pub const MAX_BINDINGS: usize = 8;
pub const MAX_EVENTS: usize = 1000;
const MAX_BYTES: usize = 1024 * 1024;
pub const MAX_VIEWER_STREAMS: usize = 64;
/// Longest quiet gap allowed between two lines of a streaming upstream.
const STREAM_IDLE: Duration = Duration::from_secs(30);
/// Largest serialized JSON body a POST operation may send upstream.
const MAX_POST_BODY: usize = 64 * 1024;
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
}
