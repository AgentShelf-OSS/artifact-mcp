use rusqlite::{OptionalExtension, TransactionBehavior, params};

use crate::{
    error::AppError,
    integrations::discord_inbound::{
        InboundEvent, InboundEventKind, InboundResult, ThreadDegradedReason,
    },
    model::{ArtifactId, OrgId},
    persistence::db::{self, DbPool},
    security::audit::{AuditEvent, MutationAudit, mutate_in_transaction},
};

use super::{
    apply::apply_in_transaction,
    common::{MAX_FETCH_ATTEMPTS, MAX_PROVIDER_ID_BYTES, MAX_SESSION_ID_BYTES, internal},
    validation::validate_event_envelope,
};

#[derive(Clone)]
pub struct DiscordInboundStore {
    pool: DbPool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InboundPolicyStatus {
    pub enabled: bool,
    pub health: String,
    pub safe_error: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GatewayOrganizationTarget {
    pub org: OrgId,
    pub guild_id: String,
    pub channel_id: String,
    pub credential_version: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GatewayResume {
    pub session_id: String,
    pub resume_url: String,
    pub sequence: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DiscordInboundMetricsSnapshot {
    pub gateway_disabled: i64,
    pub gateway_connecting: i64,
    pub gateway_ready: i64,
    pub gateway_reconnecting: i64,
    pub gateway_degraded: i64,
    pub gateway_failed: i64,
    pub inbox_depth: i64,
    pub pending_fetches: i64,
    pub ignored_events: i64,
    pub rejected_or_degraded_events: i64,
    pub tombstones: i64,
    pub last_event_age_seconds: i64,
    pub oldest_pending_age_seconds: i64,
}

impl DiscordInboundStore {
    #[must_use]
    pub const fn new(pool: DbPool) -> Self {
        Self { pool }
    }

    /// Applies one normalized event. Partial message updates are durably marked `NeedsFetch`
    /// without retaining provider content; the runtime hydrates them outside SQLite and retries.
    pub async fn apply_event(&self, event: InboundEvent) -> Result<InboundResult, AppError> {
        validate_event_envelope(&event)?;
        db::interact(&self.pool, move |conn| apply_in_transaction(conn, &event)).await
    }

    /// Returns the exact enabled mapping without exposing credentials or accepting browser IDs.
    pub async fn authorization_for(
        &self,
        org: &OrgId,
        guild_id: &str,
        thread_id: &str,
    ) -> Result<Option<TwoWayThreadAuthorization>, AppError> {
        let org = org.clone();
        let guild_id = guild_id.to_owned();
        let thread_id = thread_id.to_owned();
        db::interact(&self.pool, move |conn| {
            exact_authorization(conn, &org, &guild_id, &thread_id)
        })
        .await
    }

    pub async fn policy_status(
        &self,
        artifact: &ArtifactId,
        org: &OrgId,
    ) -> Result<InboundPolicyStatus, AppError> {
        let artifact = artifact.clone();
        let org = org.clone();
        db::interact(&self.pool, move |conn| {
            if !artifact_exists(conn, &artifact, &org)? {
                return Err(AppError::NotFound("Artifact not found.".to_owned()));
            }
            conn.query_row(
                "SELECT enabled, health, safe_error FROM artifact_discord_inbound_policies \
                  WHERE artifact_id=?1 AND org=?2",
                params![artifact.0, org.0],
                |row| {
                    Ok(InboundPolicyStatus {
                        enabled: row.get::<_, i64>(0)? != 0,
                        health: row.get(1)?,
                        safe_error: row.get(2)?,
                    })
                },
            )
            .optional()
            .map(|row| {
                row.unwrap_or(InboundPolicyStatus {
                    enabled: false,
                    health: "disabled".to_owned(),
                    safe_error: String::new(),
                })
            })
            .map_err(internal)
        })
        .await
    }

    /// Organizations eligible for a Gateway connection. No credential material is selected.
    pub async fn gateway_targets(&self) -> Result<Vec<GatewayOrganizationTarget>, AppError> {
        db::interact(&self.pool, move |conn| {
            let mut statement = conn
                .prepare(
                    "SELECT p.org, c.guild_id, c.channel_id, COALESCE(k.version, 0) \
                       FROM org_discord_threading_policies p \
                       JOIN org_discord_discussion_connections c ON c.org=p.org \
                       LEFT JOIN org_discord_bot_credentials k ON k.org=p.org AND k.active=1 \
                      WHERE p.outbound_enabled=1 AND c.strategy='notification_thread' \
                        AND c.guild_id IS NOT NULL AND c.guild_id <> '' \
                        AND c.channel_id IS NOT NULL AND c.channel_id <> '' \
                        AND EXISTS ( \
                          SELECT 1 FROM artifact_discord_inbound_policies i \
                           WHERE i.org=p.org AND (i.enabled=1 OR i.health='connecting') \
                        ) \
                      ORDER BY p.org",
                )
                .map_err(internal)?;
            let rows = statement
                .query_map([], |row| {
                    Ok(GatewayOrganizationTarget {
                        org: OrgId(row.get(0)?),
                        guild_id: row.get(1)?,
                        channel_id: row.get(2)?,
                        credential_version: row.get(3)?,
                    })
                })
                .map_err(internal)?;
            rows.collect::<rusqlite::Result<Vec<_>>>().map_err(internal)
        })
        .await
    }

    pub async fn set_gateway_health(
        &self,
        org: OrgId,
        credential_version: i64,
        health: &'static str,
        safe_error: &'static str,
        session: Option<(String, String, u64)>,
    ) -> Result<(), AppError> {
        if !matches!(
            health,
            "disabled" | "connecting" | "ready" | "reconnecting" | "degraded" | "failed"
        ) || !matches!(
            safe_error,
            "" | "missing_credential"
                | "message_content_intent"
                | "guild_access"
                | "thread_permission"
                | "gateway_unavailable"
                | "thread_unavailable"
        ) {
            return Err(AppError::Internal);
        }
        db::interact(&self.pool, move |conn| {
            let (session_id, resume_url, sequence) = session.map_or(
                (None, None, None),
                |(session_id, resume_url, sequence)| {
                    (
                        Some(session_id),
                        Some(resume_url),
                        i64::try_from(sequence).ok(),
                    )
                },
            );
            conn.execute(
                "INSERT INTO discord_gateway_sessions \
                 (org, credential_version, session_id, resume_gateway_url, last_sequence, health, safe_error, updated_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, datetime('now')) \
                 ON CONFLICT(org) DO UPDATE SET credential_version=excluded.credential_version, \
                   session_id=excluded.session_id, \
                   resume_gateway_url=excluded.resume_gateway_url, \
                   last_sequence=excluded.last_sequence, health=excluded.health, \
                   safe_error=excluded.safe_error, updated_at=datetime('now')",
                params![
                    org.0,
                    credential_version,
                    session_id,
                    resume_url,
                    sequence,
                    health,
                    safe_error
                ],
            )
            .map_err(internal)?;
            conn.execute(
                "UPDATE artifact_discord_inbound_policies \
                    SET health=CASE \
                          WHEN safe_error='thread_unavailable' THEN health \
                          WHEN enabled=1 OR health <> 'disabled' THEN ?2 \
                          ELSE 'disabled' \
                        END, \
                        safe_error=CASE \
                          WHEN safe_error='thread_unavailable' THEN safe_error \
                          WHEN enabled=1 OR health <> 'disabled' THEN ?3 \
                          ELSE '' \
                        END, \
                        updated_at=datetime('now') \
                  WHERE org=?1",
                params![org.0, health, safe_error],
            )
            .map_err(internal)?;
            Ok(())
        })
        .await
    }

    /// Stages a bounded, per-artifact Gateway readiness request. Outbound-only organizations
    /// never become Gateway targets; the supervisor sees this transient `connecting` marker.
    pub async fn request_gateway_readiness(
        &self,
        artifact: &ArtifactId,
        org: &OrgId,
    ) -> Result<bool, AppError> {
        let artifact = artifact.clone();
        let org = org.clone();
        db::interact(&self.pool, move |conn| {
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(internal)?;
            if !artifact_exists(&tx, &artifact, &org)? {
                return Err(AppError::NotFound("Artifact not found.".to_owned()));
            }
            let credential_version = tx
                .query_row(
                    "SELECT version FROM org_discord_bot_credentials \
                      WHERE org=?1 AND active=1",
                    [&org.0],
                    |row| row.get::<_, i64>(0),
                )
                .optional()
                .map_err(internal)?
                .ok_or_else(|| {
                    AppError::Conflict(
                        "No active organization Discord credential is configured.".to_owned(),
                    )
                })?;
            let ready = tx
                .query_row(
                    "SELECT 1 FROM discord_gateway_sessions \
                      WHERE org=?1 AND credential_version=?2 AND health='ready'",
                    params![org.0, credential_version],
                    |_| Ok(()),
                )
                .optional()
                .map_err(internal)?
                .is_some();
            tx.execute(
                "INSERT INTO discord_gateway_sessions \
                 (org, credential_version, health, safe_error, updated_at) \
                 VALUES (?1, ?2, ?3, '', datetime('now')) \
                 ON CONFLICT(org) DO UPDATE SET \
                   credential_version=excluded.credential_version, \
                   session_id=CASE \
                     WHEN discord_gateway_sessions.credential_version=excluded.credential_version \
                     THEN discord_gateway_sessions.session_id ELSE NULL END, \
                   resume_gateway_url=CASE \
                     WHEN discord_gateway_sessions.credential_version=excluded.credential_version \
                     THEN discord_gateway_sessions.resume_gateway_url ELSE NULL END, \
                   last_sequence=CASE \
                     WHEN discord_gateway_sessions.credential_version=excluded.credential_version \
                     THEN discord_gateway_sessions.last_sequence ELSE NULL END, \
                   health=CASE \
                     WHEN discord_gateway_sessions.credential_version=excluded.credential_version \
                       AND discord_gateway_sessions.health='ready' THEN 'ready' \
                     ELSE 'connecting' END, \
                   safe_error='', updated_at=datetime('now')",
                params![
                    org.0,
                    credential_version,
                    if ready { "ready" } else { "connecting" }
                ],
            )
            .map_err(internal)?;
            tx.execute(
                "INSERT INTO artifact_discord_inbound_policies \
                 (artifact_id, org, enabled, health, safe_error, updated_at) \
                 VALUES (?1, ?2, 0, ?3, '', datetime('now')) \
                 ON CONFLICT(artifact_id, org) DO UPDATE SET \
                   health=CASE WHEN enabled=1 THEN health ELSE excluded.health END, \
                   safe_error=CASE WHEN enabled=1 THEN safe_error ELSE '' END, \
                   updated_at=datetime('now')",
                params![
                    artifact.0,
                    org.0,
                    if ready { "ready" } else { "connecting" }
                ],
            )
            .map_err(internal)?;
            tx.commit().map_err(internal)?;
            Ok(ready)
        })
        .await
    }

    pub async fn gateway_ready(&self, org: &OrgId) -> Result<bool, AppError> {
        let org = org.clone();
        db::interact(&self.pool, move |conn| {
            conn.query_row(
                "SELECT 1 FROM discord_gateway_sessions g \
                  JOIN org_discord_bot_credentials k \
                    ON k.org=g.org AND k.active=1 AND k.version=g.credential_version \
                 WHERE g.org=?1 AND g.health='ready'",
                [&org.0],
                |_| Ok(()),
            )
            .optional()
            .map(|row| row.is_some())
            .map_err(internal)
        })
        .await
    }

    pub async fn gateway_resume(
        &self,
        org: &OrgId,
        credential_version: i64,
    ) -> Result<Option<GatewayResume>, AppError> {
        let org = org.clone();
        db::interact(&self.pool, move |conn| {
            conn.query_row(
                "SELECT session_id, resume_gateway_url, last_sequence \
                   FROM discord_gateway_sessions \
                  WHERE org=?1 AND credential_version=?2 \
                    AND session_id IS NOT NULL AND resume_gateway_url IS NOT NULL \
                    AND last_sequence IS NOT NULL",
                params![org.0, credential_version],
                |row| {
                    let sequence = row.get::<_, i64>(2)?;
                    Ok(GatewayResume {
                        session_id: row.get(0)?,
                        resume_url: row.get(1)?,
                        sequence: u64::try_from(sequence).map_err(|error| {
                            rusqlite::Error::ToSqlConversionFailure(Box::new(error))
                        })?,
                    })
                },
            )
            .optional()
            .map_err(internal)
        })
        .await
    }

    /// Returns a bounded set of body-free updates that still require REST hydration.
    pub async fn pending_updates(
        &self,
        org: &OrgId,
        limit: u16,
    ) -> Result<Vec<InboundEvent>, AppError> {
        let org = org.clone();
        db::interact(&self.pool, move |conn| {
            let mut statement = conn
                .prepare(
                    "SELECT event_id, gateway_session_id, guild_id, thread_id, message_id, \
                            COALESCE(provider_version, 0), payload_sha256 \
                       FROM discord_inbound_events \
                      WHERE provider='discord' AND org=?1 AND event_type='message_update' \
                        AND result='needs_fetch' \
                        AND (next_attempt_at IS NULL OR next_attempt_at <= datetime('now')) \
                      ORDER BY received_at, event_id LIMIT ?2",
                )
                .map_err(internal)?;
            let rows = statement
                .query_map(params![org.0, i64::from(limit)], |row| {
                    let guild_id: String = row.get(2)?;
                    let thread_id: String = row.get(3)?;
                    Ok(InboundEvent {
                        event_id: row.get(0)?,
                        gateway_session_id: row.get(1)?,
                        org: org.clone(),
                        kind: InboundEventKind::MessageUpdate,
                        message: Some(DiscordMessage {
                            id: row.get(4)?,
                            guild_id: guild_id.clone(),
                            thread_id: thread_id.clone(),
                            author: crate::integrations::discord_inbound::DiscordAuthor {
                                id: "deferred-author".to_owned(),
                                display: "Deferred Discord author".to_owned(),
                                is_bot: false,
                                webhook_id: None,
                            },
                            content: None,
                            reply_to_message_id: None,
                            version: row.get(5)?,
                            created_at: None,
                            edited_at: None,
                            supported_text: true,
                        }),
                        guild_id,
                        thread_id,
                        payload_fingerprint: row.get(6)?,
                    })
                })
                .map_err(internal)?;
            rows.collect::<rusqlite::Result<Vec<_>>>().map_err(internal)
        })
        .await
    }

    /// Completes a deferred update that Discord now reports as absent.
    pub async fn complete_missing_update(&self, event: &InboundEvent) -> Result<(), AppError> {
        validate_event_envelope(event)?;
        let event = event.clone();
        db::interact(&self.pool, move |conn| {
            conn.execute(
                "UPDATE discord_inbound_events \
                    SET result='ignored', safe_error='unknown_message', \
                        processed_at=datetime('now'), next_attempt_at=NULL \
                  WHERE provider='discord' AND org=?1 AND gateway_session_id=?2 \
                    AND event_id=?3 AND result='needs_fetch'",
                params![event.org.0, event.gateway_session_id, event.event_id],
            )
            .map(|_| ())
            .map_err(internal)
        })
        .await
    }

    /// Persists bounded provider/network backoff for a deferred update. Returns `true` when the
    /// attempt budget is exhausted and the event reaches a terminal safe failure.
    pub async fn defer_update_retry(
        &self,
        event: &InboundEvent,
        retry_after_seconds: u16,
        rate_limited: bool,
    ) -> Result<bool, AppError> {
        validate_event_envelope(event)?;
        let event = event.clone();
        let retry_after_seconds = retry_after_seconds.clamp(1, 60);
        db::interact(&self.pool, move |conn| {
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(internal)?;
            tx.execute(
                "UPDATE discord_inbound_events \
                    SET attempts=MIN(attempts + 1, ?4), \
                        result=CASE WHEN attempts + 1 >= ?4 THEN 'failed' \
                                    ELSE 'needs_fetch' END, \
                        safe_error=?5, \
                        processed_at=CASE WHEN attempts + 1 >= ?4 \
                                          THEN datetime('now') ELSE NULL END, \
                        next_attempt_at=CASE WHEN attempts + 1 >= ?4 THEN NULL \
                          ELSE datetime('now', ?6) END \
                  WHERE provider='discord' AND org=?1 AND gateway_session_id=?2 \
                    AND event_id=?3 AND result='needs_fetch'",
                params![
                    event.org.0,
                    event.gateway_session_id,
                    event.event_id,
                    MAX_FETCH_ATTEMPTS,
                    if rate_limited {
                        "rate_limited"
                    } else {
                        "gateway_unavailable"
                    },
                    format!("+{retry_after_seconds} seconds")
                ],
            )
            .map_err(internal)?;
            let terminal = tx
                .query_row(
                    "SELECT result='failed' FROM discord_inbound_events \
                      WHERE provider='discord' AND org=?1 AND gateway_session_id=?2 \
                        AND event_id=?3",
                    params![event.org.0, event.gateway_session_id, event.event_id],
                    |row| row.get::<_, bool>(0),
                )
                .optional()
                .map_err(internal)?
                .unwrap_or(false);
            if terminal {
                tx.execute(
                    "UPDATE artifact_discord_inbound_policies \
                        SET health='degraded', safe_error='gateway_unavailable', \
                            updated_at=datetime('now') \
                      WHERE org=?1 AND artifact_id IN ( \
                        SELECT artifact_id FROM artifact_discussions \
                         WHERE org=?1 AND thread_id=?2 \
                      )",
                    params![event.org.0, event.thread_id],
                )
                .map_err(internal)?;
            }
            tx.commit().map_err(internal)?;
            Ok(terminal)
        })
        .await
    }

    /// Deletes a bounded batch of terminal, body-free inbox receipts after the retention window.
    pub async fn cleanup_processed_events(
        &self,
        retention_days: u16,
        limit: u16,
    ) -> Result<usize, AppError> {
        db::interact(&self.pool, move |conn| {
            conn.execute(
                "DELETE FROM discord_inbound_events WHERE rowid IN ( \
                   SELECT rowid FROM discord_inbound_events \
                    WHERE processed_at IS NOT NULL \
                      AND processed_at < datetime('now', ?1) \
                    ORDER BY processed_at, received_at LIMIT ?2 \
                 )",
                params![
                    format!("-{} days", retention_days.max(1)),
                    i64::from(limit.max(1))
                ],
            )
            .map_err(internal)
        })
        .await
    }

    /// Aggregate, low-cardinality operational state. No organization or provider identifiers
    /// leave this projection.
    pub async fn operational_metrics(&self) -> Result<DiscordInboundMetricsSnapshot, AppError> {
        db::interact(&self.pool, move |conn| {
            let gateway = conn
                .query_row(
                    "SELECT \
                       COALESCE(SUM(health='disabled'),0), \
                       COALESCE(SUM(health='connecting'),0), \
                       COALESCE(SUM(health='ready'),0), \
                       COALESCE(SUM(health='reconnecting'),0), \
                       COALESCE(SUM(health='degraded'),0), \
                       COALESCE(SUM(health='failed'),0) \
                     FROM discord_gateway_sessions",
                    [],
                    |row| {
                        Ok((
                            row.get::<_, i64>(0)?,
                            row.get::<_, i64>(1)?,
                            row.get::<_, i64>(2)?,
                            row.get::<_, i64>(3)?,
                            row.get::<_, i64>(4)?,
                            row.get::<_, i64>(5)?,
                        ))
                    },
                )
                .map_err(internal)?;
            let inbox = conn
                .query_row(
                    "SELECT \
                       COUNT(*), \
                       COALESCE(SUM(result='needs_fetch'),0), \
                       COALESCE(SUM(result='ignored'),0), \
                       COALESCE(SUM(result IN ('rejected','degraded','failed')),0), \
                       COALESCE(MAX(0, unixepoch('now') - unixepoch(MAX(received_at))),0), \
                       COALESCE(MAX(0, unixepoch('now') - \
                         unixepoch(MIN(CASE WHEN result='needs_fetch' THEN received_at END))),0) \
                     FROM discord_inbound_events",
                    [],
                    |row| {
                        Ok((
                            row.get::<_, i64>(0)?,
                            row.get::<_, i64>(1)?,
                            row.get::<_, i64>(2)?,
                            row.get::<_, i64>(3)?,
                            row.get::<_, i64>(4)?,
                            row.get::<_, i64>(5)?,
                        ))
                    },
                )
                .map_err(internal)?;
            let tombstones = conn
                .query_row(
                    "SELECT COUNT(*) FROM discord_inbound_message_state \
                      WHERE external_deleted_at IS NOT NULL",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .map_err(internal)?;
            Ok(DiscordInboundMetricsSnapshot {
                gateway_disabled: gateway.0,
                gateway_connecting: gateway.1,
                gateway_ready: gateway.2,
                gateway_reconnecting: gateway.3,
                gateway_degraded: gateway.4,
                gateway_failed: gateway.5,
                inbox_depth: inbox.0,
                pending_fetches: inbox.1,
                ignored_events: inbox.2,
                rejected_or_degraded_events: inbox.3,
                tombstones,
                last_event_age_seconds: inbox.4,
                oldest_pending_age_seconds: inbox.5,
            })
        })
        .await
    }

    pub async fn disable_gateway_integration(&self) -> Result<(), AppError> {
        db::interact(&self.pool, move |conn| {
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(internal)?;
            tx.execute(
                "UPDATE discord_gateway_sessions SET health='disabled', \
                 safe_error='', updated_at=datetime('now')",
                [],
            )
            .map_err(internal)?;
            tx.execute(
                "UPDATE artifact_discord_inbound_policies \
                    SET health=CASE WHEN enabled=1 THEN 'failed' ELSE 'disabled' END, \
                        safe_error=CASE WHEN enabled=1 THEN 'gateway_unavailable' ELSE '' END, \
                        updated_at=datetime('now')",
                [],
            )
            .map_err(internal)?;
            tx.commit().map_err(internal)?;
            Ok(())
        })
        .await
    }

    /// Changes only the explicit inbound policy. Enabling additionally proves the exact
    /// organization-owned notification thread is already connected. Credential readiness is
    /// checked through PBI-081 by the application service immediately before this call.
    pub async fn set_policy_audited(
        &self,
        artifact: ArtifactId,
        org: OrgId,
        enabled: bool,
        audit: MutationAudit,
        audit_key: [u8; 32],
    ) -> Result<(), AppError> {
        db::interact(&self.pool, move |conn| {
            let audit = audit.for_target_tenant(&org.0)?;
            mutate_in_transaction(conn, &audit_key, &audit, |tx| {
                if !artifact_exists(tx, &artifact, &org)? {
                    return Err(AppError::NotFound("Artifact not found.".to_owned()));
                }
                if enabled && !connected_inbound_destination(tx, &artifact, &org)? {
                    return Err(AppError::Conflict(
                        "A connected organization Discord thread is required before enabling two-way sync."
                            .to_owned(),
                    ));
                }
                tx.execute(
                    "INSERT INTO artifact_discord_inbound_policies \
                     (artifact_id, org, enabled, health, safe_error, updated_at) \
                     VALUES (?1, ?2, ?3, ?4, '', datetime('now')) \
                     ON CONFLICT(artifact_id, org) DO UPDATE SET \
                       enabled=excluded.enabled, health=excluded.health, safe_error='', \
                       updated_at=datetime('now')",
                    params![
                        artifact.0,
                        org.0,
                        i64::from(enabled),
                        if enabled { "connecting" } else { "disabled" }
                    ],
                )
                .map_err(internal)?;
                Ok((
                    (),
                    AuditEvent {
                        operation: "discord.artifact-discussion.inbound-policy".to_owned(),
                        target_type: "artifact".to_owned(),
                        target_id: artifact.0.clone(),
                        result: "success".to_owned(),
                        classification: if enabled {
                            "discord_two_way_enabled"
                        } else {
                            "discord_two_way_disabled"
                        }
                        .to_owned(),
                        revision: None,
                    },
                ))
            })
        })
        .await
    }
}
