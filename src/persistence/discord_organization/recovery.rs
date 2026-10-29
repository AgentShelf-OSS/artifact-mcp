use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};

use crate::{
    error::AppError,
    integrations::delivery_envelope::stable_delivery_event_id,
    model::{ArtifactId, OrgId, WebhookEvent},
    persistence::db,
    security::audit::{AuditEvent, MutationAudit, mutate_in_transaction},
};

use super::{
    helpers::{
        INVALID_RECOVERY_DESTINATION, artifact_exists, destination_matches,
        destination_matches_preflight, internal, is_canonical_url, recovery_record, trimmed,
        valid_discord_id,
    },
    types::{
        OrganizationDiscordStore, RecoveryDestination, RecoveryJob, RecoveryRecord, RecoveryState,
    },
};

impl OrganizationDiscordStore {
    /// Enqueues only local evidence for a bounded recovery worker.  Provider scans happen later;
    /// no Discord response or body can enter this transaction.
    pub async fn schedule_recovery(
        &self,
        artifact: &ArtifactId,
        org: &OrgId,
        destination: RecoveryDestination,
        canonical_artifact_url: String,
    ) -> Result<(), AppError> {
        if !is_canonical_url(&canonical_artifact_url) {
            return Err(AppError::Validation(
                "A canonical artifact URL is required for Discord recovery.".to_owned(),
            ));
        }
        let artifact = trimmed(&artifact.0);
        let org = trimmed(&org.0);
        db::interact(&self.pool, move |conn| {
            if !artifact_exists(conn, &artifact, &org)? || !destination_matches(conn, &org, &destination)? {
                return Err(AppError::Validation(INVALID_RECOVERY_DESTINATION.to_owned()));
            }
            conn.execute(
                "INSERT INTO discord_notification_anchor_recoveries \
                 (artifact_id, org, connection_id, notification_webhook_id, provider_webhook_id, guild_id, channel_id, canonical_artifact_url, state) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'pending') \
                 ON CONFLICT(artifact_id, org) DO UPDATE SET connection_id=excluded.connection_id, \
                   notification_webhook_id=excluded.notification_webhook_id, provider_webhook_id=excluded.provider_webhook_id, guild_id=excluded.guild_id, \
                   channel_id=excluded.channel_id, canonical_artifact_url=excluded.canonical_artifact_url, \
                   state='pending', recovered_message_id=NULL, provenance='', last_error='', completed_at=NULL, updated_at=datetime('now')",
                params![artifact, org, destination.connection_id, destination.notification_webhook_id, destination.provider_webhook_id, destination.guild_id, destination.channel_id, canonical_artifact_url],
            ).map_err(internal)?;
            Ok(())
        }).await
    }

    /// Queue eligible historical artifacts without touching artifacts that already have an exact
    /// retained publication receipt or recovered provenance.
    pub async fn queue_recoveries_for_org_audited(
        &self,
        org: OrgId,
        destination: RecoveryDestination,
        public_base_url: String,
        audit: MutationAudit,
        audit_key: [u8; 32],
    ) -> Result<u64, AppError> {
        let org_name = trimmed(&org.0);
        let base = public_base_url.trim_end_matches('/').to_owned();
        if !destination_matches_preflight(&destination) {
            return Err(AppError::Validation(
                INVALID_RECOVERY_DESTINATION.to_owned(),
            ));
        }
        db::interact(&self.pool, move |conn| {
            Self::queue_recoveries_with_audit(conn, org_name, destination, base, audit, audit_key)
        })
        .await
    }

    pub async fn claim_recovery(&self) -> Result<Option<RecoveryJob>, AppError> {
        db::interact(&self.pool, move |conn| {
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(internal)?;
            let job = tx
                .query_row(
                    "SELECT artifact_id, org, connection_id, notification_webhook_id, \
                            provider_webhook_id, guild_id, channel_id, canonical_artifact_url \
                     FROM discord_notification_anchor_recoveries \
                     WHERE attempts < 5 AND (state='pending' \
                        OR (state='recovering' AND updated_at <= datetime('now','-60 seconds')) \
                        OR (state='retryable' AND updated_at <= datetime('now','-5 seconds')) \
                        OR (state='rate_limited' AND updated_at <= datetime('now','-30 seconds'))) \
                     ORDER BY updated_at, org, artifact_id LIMIT 1",
                    [],
                    |row| {
                        Ok(RecoveryJob {
                            artifact_id: ArtifactId(row.get(0)?),
                            org: OrgId(row.get(1)?),
                            destination: RecoveryDestination {
                                connection_id: row.get(2)?,
                                notification_webhook_id: row.get(3)?,
                                provider_webhook_id: row.get(4)?,
                                guild_id: row.get(5)?,
                                channel_id: row.get(6)?,
                            },
                            canonical_artifact_url: row.get(7)?,
                        })
                    },
                )
                .optional()
                .map_err(internal)?;
            if let Some(job) = &job {
                let changed = tx
                    .execute(
                        "UPDATE discord_notification_anchor_recoveries \
                         SET state='recovering', attempts=attempts+1, updated_at=datetime('now') \
                         WHERE artifact_id=?1 AND org=?2 \
                           AND state IN ('pending','recovering','retryable','rate_limited')",
                        params![job.artifact_id.0, job.org.0],
                    )
                    .map_err(internal)?;
                if changed != 1 {
                    return Err(AppError::Internal);
                }
            }
            tx.commit().map_err(internal)?;
            Ok(job)
        })
        .await
    }

    /// Records a provider outcome only after the recovery worker has applied exact selected
    /// webhook + canonical URL matching. `message_id` is accepted only for `Recovered`.
    pub async fn complete_recovery(
        &self,
        artifact: &ArtifactId,
        org: &OrgId,
        state: RecoveryState,
        message_id: Option<String>,
    ) -> Result<RecoveryRecord, AppError> {
        let artifact = trimmed(&artifact.0);
        let org = trimmed(&org.0);
        let message_id = message_id.map(|value| trimmed(&value));
        if (state == RecoveryState::Recovered)
            != message_id
                .as_ref()
                .is_some_and(|value| valid_discord_id(value))
        {
            return Err(AppError::Validation(
                "Invalid exact Discord recovery result.".to_owned(),
            ));
        }
        db::interact(&self.pool, move |conn| {
            let changed = conn.execute(
                "UPDATE discord_notification_anchor_recoveries SET state=?3, recovered_message_id=?4, \
                 provenance=CASE WHEN ?3='recovered' THEN 'exact_selected_webhook_canonical_url' ELSE '' END, \
                 last_error=CASE WHEN ?3 IN ('not_found','ambiguous','permission_denied','rate_limited','retryable','invalid') THEN ?3 ELSE '' END, \
                 completed_at=CASE WHEN ?3 IN ('pending','recovering','retryable','rate_limited') THEN NULL ELSE datetime('now') END, updated_at=datetime('now') \
                 WHERE artifact_id=?1 AND org=?2",
                params![artifact, org, state.as_str(), message_id],
            ).map_err(internal)?;
            if changed != 1 { return Err(AppError::NotFound("Discord recovery was not scheduled.".to_owned())); }
            recovery_record(conn, &artifact, &org)?.ok_or(AppError::Internal)
        }).await
    }

    /// Worker-only terminal/retry transition with a secret-free, fixed-classification audit
    /// event in the same immediate transaction. If audit persistence fails, the recovering row
    /// remains reclaimable instead of silently losing outcome evidence.
    pub async fn complete_recovery_audited(
        &self,
        artifact: ArtifactId,
        org: OrgId,
        state: RecoveryState,
        message_id: Option<String>,
        audit: MutationAudit,
        audit_key: [u8; 32],
    ) -> Result<RecoveryRecord, AppError> {
        let artifact_name = trimmed(&artifact.0);
        let org_name = trimmed(&org.0);
        let message_id = message_id.map(|value| trimmed(&value));
        if (state == RecoveryState::Recovered)
            != message_id
                .as_ref()
                .is_some_and(|value| valid_discord_id(value))
        {
            return Err(AppError::Validation(
                "Invalid exact Discord recovery result.".to_owned(),
            ));
        }
        db::interact(&self.pool, move |conn| {
            let audit = audit.for_affected_tenant(&OrgId(org_name.clone()));
            mutate_in_transaction(conn, &audit_key, &audit, |tx| {
                let changed = tx
                    .execute(
                        "UPDATE discord_notification_anchor_recoveries SET state=?3, recovered_message_id=?4, \
                         provenance=CASE WHEN ?3='recovered' THEN 'exact_selected_webhook_canonical_url' ELSE '' END, \
                         last_error=CASE WHEN ?3 IN ('not_found','ambiguous','permission_denied','rate_limited','retryable','invalid') THEN ?3 ELSE '' END, \
                         completed_at=CASE WHEN ?3 IN ('pending','recovering','retryable','rate_limited') THEN NULL ELSE datetime('now') END, \
                         updated_at=datetime('now') WHERE artifact_id=?1 AND org=?2 AND state='recovering'",
                        params![artifact_name, org_name, state.as_str(), message_id],
                    )
                    .map_err(internal)?;
                if changed != 1 {
                    return Err(AppError::Conflict(
                        "Discord recovery lease is no longer current.".to_owned(),
                    ));
                }
                let record =
                    recovery_record(tx, &artifact_name, &org_name)?.ok_or(AppError::Internal)?;
                Ok((
                    record,
                    AuditEvent {
                        operation: "discord.history-recovery.complete".to_owned(),
                        target_type: "artifact".to_owned(),
                        target_id: artifact_name.clone(),
                        result: "success".to_owned(),
                        classification: format!("discord_recovery_{}", state.as_str()),
                        revision: None,
                    },
                ))
            })
        })
        .await
    }

    pub async fn recovery_record(
        &self,
        artifact: &ArtifactId,
        org: &OrgId,
    ) -> Result<Option<RecoveryRecord>, AppError> {
        let artifact = trimmed(&artifact.0);
        let org = trimmed(&org.0);
        db::interact(&self.pool, move |conn| {
            recovery_record(conn, &artifact, &org)
        })
        .await
    }

    /// Returns a recovered anchor only to the root-thread delivery path after binding every
    /// artifact, tenant, and selected destination dimension.  This intentionally does *not*
    /// synthesize a `provider_delivery_outbox` acceptance: a recovered historic notification has
    /// no durable delivery row, and its provenance stays in the recovery table instead.
    pub async fn recovered_anchor_message(
        &self,
        artifact: &ArtifactId,
        org: &OrgId,
        destination: &RecoveryDestination,
    ) -> Result<Option<String>, AppError> {
        let artifact = trimmed(&artifact.0);
        let org = trimmed(&org.0);
        let destination = destination.clone();
        db::interact(&self.pool, move |conn| {
            conn.query_row(
                "SELECT recovered_message_id FROM discord_notification_anchor_recoveries \
                 WHERE artifact_id=?1 AND org=?2 AND connection_id=?3 AND notification_webhook_id=?4 \
                   AND provider_webhook_id=?5 AND guild_id=?6 AND channel_id=?7 AND state='recovered' \
                   AND provenance='exact_selected_webhook_canonical_url'",
                params![artifact, org, destination.connection_id, destination.notification_webhook_id, destination.provider_webhook_id, destination.guild_id, destination.channel_id],
                |row| row.get::<_, String>(0),
            ).optional().map_err(internal)
        }).await
    }

    fn queue_recoveries_with_audit(
        conn: &mut Connection,
        org_name: String,
        destination: RecoveryDestination,
        base: String,
        audit: MutationAudit,
        audit_key: [u8; 32],
    ) -> Result<u64, AppError> {
        let audit = audit.for_target_tenant(&org_name)?;
        mutate_in_transaction(conn, &audit_key, &audit, |tx| {
            if !destination_matches(tx, &org_name, &destination)? {
                return Err(AppError::Validation(
                    INVALID_RECOVERY_DESTINATION.to_owned(),
                ));
            }
            let artifact_ids = Self::eligible_artifacts(tx, &org_name)?;
            let queued = Self::queue_artifacts(tx, &org_name, &destination, &base, artifact_ids)?;
            Ok((
                queued,
                AuditEvent {
                    operation: "discord.history-recovery.queue".to_owned(),
                    target_type: "organization".to_owned(),
                    target_id: org_name.clone(),
                    result: "success".to_owned(),
                    classification: "discord_recovery_queued".to_owned(),
                    revision: None,
                },
            ))
        })
    }

    fn eligible_artifacts(conn: &Connection, org_name: &str) -> Result<Vec<String>, AppError> {
        let mut artifact_ids = Vec::new();
        {
            let mut statement = conn
                .prepare(
                    "SELECT a.id FROM artifacts a \
                 WHERE a.org=?1 AND NOT EXISTS(\
                   SELECT 1 FROM artifact_discussion_overrides x \
                    WHERE x.artifact_id=a.id AND x.org=a.org\
                 ) ORDER BY a.id",
                )
                .map_err(internal)?;
            let rows = statement
                .query_map([org_name], |row| row.get::<_, String>(0))
                .map_err(internal)?;
            for row in rows {
                artifact_ids.push(row.map_err(internal)?);
            }
        }
        Ok(artifact_ids)
    }

    fn queue_artifacts(
        conn: &Connection,
        org_name: &str,
        destination: &RecoveryDestination,
        base: &str,
        artifact_ids: Vec<String>,
    ) -> Result<u64, AppError> {
        let mut queued = 0_u64;
        for artifact_id in artifact_ids {
            queued += Self::queue_artifact(conn, org_name, destination, base, &artifact_id)?;
        }
        Ok(queued)
    }

    fn queue_artifact(
        conn: &Connection,
        org_name: &str,
        destination: &RecoveryDestination,
        base: &str,
        artifact_id: &str,
    ) -> Result<u64, AppError> {
        let subject = format!("artifact:{artifact_id}:1");
        let event_id = stable_delivery_event_id(
            &OrgId(org_name.to_owned()),
            &WebhookEvent::Published,
            &subject,
        );
        let retained: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM provider_delivery_outbox \
             WHERE provider='discord' AND tenant=?1 AND target_key=?2 \
               AND event_id=?3 AND event_type='published' \
               AND delivery_kind='event' AND state='accepted' \
               AND discord_message_id IS NOT NULL)",
                params![org_name, destination.notification_webhook_id, event_id],
                |row| row.get(0),
            )
            .map_err(internal)?;
        if retained {
            return Ok(0);
        }
        let canonical_url = format!("{base}/{artifact_id}");
        if !is_canonical_url(&canonical_url) {
            return Err(AppError::Validation(
                "A canonical artifact URL is required for Discord recovery.".to_owned(),
            ));
        }
        conn.execute(
            "INSERT INTO discord_notification_anchor_recoveries \
         (artifact_id, org, connection_id, notification_webhook_id, \
          provider_webhook_id, guild_id, channel_id, canonical_artifact_url, state) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'pending') \
         ON CONFLICT(artifact_id, org) DO UPDATE SET \
           connection_id=excluded.connection_id, \
           notification_webhook_id=excluded.notification_webhook_id, \
           provider_webhook_id=excluded.provider_webhook_id, \
           guild_id=excluded.guild_id, channel_id=excluded.channel_id, \
           canonical_artifact_url=excluded.canonical_artifact_url, \
           state='pending', recovered_message_id=NULL, provenance='', \
           last_error='', completed_at=NULL, updated_at=datetime('now') \
         WHERE discord_notification_anchor_recoveries.state <> 'recovered'",
            params![
                artifact_id,
                org_name,
                destination.connection_id,
                destination.notification_webhook_id,
                destination.provider_webhook_id,
                destination.guild_id,
                destination.channel_id,
                canonical_url
            ],
        )
        .map_err(internal)
        .map(|count| count as u64)
    }
}
