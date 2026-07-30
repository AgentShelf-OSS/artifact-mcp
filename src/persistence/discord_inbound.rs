//! Durable PBI-080 Discord inbound event application.
//!
//! Provider frames are normalized before they reach this store.  The store then resolves the
//! tenant/thread binding from server-owned PBI-079/081 state and commits the inbox receipt,
//! canonical feedback mutation, external-message correlation, and health transition in one
//! immediate SQLite transaction.  It never calls Discord and never enqueues outbound delivery.

use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};

use crate::{
    error::AppError,
    integrations::discord_inbound::{
        DiscordMessage, DiscordRestPort, IgnoreReason, InboundEvent, InboundEventInbox,
        InboundEventKind, InboundFeedback, InboundIntegrationState, InboundProcessor,
        InboundResult, MemoryInbox, RejectReason, RestError, ThreadDegradedReason,
        TwoWayThreadAuthorization,
    },
    model::{ArtifactId, FeedbackAuthor, FeedbackId, OrgId},
    persistence::db::{self, DbPool},
    security::audit::{AuditEvent, MutationAudit, mutate_in_transaction},
};

const MAX_PROVIDER_ID_BYTES: usize = 128;
const MAX_SESSION_ID_BYTES: usize = 256;
const MAX_SAFE_ERROR_BYTES: usize = 64;
const MAX_FETCH_ATTEMPTS: i64 = 20;

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

fn artifact_exists(
    conn: &Connection,
    artifact: &ArtifactId,
    org: &OrgId,
) -> Result<bool, AppError> {
    conn.query_row(
        "SELECT 1 FROM artifacts WHERE id=?1 AND org=?2",
        params![artifact.0, org.0],
        |_| Ok(()),
    )
    .optional()
    .map(|row| row.is_some())
    .map_err(internal)
}

fn connected_inbound_destination(
    conn: &Connection,
    artifact: &ArtifactId,
    org: &OrgId,
) -> Result<bool, AppError> {
    conn.query_row(
        "SELECT EXISTS( \
           SELECT 1 FROM artifact_discussions d \
           JOIN org_discord_discussion_connections c \
             ON c.id=d.connection_id AND c.org=d.org \
           JOIN org_discord_threading_policies p ON p.org=d.org \
          WHERE d.artifact_id=?1 AND d.org=?2 AND d.provider='discord' \
            AND d.mode='discord_mirror' AND d.state='connected' \
            AND d.thread_id IS NOT NULL AND d.thread_id <> '' \
            AND c.strategy='notification_thread' AND c.guild_id IS NOT NULL \
            AND c.guild_id <> '' AND c.notification_provider_webhook_id IS NOT NULL \
            AND c.notification_provider_webhook_id <> '' AND p.outbound_enabled=1 \
            AND EXISTS (SELECT 1 FROM discord_gateway_sessions g \
                         WHERE g.org=d.org AND g.health='ready') \
         )",
        params![artifact.0, org.0],
        |row| row.get(0),
    )
    .map_err(internal)
}

fn apply_in_transaction(
    conn: &mut Connection,
    event: &InboundEvent,
) -> Result<InboundResult, AppError> {
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(internal)?;

    if event_exists(&tx, event)? {
        return Ok(InboundResult::Duplicate);
    }

    let binding = authorization_for_event(&tx, &event.org, &event.guild_id, &event.thread_id)?;
    if locally_moderated_message(&tx, event)? {
        let result = InboundResult::Ignored(
            crate::integrations::discord_inbound::IgnoreReason::UnknownMessage,
        );
        record_event(&tx, event, result)?;
        tx.commit().map_err(internal)?;
        return Ok(result);
    }
    let mut inbox = MemoryInbox::default();
    if let Some(binding) = binding.as_ref() {
        load_relevant_feedback(&tx, binding, event, &mut inbox)?;
    }

    let processor = InboundProcessor::new(InboundIntegrationState {
        enabled: true,
        health: crate::integrations::discord_inbound::GatewayHealth::Ready,
    });
    let result = processor.process(binding.as_ref(), event.clone(), &mut inbox, &NoFetchRest);

    if result == InboundResult::Applied {
        let message_id = event
            .message
            .as_ref()
            .map(|message| message.id.as_str())
            .ok_or_else(|| {
                AppError::Validation("Discord event is missing a message.".to_owned())
            })?;
        let feedback = inbox.feedback.get(message_id).ok_or(AppError::Internal)?;
        persist_feedback(&tx, feedback, &event.thread_id)?;
    } else if let InboundResult::Degraded(reason) = result
        && let Some(binding) = binding.as_ref()
    {
        mark_policy_degraded(&tx, binding, reason)?;
    }

    record_event(&tx, event, result)?;
    tx.commit().map_err(internal)?;
    Ok(result)
}

fn exact_authorization(
    conn: &Connection,
    org: &OrgId,
    guild_id: &str,
    thread_id: &str,
) -> Result<Option<TwoWayThreadAuthorization>, AppError> {
    conn.query_row(
        "SELECT p.artifact_id, c.notification_provider_webhook_id \
           FROM artifact_discord_inbound_policies p \
           JOIN artifact_discussions d \
             ON d.artifact_id=p.artifact_id AND d.org=p.org \
           JOIN org_discord_discussion_connections c \
             ON c.id=d.connection_id AND c.org=d.org \
          WHERE p.org=?1 AND p.enabled=1 \
            AND d.provider='discord' AND d.mode='discord_mirror' \
            AND d.state='connected' AND d.thread_id=?3 \
            AND c.strategy='notification_thread' AND c.guild_id=?2 \
          LIMIT 1",
        params![org.0, guild_id, thread_id],
        |row| {
            Ok(TwoWayThreadAuthorization {
                org: org.clone(),
                artifact_id: ArtifactId(row.get(0)?),
                guild_id: guild_id.to_owned(),
                thread_id: thread_id.to_owned(),
                enabled: true,
                // Gateway's author.bot flag is authoritative. The value remains as a second
                // provider-adapter check when a future credential projection stores the bot id.
                configured_bot_user_id: String::new(),
                configured_webhook_id: row.get(1)?,
            })
        },
    )
    .optional()
    .map_err(internal)
}

fn authorization_for_event(
    conn: &Connection,
    org: &OrgId,
    guild_id: &str,
    thread_id: &str,
) -> Result<Option<TwoWayThreadAuthorization>, AppError> {
    if let Some(binding) = exact_authorization(conn, org, guild_id, thread_id)? {
        return Ok(Some(binding));
    }

    // A thread can be a valid mapping for another tenant in the same Discord guild. Resolve only
    // enough server-side state for the pure processor to emit a durable CrossTenant rejection.
    // The foreign artifact/provider identifiers never leave persistence or enter an error.
    conn.query_row(
        "SELECT p.org, p.artifact_id, c.notification_provider_webhook_id \
           FROM artifact_discord_inbound_policies p \
           JOIN artifact_discussions d \
             ON d.artifact_id=p.artifact_id AND d.org=p.org \
           JOIN org_discord_discussion_connections c \
             ON c.id=d.connection_id AND c.org=d.org \
          WHERE p.org<>?1 AND p.enabled=1 \
            AND d.provider='discord' AND d.mode='discord_mirror' \
            AND d.state='connected' AND d.thread_id=?3 \
            AND c.strategy='notification_thread' AND c.guild_id=?2 \
          LIMIT 1",
        params![org.0, guild_id, thread_id],
        |row| {
            Ok(TwoWayThreadAuthorization {
                org: OrgId(row.get(0)?),
                artifact_id: ArtifactId(row.get(1)?),
                guild_id: guild_id.to_owned(),
                thread_id: thread_id.to_owned(),
                enabled: true,
                configured_bot_user_id: String::new(),
                configured_webhook_id: row.get(2)?,
            })
        },
    )
    .optional()
    .map_err(internal)
}

fn locally_moderated_message(conn: &Connection, event: &InboundEvent) -> Result<bool, AppError> {
    let Some(message_id) = event.message.as_ref().map(|message| message.id.as_str()) else {
        return Ok(false);
    };
    conn.query_row(
        "SELECT 1 FROM discord_inbound_message_state \
          WHERE provider='discord' AND org=?1 AND external_message_id=?2 \
            AND feedback_id IS NULL",
        params![event.org.0, message_id],
        |_| Ok(()),
    )
    .optional()
    .map(|row| row.is_some())
    .map_err(internal)
}

fn event_exists(tx: &Transaction<'_>, event: &InboundEvent) -> Result<bool, AppError> {
    tx.query_row(
        "SELECT 1 FROM discord_inbound_events \
          WHERE provider='discord' AND org=?1 AND gateway_session_id=?2 AND event_id=?3 \
            AND result <> 'needs_fetch'",
        params![event.org.0, event.gateway_session_id, event.event_id],
        |_| Ok(()),
    )
    .optional()
    .map(|row| row.is_some())
    .map_err(internal)
}

fn load_relevant_feedback(
    tx: &Transaction<'_>,
    binding: &TwoWayThreadAuthorization,
    event: &InboundEvent,
    inbox: &mut MemoryInbox,
) -> Result<(), AppError> {
    let Some(message) = event.message.as_ref() else {
        return Ok(());
    };
    load_message(tx, binding, &message.id, inbox)?;
    if let Some(parent) = message.reply_to_message_id.as_deref() {
        load_message(tx, binding, parent, inbox)?;
    }
    Ok(())
}

fn load_message(
    tx: &Transaction<'_>,
    binding: &TwoWayThreadAuthorization,
    message_id: &str,
    inbox: &mut MemoryInbox,
) -> Result<(), AppError> {
    let row = tx
        .query_row(
            "SELECT s.feedback_id, f.parent_id, s.external_author_id, \
                    s.external_author_display, f.body, s.provider_version, \
                    s.external_created_at, s.external_edited_at, s.external_deleted_at \
               FROM discord_inbound_message_state s \
               JOIN feedback f \
                 ON f.id=s.feedback_id AND f.artifact_id=s.artifact_id AND f.org=s.org \
              WHERE s.provider='discord' AND s.org=?1 AND s.artifact_id=?2 \
                AND s.external_thread_id=?3 AND s.external_message_id=?4",
            params![
                binding.org.0,
                binding.artifact_id.0,
                binding.thread_id,
                message_id
            ],
            |row| {
                Ok(InboundFeedback {
                    id: FeedbackId(row.get(0)?),
                    artifact_id: binding.artifact_id.clone(),
                    org: binding.org.clone(),
                    parent_id: row.get::<_, Option<String>>(1)?.map(FeedbackId),
                    author: FeedbackAuthor::Discord {
                        external_author_id: row.get(2)?,
                        external_author_display: row.get(3)?,
                    },
                    body: row.get(4)?,
                    external_message_id: message_id.to_owned(),
                    provider_version: row.get(5)?,
                    external_created_at: row.get(6)?,
                    external_edited_at: row.get(7)?,
                    external_deleted_at: row.get(8)?,
                })
            },
        )
        .optional()
        .map_err(internal)?;
    if let Some(row) = row {
        inbox.put_feedback(row);
        return Ok(());
    }

    // A Discord reply may reference an Artifact-origin comment that PBI-079 mirrored outward.
    // Load only its canonical thread identity; this is a read-only parent correlation and can
    // never make that Artifact row look externally authored.
    let mirrored = tx
        .query_row(
            "SELECT f.id, f.parent_id, f.viewer_email, f.body \
               FROM discussion_message_links l \
               JOIN feedback f \
                 ON f.id=l.feedback_id AND f.artifact_id=l.artifact_id AND f.org=l.org \
              WHERE l.provider='discord' AND l.org=?1 AND l.artifact_id=?2 \
                AND l.external_thread_id=?3 AND l.external_message_id=?4 \
                AND l.source='artifact' AND l.state IN ('posted','local_deleted')",
            params![
                binding.org.0,
                binding.artifact_id.0,
                binding.thread_id,
                message_id
            ],
            |row| {
                let viewer_email = row.get::<_, Option<String>>(2)?.ok_or_else(|| {
                    rusqlite::Error::InvalidColumnType(
                        2,
                        "viewer_email".to_owned(),
                        rusqlite::types::Type::Null,
                    )
                })?;
                Ok(InboundFeedback {
                    id: FeedbackId(row.get(0)?),
                    artifact_id: binding.artifact_id.clone(),
                    org: binding.org.clone(),
                    parent_id: row.get::<_, Option<String>>(1)?.map(FeedbackId),
                    author: FeedbackAuthor::Artifact {
                        viewer_email: crate::model::EmailAddress(viewer_email),
                    },
                    body: row.get(3)?,
                    external_message_id: message_id.to_owned(),
                    provider_version: 0,
                    external_created_at: None,
                    external_edited_at: None,
                    external_deleted_at: None,
                })
            },
        )
        .optional()
        .map_err(internal)?;
    if let Some(row) = mirrored {
        inbox.put_feedback(row);
    }
    Ok(())
}

fn persist_feedback(
    tx: &Transaction<'_>,
    feedback: &InboundFeedback,
    thread_id: &str,
) -> Result<(), AppError> {
    let FeedbackAuthor::Discord {
        external_author_id,
        external_author_display,
    } = &feedback.author
    else {
        return Err(AppError::Internal);
    };
    let revision: i64 = tx
        .query_row(
            "SELECT revision FROM artifacts WHERE id=?1 AND org=?2",
            params![feedback.artifact_id.0, feedback.org.0],
            |row| row.get(0),
        )
        .map_err(internal)?;
    let exists = tx
        .query_row(
            "SELECT 1 FROM discord_inbound_message_state \
              WHERE provider='discord' AND org=?1 AND external_message_id=?2",
            params![feedback.org.0, feedback.external_message_id],
            |_| Ok(()),
        )
        .optional()
        .map_err(internal)?
        .is_some();

    if exists {
        tx.execute(
            "UPDATE feedback SET body=?1, external_edited_at=?2, external_deleted_at=?3 \
              WHERE id=?4 AND artifact_id=?5 AND org=?6 AND author_source='discord'",
            params![
                feedback.body,
                feedback.external_edited_at,
                feedback.external_deleted_at,
                feedback.id.0,
                feedback.artifact_id.0,
                feedback.org.0
            ],
        )
        .map_err(internal)?;
        tx.execute(
            "UPDATE discord_inbound_message_state \
                SET provider_version=?1, external_author_display=?2, external_edited_at=?3, \
                    external_deleted_at=?4, updated_at=datetime('now') \
              WHERE provider='discord' AND org=?5 AND external_message_id=?6 \
                AND artifact_id=?7 AND external_thread_id=?8",
            params![
                feedback.provider_version,
                external_author_display,
                feedback.external_edited_at,
                feedback.external_deleted_at,
                feedback.org.0,
                feedback.external_message_id,
                feedback.artifact_id.0,
                thread_id
            ],
        )
        .map_err(internal)?;
    } else {
        tx.execute(
            "INSERT INTO feedback \
             (id, artifact_id, org, viewer_email, body, artifact_revision, parent_id, \
              author_source, external_author_id, external_author_display, external_created_at, \
              external_edited_at, external_deleted_at) \
             VALUES (?1, ?2, ?3, NULL, ?4, ?5, ?6, 'discord', ?7, ?8, ?9, ?10, ?11)",
            params![
                feedback.id.0,
                feedback.artifact_id.0,
                feedback.org.0,
                feedback.body,
                revision,
                feedback.parent_id.as_ref().map(|id| id.0.as_str()),
                external_author_id,
                external_author_display,
                feedback.external_created_at,
                feedback.external_edited_at,
                feedback.external_deleted_at
            ],
        )
        .map_err(internal)?;
        tx.execute(
            "INSERT INTO discord_inbound_message_state \
             (provider, external_message_id, org, artifact_id, feedback_id, \
              external_thread_id, external_author_id, external_author_display, provider_version, \
              external_created_at, external_edited_at, external_deleted_at) \
             VALUES ('discord', ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                feedback.external_message_id,
                feedback.org.0,
                feedback.artifact_id.0,
                feedback.id.0,
                thread_id,
                external_author_id,
                external_author_display,
                feedback.provider_version,
                feedback.external_created_at,
                feedback.external_edited_at,
                feedback.external_deleted_at
            ],
        )
        .map_err(internal)?;
    }
    Ok(())
}

fn mark_policy_degraded(
    tx: &Transaction<'_>,
    binding: &TwoWayThreadAuthorization,
    _reason: ThreadDegradedReason,
) -> Result<(), AppError> {
    tx.execute(
        "UPDATE artifact_discord_inbound_policies \
            SET health='degraded', safe_error='thread_unavailable', updated_at=datetime('now') \
          WHERE artifact_id=?1 AND org=?2",
        params![binding.artifact_id.0, binding.org.0],
    )
    .map_err(internal)?;
    tx.execute(
        "UPDATE artifact_discussions \
            SET state='failed', last_error='thread_unavailable', updated_at=datetime('now') \
          WHERE artifact_id=?1 AND org=?2 AND provider='discord' \
            AND mode='discord_mirror' AND state='connected'",
        params![binding.artifact_id.0, binding.org.0],
    )
    .map(|_| ())
    .map_err(internal)
}

fn record_event(
    tx: &Transaction<'_>,
    event: &InboundEvent,
    result: InboundResult,
) -> Result<(), AppError> {
    let (result_name, safe_error) = result_projection(result);
    let (event_type, version, message_id) = event_projection(event);
    tx.execute(
        "INSERT INTO discord_inbound_events \
         (provider, event_id, org, gateway_session_id, guild_id, thread_id, message_id, \
          event_type, provider_version, payload_sha256, processed_at, result, safe_error) \
         VALUES ('discord', ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, \
                 CASE WHEN ?10='needs_fetch' THEN NULL ELSE datetime('now') END, ?10, ?11) \
         ON CONFLICT(provider, org, gateway_session_id, event_id) DO UPDATE SET \
           guild_id=excluded.guild_id, thread_id=excluded.thread_id, \
           message_id=excluded.message_id, event_type=excluded.event_type, \
           provider_version=excluded.provider_version, payload_sha256=excluded.payload_sha256, \
           processed_at=excluded.processed_at, result=excluded.result, \
           safe_error=excluded.safe_error, next_attempt_at=NULL \
         WHERE discord_inbound_events.result='needs_fetch'",
        params![
            event.event_id,
            event.org.0,
            event.gateway_session_id,
            event.guild_id,
            event.thread_id,
            message_id,
            event_type,
            version,
            event.payload_fingerprint,
            result_name,
            safe_error
        ],
    )
    .map(|_| ())
    .map_err(internal)
}

fn event_projection(event: &InboundEvent) -> (&'static str, Option<i64>, Option<&str>) {
    let kind = match event.kind {
        InboundEventKind::MessageCreate => "message_create",
        InboundEventKind::MessageUpdate => "message_update",
        InboundEventKind::MessageDelete => "message_delete",
        InboundEventKind::ThreadUpdate { .. } => "thread_update",
        InboundEventKind::ThreadDelete => "thread_delete",
    };
    (
        kind,
        event.message.as_ref().map(|message| message.version),
        event.message.as_ref().map(|message| message.id.as_str()),
    )
}

fn result_projection(result: InboundResult) -> (&'static str, &'static str) {
    match result {
        InboundResult::Applied => ("applied", ""),
        InboundResult::Duplicate => ("duplicate", ""),
        InboundResult::NeedsFetch => ("needs_fetch", "gateway_unavailable"),
        InboundResult::Degraded(_) => ("degraded", "thread_unavailable"),
        InboundResult::Ignored(reason) => (
            "ignored",
            match reason {
                IgnoreReason::Disabled => "disabled",
                IgnoreReason::Unmapped => "unmapped",
                IgnoreReason::Bot => "bot",
                IgnoreReason::Webhook => "webhook",
                IgnoreReason::Unsupported => "unsupported",
                IgnoreReason::EmptyContent => "empty_content",
                IgnoreReason::Stale => "stale",
                IgnoreReason::UnknownMessage => "unknown_message",
            },
        ),
        InboundResult::Rejected(reason) => (
            "rejected",
            match reason {
                RejectReason::CrossTenant => "cross_tenant",
                RejectReason::WrongGuild => "wrong_guild",
                RejectReason::WrongThread => "wrong_thread",
                RejectReason::InvalidEvent => "invalid_event",
            },
        ),
    }
}

fn validate_event_envelope(event: &InboundEvent) -> Result<(), AppError> {
    for (label, value, max) in [
        ("event id", event.event_id.as_str(), MAX_PROVIDER_ID_BYTES),
        (
            "gateway session id",
            event.gateway_session_id.as_str(),
            MAX_SESSION_ID_BYTES,
        ),
        ("guild id", event.guild_id.as_str(), MAX_PROVIDER_ID_BYTES),
        ("thread id", event.thread_id.as_str(), MAX_PROVIDER_ID_BYTES),
    ] {
        if value.is_empty() || value.len() > max {
            return Err(AppError::Validation(format!("Discord {label} is invalid.")));
        }
    }
    if event.payload_fingerprint.len() != 64
        || !event
            .payload_fingerprint
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(AppError::Validation(
            "Discord event fingerprint is invalid.".to_owned(),
        ));
    }
    if let Some(message) = event.message.as_ref() {
        validate_message(message)?;
    }
    debug_assert!(MAX_SAFE_ERROR_BYTES >= "thread_unavailable".len());
    Ok(())
}

fn validate_message(message: &DiscordMessage) -> Result<(), AppError> {
    for value in [
        message.id.as_str(),
        message.guild_id.as_str(),
        message.thread_id.as_str(),
        message.author.id.as_str(),
    ] {
        if value.is_empty() || value.len() > MAX_PROVIDER_ID_BYTES {
            return Err(AppError::Validation(
                "Discord message identity is invalid.".to_owned(),
            ));
        }
    }
    if message.author.display.trim().is_empty() || message.author.display.len() > 160 {
        return Err(AppError::Validation(
            "Discord author display is invalid.".to_owned(),
        ));
    }
    Ok(())
}

struct NoFetchRest;

impl DiscordRestPort for NoFetchRest {
    fn fetch_message(
        &self,
        _guild_id: &str,
        _thread_id: &str,
        _message_id: &str,
    ) -> Result<Option<DiscordMessage>, RestError> {
        Err(RestError::Unavailable)
    }
}

fn internal(error: impl std::fmt::Display) -> AppError {
    tracing::error!(error = %error, "discord inbound persistence failed");
    AppError::Internal
}
