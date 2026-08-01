//! Bounded historical-recovery worker. It is optional integration health, never app liveness.

use std::{sync::Arc, time::Duration};

use tokio::{sync::watch, task::JoinHandle};

use crate::{
    integrations::discord_history_recovery::{
        DiscordHistoryProvider, ExactRecoveryOutcome, HistoryArtifact, HistoryDestination,
        recover_exact,
    },
    persistence::discord_organization::{OrganizationDiscordStore, RecoveryJob, RecoveryState},
    ports::discussions::OrganizationDiscordCredentialService,
    security::audit::MutationAudit,
};

pub struct DiscordRecoveryRuntime {
    shutdown: watch::Sender<bool>,
    task: JoinHandle<()>,
}

impl DiscordRecoveryRuntime {
    pub fn start(
        store: OrganizationDiscordStore,
        provider: Arc<dyn DiscordHistoryProvider>,
        audit_key: [u8; 32],
    ) -> Self {
        let (shutdown, mut receiver) = watch::channel(false);
        let task = tokio::spawn(async move {
            loop {
                if *receiver.borrow() {
                    break;
                }
                match store.claim_recovery().await {
                    Ok(Some(job)) => process(&store, provider.as_ref(), job, audit_key).await,
                    Ok(None) => {
                        tokio::select! {
                            _ = tokio::time::sleep(Duration::from_secs(2)) => {}
                            changed = receiver.changed() => {
                                if changed.is_err() || *receiver.borrow() {
                                    break;
                                }
                            }
                        }
                    }
                    Err(_) => {
                        tracing::warn!("Discord recovery persistence temporarily unavailable");
                        tokio::time::sleep(Duration::from_secs(2)).await;
                    }
                }
            }
        });
        Self { shutdown, task }
    }

    pub async fn shutdown(self) {
        let _ = self.shutdown.send(true);
        let _ = self.task.await;
    }
}

async fn process(
    store: &OrganizationDiscordStore,
    provider: &dyn DiscordHistoryProvider,
    job: RecoveryJob,
    audit_key: [u8; 32],
) {
    let credential = match store.credential_for_provider(&job.org).await {
        Ok(Some(credential)) => credential,
        Ok(None) | Err(_) => {
            complete(store, &job, RecoveryState::Invalid, None, audit_key).await;
            return;
        }
    };
    let outcome = recover_exact(
        provider,
        &credential,
        &HistoryDestination {
            guild_id: job.destination.guild_id.clone(),
            channel_id: job.destination.channel_id.clone(),
            provider_webhook_id: job.destination.provider_webhook_id.clone(),
        },
        &HistoryArtifact {
            canonical_url: job.canonical_artifact_url.clone(),
        },
    )
    .await;
    let (state, message_id) = match outcome {
        ExactRecoveryOutcome::Recovered { message_id } => {
            (RecoveryState::Recovered, Some(message_id))
        }
        ExactRecoveryOutcome::NotFound => (RecoveryState::NotFound, None),
        ExactRecoveryOutcome::PermissionDenied => (RecoveryState::PermissionDenied, None),
        ExactRecoveryOutcome::RateLimited => (RecoveryState::RateLimited, None),
        ExactRecoveryOutcome::Retryable => (RecoveryState::Retryable, None),
    };
    complete(store, &job, state, message_id, audit_key).await;
}

async fn complete(
    store: &OrganizationDiscordStore,
    job: &RecoveryJob,
    state: RecoveryState,
    message_id: Option<String>,
    audit_key: [u8; 32],
) {
    let result = match MutationAudit::recovery() {
        Ok(audit) => {
            store
                .complete_recovery_audited(
                    job.artifact_id.clone(),
                    job.org.clone(),
                    state,
                    message_id,
                    audit,
                    audit_key,
                )
                .await
        }
        Err(error) => Err(error),
    };
    if result.is_err() {
        tracing::warn!("Discord recovery result could not be persisted");
    }
}
