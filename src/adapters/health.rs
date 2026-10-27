//! Readiness probe for the SQLite pool and the artifact directory.

use std::{
    fs::OpenOptions,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};

use crate::{
    error::AppError,
    persistence::db::{self, DbPool},
    ports::{BoxFuture, HealthProbe, integrations::HealthReport},
};

static HEALTH_PROBE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone)]
pub struct ProductionHealth {
    pool: DbPool,
    artifact_dir: PathBuf,
}

impl ProductionHealth {
    pub const fn new(pool: DbPool, artifact_dir: PathBuf) -> Self {
        Self { pool, artifact_dir }
    }
}

impl HealthProbe for ProductionHealth {
    fn check(&self) -> BoxFuture<'_, Result<HealthReport, AppError>> {
        let pool = self.pool.clone();
        let artifact_dir = self.artifact_dir.clone();
        Box::pin(async move {
            db::interact(&pool, |conn| {
                conn.query_row("SELECT 1", [], |_| Ok(())).map_err(|error| {
                    tracing::warn!(error = %error, "health SQLite check failed");
                    AppError::Unavailable("database unavailable".to_owned())
                })
            })
            .await?;
            check_artifact_directory(artifact_dir).await?;
            Ok(HealthReport::ok())
        })
    }
}

#[tracing::instrument(skip_all)]
async fn check_artifact_directory(artifact_dir: PathBuf) -> Result<(), AppError> {
    tokio::task::spawn_blocking(move || {
        let mut entries = std::fs::read_dir(&artifact_dir).map_err(|error| {
            tracing::warn!(error = %error, "health artifact-directory read check failed");
            AppError::Unavailable("artifact directory unavailable".to_owned())
        })?;
        let _ = entries.next().transpose().map_err(|error| {
            tracing::warn!(error = %error, "health artifact-directory read check failed");
            AppError::Unavailable("artifact directory unavailable".to_owned())
        })?;

        let sequence = HEALTH_PROBE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let probe = artifact_dir.join(format!(".healthcheck-{}-{sequence}", std::process::id()));
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&probe)
            .map_err(|error| {
                tracing::warn!(error = %error, "health artifact-directory write check failed");
                AppError::Unavailable("artifact directory unavailable".to_owned())
            })?;
        std::fs::remove_file(&probe).map_err(|error| {
            tracing::warn!(error = %error, "health artifact-directory cleanup failed");
            AppError::Unavailable("artifact directory unavailable".to_owned())
        })?;
        Ok(())
    })
    .await
    .map_err(|error| {
        tracing::error!(error = %error, "health filesystem task failed");
        AppError::Internal
    })?
}
