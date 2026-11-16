use artifact_mcp::{
    artifacts::lifecycle::ArtifactStore,
    config::{AppConfig, NanoIdSource},
    persistence::{db::Database, viewer_state_service::SqliteViewerState},
    security::audit::parse_hmac_key,
};
use std::{env, net::SocketAddr, path::PathBuf, sync::Arc};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt().with_target(false).init();
    let config = AppConfig::from_env()?;
    let token_path = PathBuf::from(env::var("BABY_LOG_VOICE_TOKEN_FILE")?);
    let (artifacts, state, token) = tokio::task::spawn_blocking(move || {
        let pool = Database::connect_existing(&config.data_dir)?;
        let encoded = config.audit_ledger_hmac_key.as_ref().ok_or_else(|| {
            artifact_mcp::error::AppError::Validation("audit key required".into())
        })?;
        let key = parse_hmac_key(encoded.expose())?;
        let store =
            ArtifactStore::from_config(pool.clone(), &config, Arc::new(NanoIdSource::default()))?;
        let token = std::fs::read(token_path).map_err(|_| {
            artifact_mcp::error::AppError::Validation("token file unavailable".into())
        })?;
        Ok::<_, artifact_mcp::error::AppError>((
            Arc::new(store),
            Arc::new(SqliteViewerState::new(pool, Some(key))),
            token,
        ))
    })
    .await??;
    let state = baby_log_voice::AppState::new(artifacts, state, &token)?;
    let addr: SocketAddr = env::var("BABY_LOG_VOICE_BIND")
        .unwrap_or_else(|_| "192.168.0.220:3491".into())
        .parse()?;
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!(%addr, "Baby Log voice listener ready");
    axum::serve(listener, baby_log_voice::router(state))
        .with_graceful_shutdown(async {
            if let Err(error) = tokio::signal::ctrl_c().await {
                tracing::error!(%error, "shutdown signal unavailable");
            }
        })
        .await?;
    Ok(())
}
