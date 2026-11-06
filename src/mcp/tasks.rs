//! Durable state for the MCP Tasks extension.
//!
//! Task files contain operational metadata only: no credentials, artifact content, request
//! arguments, or OAuth claims. Each update is a same-directory write-and-rename so a process
//! restart observes either the previous complete state or the next complete state.

use std::{
    fs::OpenOptions,
    io::Write as _,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use nanoid::nanoid;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

use crate::{
    AppDeps,
    error::{AppError, JsonRpcError, McpError},
    model::{ArtifactId, ClientId, OrgId, PublisherIdentity},
    ports::integrations::PreviewPriority,
    security::access::AccessPolicy,
};

pub const TASKS_EXTENSION: &str = "io.modelcontextprotocol/tasks";
pub const TASK_TTL_MS: u64 = 86_400_000;
pub const TASK_POLL_INTERVAL_MS: u64 = 1_000;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PreviewTask {
    pub task_id: String,
    pub artifact_id: ArtifactId,
    pub client_id: ClientId,
    pub org: OrgId,
    pub role: crate::model::Role,
    pub status: TaskStatus,
    pub status_message: String,
    pub created_at: String,
    pub last_updated_at: String,
    pub ttl_ms: Option<u64>,
    pub poll_interval_ms: u64,
    pub progress_current: u64,
    pub progress_total: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<Value>,
}

impl PreviewTask {
    pub fn publisher(&self) -> PublisherIdentity {
        PublisherIdentity {
            client_id: self.client_id.clone(),
            org: self.org.clone(),
            label: "durable-preview-task".to_owned(),
            role: self.role,
            scopes: None,
        }
    }

    pub fn is_terminal(&self) -> bool {
        matches!(
            self.status,
            TaskStatus::Completed | TaskStatus::Failed | TaskStatus::Cancelled
        )
    }

    pub fn accessible_to(&self, publisher: &PublisherIdentity) -> bool {
        publisher.is_admin() || (publisher.org == self.org && publisher.client_id == self.client_id)
    }

    pub fn wire(&self, creation: bool) -> Value {
        let mut value = json!({
            "resultType": if creation { "task" } else { "complete" },
            "taskId": self.task_id,
            "status": self.status,
            "statusMessage": self.status_message,
            "createdAt": self.created_at,
            "lastUpdatedAt": self.last_updated_at,
            "ttlMs": self.ttl_ms,
            "pollIntervalMs": self.poll_interval_ms,
            "_meta": {
                "com.agentshelf.artifact-mcp/progress": {
                    "current": self.progress_current,
                    "total": self.progress_total
                }
            }
        });
        if let Some(result) = &self.result {
            value["result"] = result.clone();
        }
        if let Some(error) = &self.error {
            value["error"] = error.clone();
        }
        value
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Working,
    Completed,
    Failed,
    Cancelled,
}

#[derive(Debug)]
pub struct PreviewTaskStore {
    root: PathBuf,
    lock: Mutex<()>,
}

pub struct WorkingTasks {
    pub tasks: Vec<PreviewTask>,
    pub skipped: usize,
}

impl PreviewTaskStore {
    async fn blocking<T: Send + 'static>(
        self: &Arc<Self>,
        operation: impl FnOnce(&Self) -> Result<T, AppError> + Send + 'static,
    ) -> Result<T, AppError> {
        let store = Arc::clone(self);
        tokio::task::spawn_blocking(move || operation(&store))
            .await
            .map_err(|error| task_io_error("blocking", error))?
    }

    pub async fn create(
        self: &Arc<Self>,
        artifact_id: ArtifactId,
        publisher: &PublisherIdentity,
    ) -> Result<PreviewTask, AppError> {
        let publisher = publisher.clone();
        self.blocking(move |store| store.create_locked(artifact_id, &publisher))
            .await
    }

    pub async fn get(self: &Arc<Self>, task_id: &str) -> Result<Option<PreviewTask>, AppError> {
        let task_id = task_id.to_owned();
        self.blocking(move |store| store.get_locked(&task_id)).await
    }

    pub async fn working(self: &Arc<Self>) -> Result<WorkingTasks, AppError> {
        self.blocking(Self::working_locked).await
    }

    pub async fn mark_running(
        self: &Arc<Self>,
        task_id: &str,
    ) -> Result<Option<PreviewTask>, AppError> {
        let task_id = task_id.to_owned();
        self.blocking(move |store| store.mark_running_locked(&task_id))
            .await
    }

    pub async fn complete(
        self: &Arc<Self>,
        task_id: &str,
        result: Value,
    ) -> Result<Option<PreviewTask>, AppError> {
        let task_id = task_id.to_owned();
        self.blocking(move |store| store.complete_locked(&task_id, result))
            .await
    }

    pub async fn fail(
        self: &Arc<Self>,
        task_id: &str,
        message: &str,
    ) -> Result<Option<PreviewTask>, AppError> {
        let task_id = task_id.to_owned();
        let message = message.to_owned();
        self.blocking(move |store| store.fail_locked(&task_id, &message))
            .await
    }

    pub async fn cancel(self: &Arc<Self>, task_id: &str) -> Result<Option<PreviewTask>, AppError> {
        let task_id = task_id.to_owned();
        self.blocking(move |store| store.cancel_locked(&task_id))
            .await
    }

    pub fn new(data_dir: impl AsRef<Path>) -> Arc<Self> {
        Arc::new(Self {
            root: data_dir.as_ref().join("tasks"),
            lock: Mutex::new(()),
        })
    }

    fn create_locked(
        &self,
        artifact_id: ArtifactId,
        publisher: &PublisherIdentity,
    ) -> Result<PreviewTask, AppError> {
        let _guard = self.lock.lock().unwrap_or_else(|error| error.into_inner());
        std::fs::create_dir_all(&self.root)
            .map_err(|error| task_io_error("create locked", error))?;
        for _ in 0..16 {
            let now = timestamp();
            let task = PreviewTask {
                task_id: format!("task_{}", nanoid!(20)),
                artifact_id: artifact_id.clone(),
                client_id: publisher.client_id.clone(),
                org: publisher.org.clone(),
                role: publisher.role,
                status: TaskStatus::Working,
                status_message: "Preview regeneration queued".to_owned(),
                created_at: now.clone(),
                last_updated_at: now,
                ttl_ms: Some(TASK_TTL_MS),
                poll_interval_ms: TASK_POLL_INTERVAL_MS,
                progress_current: 0,
                progress_total: 2,
                result: None,
                error: None,
            };
            if !self.path(&task.task_id).exists() {
                self.write_locked(&task)?;
                return Ok(task);
            }
        }
        Err(AppError::Internal)
    }

    fn get_locked(&self, task_id: &str) -> Result<Option<PreviewTask>, AppError> {
        if !valid_task_id(task_id) {
            return Ok(None);
        }
        let _guard = self.lock.lock().unwrap_or_else(|error| error.into_inner());
        self.read_locked(task_id)
    }

    fn working_locked(&self) -> Result<WorkingTasks, AppError> {
        let _guard = self.lock.lock().unwrap_or_else(|error| error.into_inner());
        let entries = match std::fs::read_dir(&self.root) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(WorkingTasks {
                    tasks: Vec::new(),
                    skipped: 0,
                });
            }
            Err(error) => return Err(task_io_error("working locked", error)),
        };
        let mut tasks = Vec::new();
        let mut skipped = 0;
        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    tracing::warn!(error = %error, "could not read preview task directory entry");
                    skipped += 1;
                    continue;
                }
            };
            let name = entry.file_name();
            let Some(task_id) = name.to_str().and_then(|name| name.strip_suffix(".json")) else {
                continue;
            };
            if !valid_task_id(task_id) {
                continue;
            }
            match std::fs::read(entry.path()).and_then(|bytes| {
                serde_json::from_slice::<PreviewTask>(&bytes).map_err(std::io::Error::other)
            }) {
                Ok(task) if !task.is_terminal() => tasks.push(task),
                Ok(_) => {}
                Err(error) => {
                    tracing::warn!(file = %name.to_string_lossy(), error = %error, "could not recover preview task");
                    skipped += 1;
                }
            }
        }
        tasks.sort_by(|left, right| left.created_at.cmp(&right.created_at));
        Ok(WorkingTasks { tasks, skipped })
    }

    fn mark_running_locked(&self, task_id: &str) -> Result<Option<PreviewTask>, AppError> {
        self.transition(task_id, |task| {
            if !task.is_terminal() {
                task.status_message = "Rendering artifact preview".to_owned();
                task.progress_current = 1;
            }
        })
    }

    fn complete_locked(
        &self,
        task_id: &str,
        result: Value,
    ) -> Result<Option<PreviewTask>, AppError> {
        self.transition(task_id, |task| {
            if !task.is_terminal() {
                task.status = TaskStatus::Completed;
                task.status_message = "Preview regeneration completed".to_owned();
                task.progress_current = task.progress_total;
                task.result = Some(result);
                task.error = None;
            }
        })
    }

    fn fail_locked(&self, task_id: &str, message: &str) -> Result<Option<PreviewTask>, AppError> {
        self.transition(task_id, |task| {
            if !task.is_terminal() {
                task.status = TaskStatus::Failed;
                task.status_message = message.to_owned();
                task.error = Some(json!({
                    "code": -32603,
                    "message": message
                }));
                task.result = None;
            }
        })
    }

    fn cancel_locked(&self, task_id: &str) -> Result<Option<PreviewTask>, AppError> {
        self.transition(task_id, |task| {
            if !task.is_terminal() {
                task.status = TaskStatus::Cancelled;
                task.status_message = "Preview regeneration cancelled".to_owned();
                task.result = None;
                task.error = None;
            }
        })
    }

    fn transition(
        &self,
        task_id: &str,
        mutate: impl FnOnce(&mut PreviewTask),
    ) -> Result<Option<PreviewTask>, AppError> {
        if !valid_task_id(task_id) {
            return Ok(None);
        }
        let _guard = self.lock.lock().unwrap_or_else(|error| error.into_inner());
        let Some(mut task) = self.read_locked(task_id)? else {
            return Ok(None);
        };
        mutate(&mut task);
        task.last_updated_at = timestamp();
        self.write_locked(&task)?;
        Ok(Some(task))
    }

    fn read_locked(&self, task_id: &str) -> Result<Option<PreviewTask>, AppError> {
        match std::fs::read(self.path(task_id)) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map(Some)
                .map_err(|error| task_io_error("read locked", error)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(task_io_error("read locked", error)),
        }
    }

    fn write_locked(&self, task: &PreviewTask) -> Result<(), AppError> {
        let bytes =
            serde_json::to_vec(task).map_err(|error| task_io_error("write locked", error))?;
        let temporary = self
            .root
            .join(format!(".{}.{}.tmp", task.task_id, nanoid!(10)));
        let target = self.path(&task.task_id);
        let result = (|| -> Result<(), std::io::Error> {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
            drop(file);
            std::fs::rename(&temporary, target)?;
            Ok(())
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&temporary);
        }
        result.map_err(|error| task_io_error("write locked", error))
    }

    fn path(&self, task_id: &str) -> PathBuf {
        self.root.join(format!("{task_id}.json"))
    }
}

pub fn client_supports_tasks(message: &super::protocol::OrderedJson) -> bool {
    message
        .get("params")
        .and_then(|params| params.get("_meta"))
        .and_then(|meta| meta.get("io.modelcontextprotocol/clientCapabilities"))
        .and_then(|capabilities| capabilities.get("extensions"))
        .and_then(|extensions| extensions.get(TASKS_EXTENSION))
        .and_then(super::protocol::OrderedJson::as_object)
        .is_some()
}

pub async fn regenerate_synchronously(
    artifact_id: &str,
    publisher: &PublisherIdentity,
    deps: &AppDeps,
) -> Result<Value, McpError> {
    match regenerate(artifact_id, publisher, deps).await {
        Ok(result) => Ok(result),
        Err(AppError::Unavailable(reason)) => {
            Ok(tool_result(artifact_id, false, "", Some(reason.as_str())))
        }
        Err(error) => Err(error.into()),
    }
}

pub async fn create_preview_task(
    artifact_id: &str,
    publisher: &PublisherIdentity,
    deps: &AppDeps,
) -> Result<Value, McpError> {
    // Authorization and basic feasibility happen before durable creation, so a task handle never
    // reveals a foreign artifact and a bundle never creates work that cannot run.
    let _ = preview_source(artifact_id, publisher, deps).await?;
    let task = deps
        .preview_tasks
        .create(ArtifactId::from(artifact_id), publisher)
        .await?;
    spawn_preview_task(task.task_id.clone(), deps.clone());
    Ok(task.wire(true))
}

pub async fn resume_preview_tasks(deps: AppDeps) {
    match deps.preview_tasks.working().await {
        Ok(recovery) => {
            if recovery.skipped > 0 {
                tracing::warn!(
                    skipped = recovery.skipped,
                    "preview task recovery skipped entries"
                );
            }
            for task in recovery.tasks {
                spawn_preview_task(task.task_id, deps.clone());
            }
        }
        Err(error) => {
            tracing::error!(error = %error, "could not recover durable preview tasks");
        }
    }
}

pub async fn dispatch_task_method(
    message: &super::protocol::OrderedJson,
    publisher: &PublisherIdentity,
    deps: &AppDeps,
) -> Result<Value, McpError> {
    let method = message
        .get("method")
        .and_then(super::protocol::OrderedJson::as_str)
        .unwrap_or_default();
    let params = message.get("params");
    let task_id = params
        .and_then(|params| params.get("taskId"))
        .and_then(super::protocol::OrderedJson::as_str)
        .ok_or_else(|| JsonRpcError::InvalidParams("taskId is required".to_owned()))?;
    let task = authorized_task(task_id, publisher, deps).await?;
    match method {
        "tasks/get" => Ok(task.wire(false)),
        "tasks/update" => {
            if params
                .and_then(|params| params.get("inputResponses"))
                .and_then(super::protocol::OrderedJson::as_object)
                .is_none()
            {
                return Err(JsonRpcError::InvalidParams(
                    "inputResponses must be an object".to_owned(),
                )
                .into());
            }
            Ok(json!({ "resultType": "complete" }))
        }
        "tasks/cancel" => {
            let _ = deps.preview_tasks.cancel(task_id).await?;
            Ok(json!({ "resultType": "complete" }))
        }
        _ => Err(JsonRpcError::MethodNotFound(method.to_owned()).into()),
    }
}

async fn authorized_task(
    task_id: &str,
    publisher: &PublisherIdentity,
    deps: &AppDeps,
) -> Result<PreviewTask, McpError> {
    let task = deps
        .preview_tasks
        .get(task_id)
        .await?
        .filter(|task| task.accessible_to(publisher))
        .ok_or_else(|| JsonRpcError::InvalidParams(format!("Unknown task: {task_id}")))?;
    Ok(task)
}

fn spawn_preview_task(task_id: String, deps: AppDeps) {
    tokio::spawn(async move {
        let Some(task) = deps
            .preview_tasks
            .mark_running(&task_id)
            .await
            .ok()
            .flatten()
        else {
            return;
        };
        if task.is_terminal() {
            return;
        }
        match regenerate(&task.artifact_id.0, &task.publisher(), &deps).await {
            Ok(result) => {
                let _ = deps.preview_tasks.complete(&task_id, result).await;
            }
            Err(error) => {
                tracing::warn!(
                    task_id,
                    artifact_id = %task.artifact_id,
                    "durable preview regeneration failed"
                );
                let _ = deps
                    .preview_tasks
                    .fail(&task_id, &preview_failure_message(&error))
                    .await;
            }
        }
    });
}

async fn regenerate(
    artifact_id: &str,
    publisher: &PublisherIdentity,
    deps: &AppDeps,
) -> Result<Value, AppError> {
    let (meta, html) = preview_source(artifact_id, publisher, deps).await?;
    if !deps.previews.enabled() {
        return Err(AppError::Unavailable(
            "Preview renderer is disabled".to_owned(),
        ));
    }
    deps.previews.remove_artifact(&meta.id).await?;
    let rendered = deps
        .previews
        .ensure_thumbnail(&meta, &html, PreviewPriority::High)
        .await?;
    if rendered.is_none() {
        return Err(AppError::Unavailable(
            "Preview renderer did not produce a valid PNG".to_owned(),
        ));
    }
    Ok(tool_result(artifact_id, true, &meta.body_sha256, None))
}

async fn preview_source(
    artifact_id: &str,
    publisher: &PublisherIdentity,
    deps: &AppDeps,
) -> Result<(crate::model::ArtifactMeta, String), AppError> {
    let meta = deps
        .artifacts
        .find_meta(&ArtifactId::from(artifact_id))
        .await?;
    let owned = AccessPolicy::authorize_publisher_delete(publisher, meta, artifact_id)?;
    if owned.meta().is_bundle {
        return Err(AppError::Validation(
            "Preview regeneration supports single-file artifacts only".to_owned(),
        ));
    }
    let meta = owned.meta().clone();
    let body = deps
        .artifacts
        .read_body(&owned.into_authorized())
        .await?
        .ok_or_else(|| AppError::Gone("Artifact body is unavailable".to_owned()))?;
    let html = String::from_utf8(body.content)
        .map_err(|_| AppError::Validation("Artifact body is not UTF-8 HTML".to_owned()))?;
    Ok((meta, html))
}

fn tool_result(id: &str, regenerated: bool, digest: &str, reason: Option<&str>) -> Value {
    let mut structured = json!({
        "id": id,
        "regenerated": regenerated,
        "digest": digest
    });
    if let Some(reason) = reason {
        structured["reason"] = Value::String(reason.to_owned());
    }
    json!({
        "content": [{
            "type": "text",
            "text": structured.to_string()
        }],
        "structuredContent": structured
    })
}

fn preview_failure_message(error: &AppError) -> String {
    match error {
        AppError::Unavailable(message)
        | AppError::Validation(message)
        | AppError::Gone(message) => message.clone(),
        AppError::NotFound(_) | AppError::ConcealedNotFound => {
            "Artifact is no longer available".to_owned()
        }
        _ => "Preview regeneration failed".to_owned(),
    }
}

fn valid_task_id(value: &str) -> bool {
    value.strip_prefix("task_").is_some_and(|suffix| {
        suffix.len() == 20
            && suffix
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    })
}

fn timestamp() -> String {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_owned())
}

fn task_io_error(operation: &'static str, error: impl std::fmt::Display) -> AppError {
    tracing::error!(operation, error = %error, "durable preview task persistence failed");
    AppError::Internal
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn task_state_round_trips_and_terminal_transitions_are_monotonic() {
        let root = std::env::temp_dir().join(format!("artifact-mcp-tasks-{}", nanoid!(10)));
        let store = PreviewTaskStore::new(&root);
        let publisher = PublisherIdentity {
            client_id: ClientId::from("owner"),
            org: OrgId::from("acme"),
            label: "Owner".to_owned(),
            role: crate::model::Role::Author,
            scopes: None,
        };
        let created = store
            .create(ArtifactId::from("abc123"), &publisher)
            .await
            .expect("create");
        assert_eq!(store.working().await.expect("working").tasks.len(), 1);
        store.cancel(&created.task_id).await.expect("cancel");
        store
            .complete(&created.task_id, json!({"unexpected": true}))
            .await
            .expect("late completion");
        let reloaded = PreviewTaskStore::new(&root)
            .get(&created.task_id)
            .await
            .expect("read")
            .expect("task");
        assert!(matches!(reloaded.status, TaskStatus::Cancelled));
        assert!(reloaded.result.is_none());
        let _ = std::fs::remove_dir_all(root);
    }
    #[tokio::test]
    async fn recovery_reports_errors_and_keeps_readable_tasks() {
        let root = std::env::temp_dir().join(format!("artifact-mcp-recovery-{}", nanoid!(10)));
        let store = PreviewTaskStore::new(&root);
        let missing = store.working().await.expect("missing directory");
        assert!(missing.tasks.is_empty());
        assert_eq!(missing.skipped, 0);

        std::fs::create_dir_all(&root).expect("create root");
        std::fs::write(root.join("tasks"), b"not a directory").expect("create file");
        assert!(store.working().await.is_err());
        std::fs::remove_file(root.join("tasks")).expect("remove file");

        let publisher = PublisherIdentity {
            client_id: ClientId::from("owner"),
            org: OrgId::from("acme"),
            label: "Owner".to_owned(),
            role: crate::model::Role::Author,
            scopes: None,
        };
        let task = store
            .create(ArtifactId::from("abc123"), &publisher)
            .await
            .expect("create");
        std::fs::write(
            root.join("tasks/task_abcdefghijklmnopqrst.json"),
            b"invalid json",
        )
        .expect("corrupt file");
        let recovery = store.working().await.expect("recover");
        assert_eq!(recovery.skipped, 1);
        assert_eq!(recovery.tasks.len(), 1);
        assert_eq!(recovery.tasks[0].task_id, task.task_id);
        let _ = std::fs::remove_dir_all(root);
    }
}
