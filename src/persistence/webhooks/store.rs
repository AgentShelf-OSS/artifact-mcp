use super::sql_helpers::{
    discussion_anchor_exists, internal, org_exists, row_by_id, rows_for_org, trimmed,
};
use super::*;

// ---------------------------------------------------------------------------
// Store
// ---------------------------------------------------------------------------

/// Per-org Discord webhook persistence on U03's pool.
#[derive(Clone)]
pub struct WebhookStore {
    pool: DbPool,
    ids: Arc<dyn IdSource>,
    protection: Arc<WebhookUrlProtection>,
}

impl WebhookStore {
    /// Bind the store to a pool, an id factory, and the at-rest protection mode.
    #[must_use]
    pub const fn new(
        pool: DbPool,
        ids: Arc<dyn IdSource>,
        protection: Arc<WebhookUrlProtection>,
    ) -> Self {
        Self {
            pool,
            ids,
            protection,
        }
    }

    /// The configured at-rest protection, shared with the notifier.
    #[must_use]
    pub const fn protection(&self) -> &Arc<WebhookUrlProtection> {
        &self.protection
    }

    /// Port of `listForOrg(org)` — masked rows only. [lib/webhooks.js:71-73]
    ///
    /// # Errors
    /// Returns [`AppError::Unavailable`] or [`AppError::Internal`] on a database fault.
    pub async fn list_for_org(&self, org: &OrgId) -> Result<Vec<WebhookSummary>, AppError> {
        let org = trimmed(&org.0);
        let rows = db::interact(&self.pool, move |conn| rows_for_org(conn, &org)).await?;
        Ok(rows.iter().map(WebhookRow::public).collect())
    }

    /// Port of `forEvent(org, event)` — internal, returns **decrypted** URLs.
    /// [lib/webhooks.js:76-81]
    ///
    /// Never hand the result to a renderer or an HTTP response.
    ///
    /// # Errors
    /// Propagates a database fault, or [`AppError::Internal`] when an encrypted row cannot be
    /// revealed with the configured key.
    pub async fn for_event(
        &self,
        org: &OrgId,
        event: &WebhookEvent,
    ) -> Result<Vec<WebhookDelivery>, AppError> {
        let org = trimmed(&org.0);
        let rows = db::interact(&self.pool, move |conn| rows_for_org(conn, &org)).await?;
        rows.iter()
            .filter(|row| row.events.contains(event))
            .map(|row| row.delivery(&self.protection))
            .collect()
    }

    /// Port of `create({ org, url, label, events })` — [lib/webhooks.js:83-102].
    ///
    /// With a key the row stores the masked URL plus the three cipher columns; without one the URL
    /// is stored verbatim with NULL cipher columns. Both are handled by
    /// [`WebhookUrlProtection::protect`].
    ///
    /// # Errors
    /// Returns [`AppError::Validation`] for an unknown organization or a URL outside the Discord
    /// allowlist, propagating the Node message verbatim; otherwise a database or cipher fault.
    pub async fn create(&self, request: CreateWebhook) -> Result<WebhookSummary, AppError> {
        let org = trimmed(&request.org.0);
        let url = trimmed(&request.url);
        let url_is_allowed = is_discord_webhook_url(&url);
        let id = self.ids.webhook_id()?;
        let label = truncate_utf16(&trimmed(&request.label), MAX_LABEL_UTF16);
        let events = encode_events(&normalize_events(request.events.as_deref(), true));
        // Encrypting before the org check costs one AEAD call on a rejected request and keeps the
        // secret out of the blocking closure's captured state as a bare `String`.
        let stored = self.protection.protect(&url)?;

        db::interact(&self.pool, move |conn| {
            create_in(
                conn,
                &org,
                &id,
                &url,
                url_is_allowed,
                &label,
                &events,
                &stored,
            )
        })
        .await
    }

    pub async fn create_audited(
        &self,
        request: CreateWebhook,
        audit: MutationAudit,
        audit_key: [u8; 32],
    ) -> Result<WebhookSummary, AppError> {
        let org = trimmed(&request.org.0);
        let url = trimmed(&request.url);
        let label = truncate_utf16(&trimmed(&request.label), MAX_LABEL_UTF16);
        let events = encode_events(&normalize_events(request.events.as_deref(), true));
        let url_is_allowed = is_discord_webhook_url(&url);
        let id = self.ids.webhook_id()?;
        let stored = self.protection.protect(&url)?;
        let target = org.clone();
        db::interact(&self.pool, move |conn| {
            let audit = audit.for_target_tenant(&org)?;
            mutate_in_transaction(conn, &audit_key, &audit, |tx| {
                let row = create_in(
                    tx,
                    &org,
                    &id,
                    &url,
                    url_is_allowed,
                    &label,
                    &events,
                    &stored,
                )?;
                Ok((
                    row,
                    AuditEvent {
                        operation: "webhook.config.create".to_owned(),
                        target_type: "organization".to_owned(),
                        target_id: target,
                        result: "success".to_owned(),
                        classification: "webhook_config_created".to_owned(),
                        revision: None,
                    },
                ))
            })
        })
        .await
    }

    pub async fn remove_audited(
        &self,
        org: OrgId,
        id: WebhookId,
        audit: MutationAudit,
        audit_key: [u8; 32],
    ) -> Result<bool, AppError> {
        let org = trimmed(&org.0);
        let id = trimmed(&id.0);
        let target = org.clone();
        db::interact(&self.pool, move |conn| {
            let audit = audit.for_target_tenant(&org)?;
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(|_| AppError::Internal)?;
            if discussion_anchor_exists(&tx, &org, &id)? {
                return Err(AppError::Conflict(
                    "Remove the Discord notification-thread connection before deleting this webhook."
                        .to_owned(),
                ));
            }
            let changed = tx
                .execute(
                    "DELETE FROM org_webhooks WHERE org = ?1 AND id = ?2",
                    (&org, &id),
                )
                .map_err(internal)?
                > 0;
            if !changed {
                tx.commit().map_err(|_| AppError::Internal)?;
                return Ok(false);
            }
            let event = AuditEvent {
                operation: "webhook.config.remove".to_owned(),
                target_type: "organization".to_owned(),
                target_id: target,
                result: "success".to_owned(),
                classification: "webhook_config_removed".to_owned(),
                revision: None,
            };
            append_in_transaction(&tx, &audit_key, &audit.event_id()?, audit.context(), &event)?;
            tx.commit().map_err(|_| AppError::Internal)?;
            Ok(true)
        })
        .await
    }

    pub async fn set_events_audited(
        &self,
        org: OrgId,
        id: WebhookId,
        events: Vec<WebhookEvent>,
        audit: MutationAudit,
        audit_key: [u8; 32],
    ) -> Result<Option<WebhookSummary>, AppError> {
        let normalized = encode_events(&normalize_events(Some(&events), false));
        let org = trimmed(&org.0);
        let id = trimmed(&id.0);
        let target = org.clone();
        db::interact(&self.pool, move |conn| {
            let audit = audit.for_target_tenant(&org)?;
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(|_| AppError::Internal)?;
            let Some(current) = row_by_id(&tx, &id)?.filter(|row| row.org.0 == org) else {
                tx.commit().map_err(|_| AppError::Internal)?;
                return Ok(None);
            };
            if !normalized.split(',').any(|event| event == "published")
                && discussion_anchor_exists(&tx, &org, &id)?
            {
                return Err(AppError::Conflict(
                    "Published notifications are required by the Discord thread connection."
                        .to_owned(),
                ));
            }
            if encode_events(&current.events) == normalized {
                let row = current.public();
                tx.commit().map_err(|_| AppError::Internal)?;
                return Ok(Some(row));
            }
            tx.execute(
                "UPDATE org_webhooks SET events = ?1 WHERE org = ?2 AND id = ?3",
                (&normalized, &org, &id),
            )
            .map_err(internal)?;
            let row = row_by_id(&tx, &id)?
                .map(|row| row.public())
                .ok_or(AppError::Internal)?;
            let event = AuditEvent {
                operation: "webhook.config.update".to_owned(),
                target_type: "organization".to_owned(),
                target_id: target,
                result: "success".to_owned(),
                classification: "webhook_config_updated".to_owned(),
                revision: None,
            };
            append_in_transaction(&tx, &audit_key, &audit.event_id()?, audit.context(), &event)?;
            tx.commit().map_err(|_| AppError::Internal)?;
            Ok(Some(row))
        })
        .await
    }

    /// Persist one half of the explicitly non-atomic webhook-test protocol. `None` records the
    /// durable pre-I/O request marker; `Some` records the terminal external delivery result.
    /// The persisted target is only `(tenant org, webhook id)`; the decrypted URL never crosses
    /// this boundary. A crash after delivery can therefore leave a visible requested-only record
    /// rather than silently losing evidence of the attempt.
    pub async fn audit_test(
        &self,
        org: OrgId,
        id: WebhookId,
        outcome: Option<bool>,
        audit: MutationAudit,
        audit_key: [u8; 32],
    ) -> Result<(), AppError> {
        let org = trimmed(&org.0);
        let id = trimmed(&id.0);
        db::interact(&self.pool, move |conn| {
            let audit = audit.for_target_tenant(&org)?;
            let event = match outcome {
                None => AuditEvent {
                    operation: "webhook.test.requested".to_owned(),
                    target_type: "webhook".to_owned(),
                    target_id: id,
                    result: "success".to_owned(),
                    classification: "external_delivery_requested".to_owned(),
                    revision: None,
                },
                Some(ok) => AuditEvent {
                    operation: "webhook.test.completed".to_owned(),
                    target_type: "webhook".to_owned(),
                    target_id: id,
                    result: if ok { "success" } else { "failure" }.to_owned(),
                    classification: if ok {
                        "external_delivery_succeeded"
                    } else {
                        "external_delivery_failed"
                    }
                    .to_owned(),
                    revision: None,
                },
            };
            mutate_in_transaction(conn, &audit_key, &audit, |_tx| Ok(((), event)))
        })
        .await
    }

    /// `remove` remains the non-audited persistence primitive used by legacy/unit callers.
    /// Production settings mutations use [`Self::remove_audited`].
    pub async fn remove(&self, org: &OrgId, id: &WebhookId) -> Result<bool, AppError> {
        let org = trimmed(&org.0);
        let id = trimmed(&id.0);
        db::interact(&self.pool, move |conn| {
            if discussion_anchor_exists(conn, &org, &id)? {
                return Err(AppError::Conflict(
                    "Remove the Discord notification-thread connection before deleting this webhook."
                        .to_owned(),
                ));
            }
            let changed = conn
                .execute(
                    "DELETE FROM org_webhooks WHERE org = ?1 AND id = ?2",
                    (&org, &id),
                )
                .map_err(internal)?;
            Ok(changed > 0)
        })
        .await
    }
}

#[allow(clippy::too_many_arguments)]
fn create_in(
    conn: &Connection,
    org: &str,
    id: &WebhookId,
    _url: &str,
    url_is_allowed: bool,
    label: &str,
    events: &str,
    stored: &StoredWebhookUrl,
) -> Result<WebhookSummary, AppError> {
    // Node's order is unknown-organization first, then invalid-URL. [lib/webhooks.js:86-87]
    if !org_exists(conn, org)? {
        return Err(AppError::Validation(format!(
            "Unknown organization \"{org}\"."
        )));
    }
    if !url_is_allowed {
        return Err(AppError::Validation(INVALID_URL_MESSAGE.to_owned()));
    }
    conn.execute(
        "INSERT INTO org_webhooks \
                 (id, org, url, url_cipher, url_nonce, url_tag, label, events) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        (
            &id.0,
            &org,
            &stored.url,
            stored.encrypted.as_ref().map(|record| &record.ciphertext),
            stored.encrypted.as_ref().map(|record| &record.nonce),
            stored.encrypted.as_ref().map(|record| &record.tag),
            &label,
            &events,
        ),
    )
    .map_err(internal)?;
    // Node re-reads the row so database defaults (`created_at`) are authoritative.
    // [lib/webhooks.js:101]
    row_by_id(conn, &id.0)?
        .map(|row| row.public())
        .ok_or(AppError::Internal)
}

impl WebhookStore {
    /// Port of `setEvents(org, id, events)` — [lib/webhooks.js:108-114].
    ///
    /// `None` means no row matched; the parsed set defaults to **empty**, not to every event.
    ///
    /// # Errors
    /// Returns a database fault.
    pub async fn set_events(
        &self,
        org: &OrgId,
        id: &WebhookId,
        events: &[WebhookEvent],
    ) -> Result<Option<WebhookSummary>, AppError> {
        let normalized = encode_events(&normalize_events(Some(events), false));
        let org = trimmed(&org.0);
        let id = trimmed(&id.0);
        db::interact(&self.pool, move |conn| {
            if !normalized.split(',').any(|event| event == "published")
                && discussion_anchor_exists(conn, &org, &id)?
            {
                return Err(AppError::Conflict(
                    "Published notifications are required by the Discord thread connection."
                        .to_owned(),
                ));
            }
            let changed = conn
                .execute(
                    "UPDATE org_webhooks SET events = ?1 WHERE org = ?2 AND id = ?3",
                    (&normalized, &org, &id),
                )
                .map_err(internal)?;
            if changed == 0 {
                return Ok(None);
            }
            Ok(row_by_id(conn, &id)?.map(|row| row.public()))
        })
        .await
    }

    /// Port of `get(id)` — internal, returns a **decrypted** URL. [lib/webhooks.js:117-119]
    ///
    /// # Errors
    /// Propagates a database fault, or [`AppError::Internal`] when an encrypted row cannot be
    /// revealed with the configured key.
    pub async fn delivery(&self, id: &WebhookId) -> Result<Option<WebhookDelivery>, AppError> {
        let id = trimmed(&id.0);
        let row = db::interact(&self.pool, move |conn| row_by_id(conn, &id)).await?;
        row.map(|row| row.delivery(&self.protection)).transpose()
    }

    /// Resolve one worker target while binding it to the durable record's tenant.
    ///
    /// Unlike [`Self::delivery`], this is intentionally typed and redacted for the background
    /// delivery path: transient database faults retry, while missing/cross-tenant references and
    /// ciphertext/key failures terminally fail without letting the worker call a provider.
    /// The long-standing [`Self::delivery`] API remains for the awaited admin webhook test path.
    pub async fn resolve_delivery(
        &self,
        id: &WebhookId,
        expected_org: &OrgId,
    ) -> Result<WebhookDelivery, WebhookDeliveryResolutionFailure> {
        let id = trimmed(&id.0);
        let expected_org = trimmed(&expected_org.0);
        if id.is_empty() || expected_org.is_empty() {
            return Err(WebhookDeliveryResolutionFailure::InvalidReference);
        }
        let row = db::interact(&self.pool, move |conn| row_by_id(conn, &id))
            .await
            .map_err(|_| WebhookDeliveryResolutionFailure::Retryable)?;
        let Some(row) = row else {
            return Err(WebhookDeliveryResolutionFailure::InvalidReference);
        };
        if row.org.0 != expected_org {
            return Err(WebhookDeliveryResolutionFailure::InvalidReference);
        }
        row.delivery(&self.protection)
            .map_err(|_| WebhookDeliveryResolutionFailure::DecryptFailed)
    }

    /// The masked view of one webhook, for callers that must not see the URL at all.
    ///
    /// # Errors
    /// Returns a database fault.
    pub async fn summary(&self, id: &WebhookId) -> Result<Option<WebhookSummary>, AppError> {
        let id = trimmed(&id.0);
        let row = db::interact(&self.pool, move |conn| row_by_id(conn, &id)).await?;
        Ok(row.map(|row| row.public()))
    }

    /// Port of `recordResult(id, ok, error)` — [lib/webhooks.js:126-129].
    ///
    /// Success clears `last_error` and stamps `last_ok_at`; failure records the message truncated
    /// to 500 UTF-16 units and leaves `last_ok_at` untouched.
    ///
    /// # Errors
    /// Returns a database fault. Callers on the delivery path deliberately ignore it: a webhook
    /// bookkeeping failure must not fail the triggering publish.
    pub async fn record_result(
        &self,
        id: &WebhookId,
        outcome: Result<(), String>,
    ) -> Result<(), AppError> {
        let id = trimmed(&id.0);
        db::interact(&self.pool, move |conn| {
            match outcome {
                Ok(()) => conn.execute(
                    "UPDATE org_webhooks SET last_ok_at = datetime('now'), last_error = NULL \
                     WHERE id = ?1",
                    (&id,),
                ),
                Err(message) => {
                    let message = if message.is_empty() {
                        DEFAULT_DELIVERY_ERROR.to_owned()
                    } else {
                        message
                    };
                    conn.execute(
                        "UPDATE org_webhooks SET last_error = ?1 WHERE id = ?2",
                        (truncate_utf16(&message, MAX_ERROR_UTF16), &id),
                    )
                }
            }
            .map_err(internal)?;
            Ok(())
        })
        .await
    }
}

/// Redacted by construction: the store owns the cipher and the pool, neither of which is
/// printable.
impl std::fmt::Debug for WebhookStore {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("WebhookStore")
            .field("protection", self.protection.as_ref())
            .finish_non_exhaustive()
    }
}
