use crate::error::AppError;
use rusqlite::{Connection, Transaction, TransactionBehavior, params};

use super::{DeadLetterTransition, DeliveryRecord, MAX_ATTEMPTS, RetryTransition, get, internal};

pub fn accepted(
    conn: &Connection,
    id: &str,
    worker: &str,
    token: &str,
    lease_version: i64,
    message: &str,
    now: i64,
) -> Result<bool, AppError> {
    accepted_guarded(conn, id, worker, token, lease_version, message, now)
}

/// Guarded acceptance for a caller which already owns a larger transaction (for example,
/// accepting a discussion delivery while recording its Discord message IDs).
pub fn accepted_in_transaction(
    tx: &Transaction<'_>,
    id: &str,
    worker: &str,
    token: &str,
    lease_version: i64,
    message: &str,
    now: i64,
) -> Result<bool, AppError> {
    accepted_guarded(tx, id, worker, token, lease_version, message, now)
}

fn accepted_guarded(
    conn: &Connection,
    id: &str,
    worker: &str,
    token: &str,
    lease_version: i64,
    message: &str,
    now: i64,
) -> Result<bool, AppError> {
    transition(
        conn,
        id,
        worker,
        token,
        lease_version,
        Transition {
            state: "accepted",
            now,
            next_attempt_at: now,
            classification: "accepted",
            message,
            error: "",
            duplicate_risk: false,
            requires_exhaustion: false,
        },
    )
}
pub fn retry(
    conn: &Connection,
    id: &str,
    worker: &str,
    token: &str,
    lease_version: i64,
    update: RetryTransition,
    now: i64,
) -> Result<bool, AppError> {
    let classification = safe_retry_classification(&update.classification)?;
    Ok(conn
        .execute(
            "UPDATE provider_delivery_outbox
         SET state = CASE WHEN attempts >= ?1 THEN 'dead_letter' ELSE 'retry' END,
             lease_owner = NULL,
             lease_expires_at = NULL,
             lease_token = NULL,
             next_attempt_at = CASE WHEN attempts >= ?1 THEN ?2 ELSE ?3 END,
             result_classification = CASE WHEN attempts >= ?1 THEN 'attempts_exhausted' ELSE ?4 END,
             duplicate_risk = ?5,
             terminal_error = CASE WHEN attempts >= ?1 THEN 'attempts_exhausted' ELSE '' END,
             updated_at = ?2,
             completed_at = CASE WHEN attempts >= ?1 THEN ?2 ELSE NULL END
         WHERE id = ?6
           AND state = 'leased'
           AND lease_owner = ?7
           AND lease_token = ?8
           AND lease_version = ?9",
            params![
                MAX_ATTEMPTS,
                now,
                update.next_attempt_at,
                classification,
                i64::from(update.duplicate_risk),
                id,
                worker,
                token,
                lease_version
            ],
        )
        .map_err(internal)?
        == 1)
}
/// Persists global, target, or effective bucket throttling and bucket discovery atomically.
pub fn persist_rate_limit(
    conn: &mut Connection,
    scope: &str,
    target: &str,
    bucket: &str,
    secret: &str,
    blocked_until: i64,
    now: i64,
) -> Result<(), AppError> {
    let (stored_target, stored_bucket, stored_secret) = match scope {
        "global" if target.is_empty() && bucket.is_empty() && secret.is_empty() => ("", "", ""),
        "target" if opaque_target_key(target) && bucket.is_empty() && secret.is_empty() => {
            (target, "", "")
        }
        "bucket"
            if opaque_target_key(target)
                && opaque_bucket_key(bucket)
                && opaque_provider_secret_ref(secret) =>
        {
            ("", bucket, secret)
        }
        _ => return Err(AppError::Validation("invalid rate limit state".into())),
    };
    if blocked_until < 0 {
        return Err(AppError::Validation("invalid rate limit state".into()));
    }
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(internal)?;
    tx.execute("INSERT INTO provider_delivery_rate_limits (provider,scope,target_key,bucket_id,top_level_secret_ref,blocked_until,updated_at) VALUES ('discord',?1,?2,?3,?4,?5,?6) ON CONFLICT(provider,scope,target_key,bucket_id,top_level_secret_ref) DO UPDATE SET blocked_until=MAX(provider_delivery_rate_limits.blocked_until,excluded.blocked_until),updated_at=CASE WHEN excluded.blocked_until >= provider_delivery_rate_limits.blocked_until THEN excluded.updated_at ELSE provider_delivery_rate_limits.updated_at END", params![scope,stored_target,stored_bucket,stored_secret,blocked_until,now]).map_err(internal)?;
    if scope == "bucket" {
        tx.execute("UPDATE provider_delivery_outbox SET bucket_id=?1,updated_at=?2 WHERE target_key=?3 AND secret_ref=?4 AND state IN ('blocked','ready','retry')", params![bucket,now,target,secret]).map_err(internal)?;
    }
    tx.commit().map_err(internal)?;
    Ok(())
}
pub fn dead_letter(
    conn: &Connection,
    id: &str,
    worker: &str,
    token: &str,
    lease_version: i64,
    update: DeadLetterTransition,
    now: i64,
) -> Result<bool, AppError> {
    dead_letter_guarded(conn, id, worker, token, lease_version, update, now)
}

/// Guarded terminal transition for a caller which must update discussion mappings atomically.
pub fn dead_letter_in_transaction(
    tx: &Transaction<'_>,
    id: &str,
    worker: &str,
    token: &str,
    lease_version: i64,
    update: DeadLetterTransition,
    now: i64,
) -> Result<bool, AppError> {
    dead_letter_guarded(tx, id, worker, token, lease_version, update, now)
}

fn dead_letter_guarded(
    conn: &Connection,
    id: &str,
    worker: &str,
    token: &str,
    lease_version: i64,
    update: DeadLetterTransition,
    now: i64,
) -> Result<bool, AppError> {
    if update.duplicate_risk && update.classification != "attempts_exhausted" {
        return Err(AppError::Validation(
            "duplicate risk is valid only for exhausted delivery".into(),
        ));
    }
    transition(
        conn,
        id,
        worker,
        token,
        lease_version,
        Transition {
            state: "dead_letter",
            now,
            next_attempt_at: now,
            classification: &update.classification,
            message: "",
            error: &update.error,
            duplicate_risk: update.duplicate_risk,
            requires_exhaustion: update.classification.starts_with("attempts_exhausted"),
        },
    )
}
struct Transition<'a> {
    state: &'a str,
    now: i64,
    next_attempt_at: i64,
    classification: &'a str,
    message: &'a str,
    error: &'a str,
    duplicate_risk: bool,
    requires_exhaustion: bool,
}
fn transition(
    conn: &Connection,
    id: &str,
    worker: &str,
    token: &str,
    lease_version: i64,
    update: Transition<'_>,
) -> Result<bool, AppError> {
    let classification = match update.state {
        "accepted" if update.classification == "accepted" => update.classification,
        "dead_letter" => safe_terminal_classification(update.classification)?,
        _ => {
            return Err(AppError::Validation(
                "invalid outbox result classification".into(),
            ));
        }
    };
    Ok(conn.execute("UPDATE provider_delivery_outbox SET state = ?1, lease_owner = NULL, lease_expires_at = NULL, lease_token = NULL, next_attempt_at = ?2, result_classification = ?3, duplicate_risk = ?4, discord_message_id = CASE WHEN ?1 = 'accepted' THEN ?5 ELSE discord_message_id END, terminal_error = ?6, updated_at = ?7, completed_at = CASE WHEN ?1 IN ('accepted', 'dead_letter') THEN ?7 ELSE NULL END WHERE id = ?8 AND state = 'leased' AND lease_owner = ?9 AND lease_token = ?10 AND lease_version = ?11 AND (?12 = 0 OR attempts >= ?13)", params![update.state, update.next_attempt_at, classification, i64::from(update.duplicate_risk), update.message, redact_error(update.error), update.now, id, worker, token, lease_version, i64::from(update.requires_exhaustion), MAX_ATTEMPTS]).map_err(internal)? == 1)
}

fn redact_error(error: &str) -> String {
    // Provider response bodies are untrusted and can contain webhook credentials.  Persist only
    // a closed set of worker-produced codes; all transport text, including token-like strings,
    // becomes the same safe operator-facing fallback.
    match error {
        "invalid_webhook"
        | "unknown_webhook"
        | "invalid_secret"
        | "decrypt_failed"
        | "allowlist_rejected"
        | "redirect"
        | "bad_request"
        | "unauthorized"
        | "forbidden"
        | "not_found"
        | "invalid_rate_limit_delay"
        | "client_error"
        | "contract_error"
        | "payload_hash_mismatch"
        | "permission_denied"
        | "validation_failed"
        | "provider_unavailable"
        | "attempts_exhausted"
        | "dependency_failed" => error.to_owned(),
        _ => "provider delivery failed".into(),
    }
}
fn safe_retry_classification(value: &str) -> Result<&str, AppError> {
    if matches!(
        value,
        "retry"
            | "network_retry"
            | "ambiguous_worker_restart"
            | "provider_unavailable"
            | "rate_limited"
            | "network"
            | "timeout"
            | "ambiguous"
            | "server_error"
    ) {
        Ok(value)
    } else {
        Err(AppError::Validation(
            "invalid outbox result classification".into(),
        ))
    }
}
fn safe_terminal_classification(value: &str) -> Result<&str, AppError> {
    if matches!(
        value,
        "attempts_exhausted"
            | "attempts_exhausted_after_worker_restart"
            | "invalid_webhook"
            | "permission_denied"
            | "validation_failed"
            | "provider_unavailable"
            | "dead_letter"
            | "server_error"
            | "invalid_secret"
            | "decrypt_failed"
            | "allowlist_rejected"
            | "redirect"
            | "bad_request"
            | "unauthorized"
            | "forbidden"
            | "not_found"
            | "invalid_rate_limit_delay"
            | "client_error"
            | "contract_error"
            | "payload_hash_mismatch"
            | "unknown_webhook"
            | "dependency_failed"
    ) {
        Ok(value)
    } else {
        Err(AppError::Validation(
            "invalid outbox result classification".into(),
        ))
    }
}
